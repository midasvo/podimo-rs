//! GET /stream/<episode_id>.aac?src=<signed Podimo .m3u8 URL>.
//!
//! Remuxes an HLS episode into one progressive ADTS AAC stream (see
//! `podimo::hls`). No auth: the signed playlist URL in `src` is itself the
//! credential, and only Podimo-hosted playlists are accepted.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::collections::HashMap;

use crate::error::AppError;
use crate::podimo::hls;
use crate::state::AppState;
use crate::util::PODCAST_ID_RE;

pub(crate) fn router() -> Router<AppState> {
    Router::new().route("/stream/:file", get(serve))
}

async fn serve(
    State(state): State<AppState>,
    Path(file): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(episode_id) = file.strip_suffix(".aac") else {
        return (StatusCode::NOT_FOUND, "404 Not found.").into_response();
    };
    if !PODCAST_ID_RE.is_match(episode_id) {
        return AppError::BadRequest("Invalid episode id".into()).into_response();
    }
    let Some(src) = params.get("src") else {
        return AppError::BadRequest("Missing src".into()).into_response();
    };
    let src = match hls::validate_source(src) {
        Ok(url) => url,
        Err(err) => return AppError::BadRequest(err.to_string()).into_response(),
    };

    let segments = match hls::resolve_segments(&state.scraper, &src).await {
        Ok(segments) => segments,
        Err(err) => {
            tracing::warn!(target: "podimo", "stream {episode_id}: {err}");
            return AppError::UpstreamUnavailable(err.to_string()).into_response();
        }
    };
    tracing::info!(target: "podimo", "stream {episode_id}: {} segments", segments.len());

    let body = Body::from_stream(hls::aac_stream(state.scraper.clone(), segments));
    (
        [
            (header::CONTENT_TYPE, hls::STREAM_CONTENT_TYPE.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"{episode_id}.aac\""),
            ),
        ],
        body,
    )
        .into_response()
}
