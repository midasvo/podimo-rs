//! Progressive audio for HLS episodes (see `podimo::hls`):
//!
//! - `GET /stream/<token>/<episode_id>.m4a|.mp3` — what feed enclosures point
//!   at. `token` is the signed Podimo `.m3u8` URL in base64url, so the URL
//!   itself ends in a plain file extension. Both formats are always served;
//!   `STREAM_FORMAT` only decides which one the feed links to.
//!   - `.m4a`: the AAC repackaged into a complete MP4. It's assembled before
//!     the first byte goes out and then kept for a while (`episode_files`),
//!     so responses have a length and honour `Range`: players can seek.
//!   - `.mp3`: re-encoded by ffmpeg and streamed as it's produced.
//! - `GET /stream/<episode_id>.aac?src=<signed .m3u8>` — the earlier AAC remux,
//!   kept so enclosure URLs that podcatchers already stored keep working.
//!
//! No auth: the signed playlist URL is itself the credential, and only
//! Podimo-hosted playlists are accepted. Anyone who has one enclosure URL can
//! open it many times, so `podimo::hls` caps how many streams run at once;
//! when every slot is taken, a request gets a 503 with `Retry-After`.
//!
//! The block list is matched against the episode id and the playlist URL.
//! The link carries no podcast id, so an episode link that's already out
//! there is stopped by listing the episode's id.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures::Stream;
use reqwest::Url;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::episode_files::EpisodeFile;
use crate::error::AppError;
use crate::podimo::hls::{self, HlsError, StreamFormat};
use crate::state::AppState;
use crate::util::PODCAST_ID_RE;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/stream/{file}", get(serve_aac))
        .route("/stream/{token}/{file}", get(serve_episode))
}

async fn serve_episode(
    State(state): State<AppState>,
    Path((token, file)): Path<(String, String)>,
    request: Request,
) -> Response {
    let Some((episode_id, format)) = file
        .rsplit_once('.')
        .and_then(|(id, ext)| Some((id, StreamFormat::from_extension(ext)?)))
    else {
        return (StatusCode::NOT_FOUND, "404 Not found.").into_response();
    };
    if let Err(resp) = check_episode_id(episode_id) {
        return resp;
    }
    let src = match hls::decode_source_token(&token) {
        Ok(src) => src,
        Err(err) => return AppError::BadRequest(err.to_string()).into_response(),
    };
    // The playlist URL is base64 in the path, so check it decoded as well.
    if state.blocklist.contains_substring(request.uri().path())
        || state.blocklist.contains_substring(&src)
    {
        return AppError::Gone.into_response();
    }
    match format {
        StreamFormat::M4a => serve_m4a(&state, episode_id, &src, request).await,
        StreamFormat::Mp3 => stream_mp3(&state, episode_id, &src, request.method()).await,
    }
}

async fn serve_m4a(state: &AppState, episode_id: &str, src: &str, request: Request) -> Response {
    let src = match hls::validate_source(src) {
        Ok(src) => src,
        Err(err) => return AppError::BadRequest(err.to_string()).into_response(),
    };
    let key = format!("{}.m4a", hls::source_key(&src));
    let file = match state.episode_files.get(&key) {
        Some(file) => file,
        // Not worth fetching a whole episode for: check that it would start
        // and leave out the length, as for MP3.
        None if request.method() == Method::HEAD => {
            return match resolve(state, episode_id, src.as_str()).await {
                Ok(_) => {
                    let format = StreamFormat::M4a;
                    (
                        [
                            (header::CONTENT_TYPE, format.content_type().to_string()),
                            (
                                header::CONTENT_DISPOSITION,
                                content_disposition(episode_id, format.extension()),
                            ),
                        ],
                        Body::empty(),
                    )
                        .into_response()
                }
                Err(resp) => resp,
            };
        }
        None => match prepare_m4a(state, episode_id, src, &key).await {
            Ok(file) => file,
            Err(resp) => return resp,
        },
    };
    serve_file(&file, episode_id, request).await
}

/// Assemble the episode behind `src` under `key`, or wait for the request
/// that's already doing so.
#[allow(clippy::result_large_err)]
async fn prepare_m4a(
    state: &AppState,
    episode_id: &str,
    src: Url,
    key: &str,
) -> Result<Arc<EpisodeFile>, Response> {
    let client = state.scraper.clone();
    let id = episode_id.to_string();
    let prepare = move |dest: std::path::PathBuf| async move {
        let started = Instant::now();
        let segments = hls::resolve_segments(&client, &src).await?;
        let count = segments.len();
        let aac = hls::aac_stream(client, segments, hls::PREPARE_PREFETCH);
        hls::remux_m4a(aac, &dest).await?;
        tracing::info!(
            target: "podimo::stream",
            "stream {id}: {count} segments to m4a in {:.1}s",
            started.elapsed().as_secs_f64()
        );
        Ok(())
    };
    state
        .episode_files
        .get_or_prepare(key, prepare)
        .await
        .map_err(|err| match *err {
            HlsError::Busy => start_failed(episode_id, HlsError::Busy),
            HlsError::Upstream(_) | HlsError::Unsupported(_) => {
                tracing::warn!(target: "podimo", "stream {episode_id}: {err}");
                AppError::UpstreamUnavailable(err.to_string()).into_response()
            }
            _ => AppError::Internal(format!("stream {episode_id}: {err}")).into_response(),
        })
}

/// `file` with the length, `Range` and conditional request handling that
/// lets players seek.
async fn serve_file(file: &EpisodeFile, episode_id: &str, request: Request) -> Response {
    let format = StreamFormat::M4a;
    let mime = format
        .content_type()
        .parse()
        .expect("content types are valid MIME types");
    let Ok(response) = ServeFile::new_with_mime(&file.path, &mime)
        .oneshot(request)
        .await;
    let mut response = response.map(Body::new);
    if let Ok(value) = HeaderValue::from_str(&content_disposition(episode_id, format.extension())) {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

async fn stream_mp3(state: &AppState, episode_id: &str, src: &str, method: &Method) -> Response {
    let segments = match resolve(state, episode_id, src).await {
        Ok(segments) => segments,
        Err(resp) => return resp,
    };
    // HEAD only needs to know the stream would start: don't spawn ffmpeg (or
    // take a stream slot) for a body axum throws away anyway.
    let format = StreamFormat::Mp3;
    if method == Method::HEAD {
        return (
            [
                (header::CONTENT_TYPE, format.content_type().to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("inline; filename=\"{episode_id}.{}\"", format.extension()),
                ),
            ],
            Body::empty(),
        )
            .into_response();
    }
    let aac = hls::aac_stream(state.scraper.clone(), segments, hls::SEGMENT_PREFETCH);
    match hls::transcode_mp3(aac).await {
        Ok(body) => audio_response(episode_id, format.extension(), format.content_type(), body),
        Err(err) => start_failed(episode_id, err),
    }
}

async fn serve_aac(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    Path(file): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(episode_id) = file.strip_suffix(".aac") else {
        return (StatusCode::NOT_FOUND, "404 Not found.").into_response();
    };
    if let Err(resp) = check_episode_id(episode_id) {
        return resp;
    }
    let Some(src) = params.get("src") else {
        return AppError::BadRequest("Missing src".into()).into_response();
    };
    // The playlist URL is the `src` query parameter here.
    let path_and_query = uri.path_and_query().map_or("", |p| p.as_str());
    if state.blocklist.contains_substring(path_and_query) {
        return AppError::Gone.into_response();
    }
    let segments = match resolve(&state, episode_id, src).await {
        Ok(segments) => segments,
        Err(resp) => return resp,
    };
    // As above: a HEAD response has no body to hold a stream slot for.
    if method == Method::HEAD {
        return (
            [
                (header::CONTENT_TYPE, hls::AAC_CONTENT_TYPE.to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("inline; filename=\"{episode_id}.aac\""),
                ),
            ],
            Body::empty(),
        )
            .into_response();
    }
    let aac = hls::aac_stream(state.scraper.clone(), segments, hls::SEGMENT_PREFETCH);
    match hls::with_stream_slot(aac).await {
        Ok(body) => audio_response(episode_id, "aac", hls::AAC_CONTENT_TYPE, body),
        Err(err) => start_failed(episode_id, err),
    }
}

/// The response when a stream can't start: 503 with `Retry-After` while
/// every stream slot is taken, 500 otherwise.
fn start_failed(episode_id: &str, err: HlsError) -> Response {
    match err {
        HlsError::Busy => {
            tracing::warn!(target: "podimo", "stream {episode_id}: {err}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "30")],
                "Too many downloads at once, please retry",
            )
                .into_response()
        }
        err => AppError::Internal(format!("stream {episode_id}: {err}")).into_response(),
    }
}

// The `Response` errors below go straight back to axum, same as `library_or_404`.

#[allow(clippy::result_large_err)]
fn check_episode_id(id: &str) -> Result<(), Response> {
    if PODCAST_ID_RE.is_match(id) {
        Ok(())
    } else {
        Err(AppError::BadRequest("Invalid episode id".into()).into_response())
    }
}

/// Vet `src` and fetch its playlists down to the segment list.
#[allow(clippy::result_large_err)]
async fn resolve(state: &AppState, episode_id: &str, src: &str) -> Result<Vec<Url>, Response> {
    let src = hls::validate_source(src)
        .map_err(|err| AppError::BadRequest(err.to_string()).into_response())?
        .clone();
    let segments = hls::resolve_segments(&state.scraper, &src)
        .await
        .map_err(|err| {
            tracing::warn!(target: "podimo", "stream {episode_id}: {err}");
            AppError::UpstreamUnavailable(err.to_string()).into_response()
        })?;
    tracing::info!(target: "podimo::stream", "stream {episode_id}: resolved {} segments", segments.len());
    Ok(segments)
}

fn content_disposition(episode_id: &str, ext: &str) -> String {
    format!("inline; filename=\"{episode_id}.{ext}\"")
}

struct TrackedStream {
    inner: Pin<Box<dyn Stream<Item = Result<axum::body::Bytes, HlsError>> + Send>>,
    id: String,
    ext: String,
    bytes: u64,
    start: Instant,
    completed: bool,
}

impl TrackedStream {
    fn new<S>(stream: S, id: String, ext: String) -> Self
    where
        S: Stream<Item = Result<axum::body::Bytes, HlsError>> + Send + 'static,
    {
        Self {
            inner: Box::pin(stream),
            id,
            ext,
            bytes: 0,
            start: Instant::now(),
            completed: false,
        }
    }
}

impl Drop for TrackedStream {
    fn drop(&mut self) {
        if !self.completed && self.bytes > 0 {
            let duration = self.start.elapsed();
            let mb = self.bytes as f64 / (1024.0 * 1024.0);
            tracing::info!(
                target: "podimo::stream",
                "stream {}.{} ended by client after {:.2} MB ({duration:.1?})",
                self.id,
                self.ext,
                mb,
            );
        }
    }
}

impl Stream for TrackedStream {
    type Item = Result<axum::body::Bytes, HlsError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                this.bytes += bytes.len() as u64;
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(err))) => {
                tracing::warn!(target: "podimo::stream", "stream {}.{} aborted: {err}", this.id, this.ext);
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                this.completed = true;
                let duration = this.start.elapsed();
                let mb = this.bytes as f64 / (1024.0 * 1024.0);
                tracing::info!(
                    target: "podimo::stream",
                    "finished streaming {}.{} ({:.2} MB in {duration:.1?})",
                    this.id,
                    this.ext,
                    mb,
                );
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn audio_response<S>(episode_id: &str, ext: &str, content_type: &str, body: S) -> Response
where
    S: Stream<Item = Result<axum::body::Bytes, HlsError>> + Send + 'static,
{
    tracing::info!(target: "podimo::stream", "started streaming {episode_id}.{ext}");
    let tracked = TrackedStream::new(body, episode_id.to_string(), ext.to_string());
    (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (
                header::CONTENT_DISPOSITION,
                content_disposition(episode_id, ext),
            ),
        ],
        Body::from_stream(tracked),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPISODE: &str = "71672e78-7a1a-4725-9f89-c3def125e22f";

    async fn serve(file: &EpisodeFile, range: Option<&str>) -> (Response, Vec<u8>) {
        let mut request = Request::builder().uri(format!("/stream/token/{EPISODE}.m4a"));
        if let Some(range) = range {
            request = request.header(header::RANGE, range);
        }
        let response = serve_file(file, EPISODE, request.body(Body::empty()).unwrap()).await;
        let (parts, body) = response.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        (Response::from_parts(parts, Body::empty()), body.to_vec())
    }

    fn header_value(response: &Response, name: header::HeaderName) -> &str {
        response.headers()[name].to_str().unwrap()
    }

    #[tokio::test]
    async fn m4a_files_have_a_length_and_honour_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("episode.m4a");
        std::fs::write(&path, b"0123456789").unwrap();
        let file = EpisodeFile { path, size: 10 };

        let (response, body) = serve(&file, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header_value(&response, header::CONTENT_LENGTH), "10");
        assert_eq!(header_value(&response, header::ACCEPT_RANGES), "bytes");
        assert_eq!(header_value(&response, header::CONTENT_TYPE), "audio/x-m4a");
        assert_eq!(
            header_value(&response, header::CONTENT_DISPOSITION),
            format!("inline; filename=\"{EPISODE}.m4a\"")
        );
        assert_eq!(body, b"0123456789");

        // What a player sends to seek or resume.
        let (response, body) = serve(&file, Some("bytes=4-")).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            header_value(&response, header::CONTENT_RANGE),
            "bytes 4-9/10"
        );
        assert_eq!(body, b"456789");
    }
}
