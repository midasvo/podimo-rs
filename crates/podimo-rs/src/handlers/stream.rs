//! Progressive audio for HLS episodes (see `podimo::hls`):
//!
//! - `GET /stream/<token>/<episode_id>.mp3|.m4a` — what feed enclosures point
//!   at: MP3 re-encoded by ffmpeg, or the AAC repackaged as M4A. `token` is
//!   the signed Podimo `.m3u8` URL in base64url, so the URL itself ends in a
//!   plain file extension. Both formats are always served; `STREAM_FORMAT`
//!   only decides which one the feed links to.
//! - `GET /stream/<episode_id>.aac?src=<signed .m3u8>` — the earlier AAC remux,
//!   kept so enclosure URLs that podcatchers already stored keep working.
//!
//! No auth: the signed playlist URL is itself the credential, and only
//! Podimo-hosted playlists are accepted. Anyone who has one enclosure URL can
//! open it many times, so `podimo::hls` caps how many streams run at once;
//! when every slot is taken, a request gets a 503 with `Retry-After`.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures::{Stream, TryStreamExt};
use reqwest::Url;
use std::collections::HashMap;

use crate::error::AppError;
use crate::podimo::hls::{self, HlsError, StreamFormat};
use crate::state::AppState;
use crate::util::PODCAST_ID_RE;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/stream/{file}", get(serve_aac))
        .route("/stream/{token}/{file}", get(serve_transcoded))
}

async fn serve_transcoded(
    State(state): State<AppState>,
    method: Method,
    Path((token, file)): Path<(String, String)>,
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
    let segments = match resolve(&state, episode_id, &src).await {
        Ok(segments) => segments,
        Err(resp) => return resp,
    };
    // HEAD only needs to know the stream would start: don't spawn ffmpeg (or
    // take a stream slot) for a body axum throws away anyway.
    if method == Method::HEAD {
        let empty = futures::stream::empty();
        return audio_response(episode_id, format.extension(), format.content_type(), empty);
    }
    let aac = hls::aac_stream(state.scraper.clone(), segments);
    match hls::transcode(aac, format).await {
        Ok(body) => audio_response(episode_id, format.extension(), format.content_type(), body),
        Err(err) => start_failed(episode_id, err),
    }
}

async fn serve_aac(
    State(state): State<AppState>,
    method: Method,
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
    let segments = match resolve(&state, episode_id, src).await {
        Ok(segments) => segments,
        Err(resp) => return resp,
    };
    // As above: a HEAD response has no body to hold a stream slot for.
    if method == Method::HEAD {
        let empty = futures::stream::empty();
        return audio_response(episode_id, "aac", hls::AAC_CONTENT_TYPE, empty);
    }
    let aac = hls::aac_stream(state.scraper.clone(), segments);
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
        .map_err(|err| AppError::BadRequest(err.to_string()).into_response())?;
    let segments = hls::resolve_segments(&state.scraper, &src)
        .await
        .map_err(|err| {
            tracing::warn!(target: "podimo", "stream {episode_id}: {err}");
            AppError::UpstreamUnavailable(err.to_string()).into_response()
        })?;
    tracing::info!(target: "podimo", "stream {episode_id}: {} segments", segments.len());
    Ok(segments)
}

fn audio_response<S>(episode_id: &str, ext: &str, content_type: &str, body: S) -> Response
where
    S: Stream<Item = Result<axum::body::Bytes, HlsError>> + Send + 'static,
{
    let id = episode_id.to_string();
    // Headers are long gone by the time a mid-stream error happens, so this
    // is the only place it gets reported.
    let body = body.inspect_err(move |err| {
        tracing::warn!(target: "podimo", "stream {id} aborted: {err}");
    });
    (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"{episode_id}.{ext}\""),
            ),
        ],
        Body::from_stream(body),
    )
        .into_response()
}
