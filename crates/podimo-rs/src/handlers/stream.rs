//! Progressive audio for HLS episodes (see `podimo::hls`):
//!
//! - `GET /stream/<token>/<episode_id>.mp3` — what feed enclosures point at:
//!   transcoded to MP3 by ffmpeg. `token` is the signed Podimo `.m3u8` URL in
//!   base64url, so the URL itself ends in a plain `.mp3`.
//! - `GET /stream/<episode_id>.aac?src=<signed .m3u8>` — the earlier AAC remux,
//!   kept so enclosure URLs that podcatchers already stored keep working.
//!
//! No auth: the signed playlist URL is itself the credential, and only
//! Podimo-hosted playlists are accepted.

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
use crate::podimo::hls::{self, HlsError};
use crate::state::AppState;
use crate::util::PODCAST_ID_RE;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/stream/:file", get(serve_aac))
        .route("/stream/:token/:file", get(serve_mp3))
}

async fn serve_mp3(
    State(state): State<AppState>,
    method: Method,
    Path((token, file)): Path<(String, String)>,
) -> Response {
    let episode_id = match episode_id(&file, ".mp3") {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let src = match hls::decode_source_token(&token) {
        Ok(src) => src,
        Err(err) => return AppError::BadRequest(err.to_string()).into_response(),
    };
    let segments = match resolve(&state, episode_id, &src).await {
        Ok(segments) => segments,
        Err(resp) => return resp,
    };
    // HEAD only needs to know the stream would start: don't spawn ffmpeg (or
    // queue for a transcode slot) for a body axum throws away anyway.
    if method == Method::HEAD {
        let empty = futures::stream::empty();
        return audio_response(episode_id, "mp3", hls::MP3_CONTENT_TYPE, empty);
    }
    match hls::transcode_to_mp3(hls::aac_stream(state.scraper.clone(), segments)).await {
        Ok(mp3) => audio_response(episode_id, "mp3", hls::MP3_CONTENT_TYPE, mp3),
        Err(err) => AppError::Internal(format!("stream {episode_id}: {err}")).into_response(),
    }
}

async fn serve_aac(
    State(state): State<AppState>,
    Path(file): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let episode_id = match episode_id(&file, ".aac") {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let Some(src) = params.get("src") else {
        return AppError::BadRequest("Missing src".into()).into_response();
    };
    let segments = match resolve(&state, episode_id, src).await {
        Ok(segments) => segments,
        Err(resp) => return resp,
    };
    let aac = hls::aac_stream(state.scraper.clone(), segments);
    audio_response(episode_id, "aac", hls::AAC_CONTENT_TYPE, aac)
}

// The `Response` errors below go straight back to axum, same as `library_or_404`.

/// The episode id from `<id><ext>`, or the response to send instead.
#[allow(clippy::result_large_err)]
fn episode_id<'a>(file: &'a str, ext: &str) -> Result<&'a str, Response> {
    let Some(id) = file.strip_suffix(ext) else {
        return Err((StatusCode::NOT_FOUND, "404 Not found.").into_response());
    };
    if !PODCAST_ID_RE.is_match(id) {
        return Err(AppError::BadRequest("Invalid episode id".into()).into_response());
    }
    Ok(id)
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
