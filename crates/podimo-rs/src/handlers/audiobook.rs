//! GET /audiobook/<audiobook_id>.xml.
//!
//! Single-item RSS feed per audiobook: one channel, one item (the book), one
//! enclosure pointing at a short-lived signed audio URL. Authorization gates
//! (basic-auth, region/locale, blocklist, login) are shared with `feed.rs` via
//! `handlers::auth`.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::collections::HashMap;

use crate::error::AppError;
use crate::handlers::auth::authorize_request;
use crate::state::AppState;

pub(crate) fn router() -> Router<AppState> {
    Router::new().route("/audiobook/{audiobook_id}", get(serve))
}

async fn serve(
    State(state): State<AppState>,
    Path(audiobook_id_with_ext): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    uri: Uri,
    req_headers: axum::http::HeaderMap,
) -> Response {
    let Some(audiobook_id) = audiobook_id_with_ext.strip_suffix(".xml") else {
        return (StatusCode::NOT_FOUND, "404 Not found.").into_response();
    };

    let auth = match authorize_request(
        &state,
        &params,
        &req_headers,
        &uri,
        audiobook_id,
        "audiobook",
    )
    .await
    {
        Ok(a) => a,
        Err(resp) => return resp,
    };

    // Metadata and audio-URL queries are independent (different GraphQL fields)
    // and both go through the same Cloudflare-bypass path, so the cold-cache
    // wall-time is dominated by the slower of the two — `tokio::join!` halves
    // it. On warm cache each branch short-circuits before any network call.
    let (meta_result, audio_result) = tokio::join!(
        auth.client.get_audiobook(
            &state.scraper,
            &state.config,
            audiobook_id,
            &state.caches.audiobook_meta,
        ),
        auth.client.get_audiobook_audio_url(
            &state.scraper,
            &state.config,
            audiobook_id,
            &state.caches.audiobook_audio,
        ),
    );

    let meta = match meta_result {
        Ok(v) => v,
        Err(err) if err.is_not_found() => return AppError::NotFound.into_response(),
        Err(err) => return AppError::Internal(format!("fetch audiobook: {err}")).into_response(),
    };

    let audio_url = match audio_result {
        Ok(v) => v,
        Err(err) if err.is_not_found() => return AppError::NotFound.into_response(),
        Err(err) => {
            return AppError::Internal(format!("fetch audiobook audio: {err}")).into_response()
        }
    };

    match crate::podimo::rss::audiobook_to_rss(
        &meta,
        &audio_url,
        audiobook_id,
        &auth.locale,
        state.config.public_feeds,
        &state.scraper,
        &state.caches.head,
    )
    .await
    {
        Ok(rss) => (StatusCode::OK, [(header::CONTENT_TYPE, "text/xml")], rss).into_response(),
        Err(err) => AppError::Internal(format!("rss render: {err}")).into_response(),
    }
}
