//! GET /feed/<podcast_id>.xml.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::collections::HashMap;

use crate::error::AppError;
use crate::handlers::auth::authorize_request;
use crate::state::AppState;
use crate::util::amp_arg;

pub(crate) fn router() -> Router<AppState> {
    Router::new().route("/feed/:podcast_id", get(serve))
}

async fn serve(
    State(state): State<AppState>,
    Path(podcast_id_with_ext): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    uri: Uri,
    req_headers: axum::http::HeaderMap,
) -> Response {
    // The route matches anything under /feed/<segment>; require `.xml` so the
    // router doesn't have to treat the dot specially.
    let Some(podcast_id) = podcast_id_with_ext.strip_suffix(".xml") else {
        return (StatusCode::NOT_FOUND, "404 Not found.").into_response();
    };

    // Parse `?limit=` first — a malformed limit is cheap to detect and gives a
    // clearer 400 than the auth path would.
    let limit = match amp_arg(|k| params.get(k).map(String::as_str), "limit") {
        Some(s) if s.is_empty() => None,
        Some(s) => match s.parse::<usize>() {
            Ok(n) if n >= 1 => Some(n),
            _ => {
                return AppError::BadRequest("Invalid limit (must be a positive integer)".into())
                    .into_response();
            }
        },
        None => None,
    };

    let auth =
        match authorize_request(&state, &params, &req_headers, &uri, podcast_id, "podcast").await {
            Ok(a) => a,
            Err(resp) => return resp,
        };

    let payload = match auth
        .client
        .get_podcasts(
            &state.scraper,
            &state.config,
            podcast_id,
            limit,
            &state.caches.podcasts,
        )
        .await
    {
        Ok(v) => v,
        Err(err) if err.is_not_found() => return AppError::NotFound.into_response(),
        Err(err) => return AppError::Internal(format!("fetch podcasts: {err}")).into_response(),
    };

    let stream_base_url = format!("{}://{}", state.config.protocol, state.config.hostname);
    match crate::podimo::rss::podcasts_to_rss(
        &payload,
        podcast_id,
        &auth.locale,
        state.config.public_feeds,
        limit,
        &stream_base_url,
        &state.scraper,
        &state.caches.head,
    )
    .await
    {
        Ok(rss) => (StatusCode::OK, [(header::CONTENT_TYPE, "text/xml")], rss).into_response(),
        Err(err) => AppError::Internal(format!("rss render: {err}")).into_response(),
    }
}
