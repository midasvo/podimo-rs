//! GET /feed/<podcast_id>.xml.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::collections::HashMap;

use crate::error::AppError;
use crate::handlers::auth::authorize_request;
use crate::podimo::rss::StreamLinks;
use crate::state::AppState;
use crate::util::{amp_arg, request_base_url};

pub(crate) fn router() -> Router<AppState> {
    Router::new().route("/feed/{podcast_id}", get(serve))
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
        Err(err) => {
            auth.forget_token_after(&state, &err).await;
            return AppError::Internal(format!("fetch podcasts: {err}")).into_response();
        }
    };

    // Stream links go back through whatever address this client used for the
    // feed, so an in-cluster client (e.g. `http://podimo`) and one coming in
    // through the public reverse proxy each get links they can reach. A proxy
    // that rewrites `Host` hides that address, hence the setting to always
    // use the configured one.
    let config = &state.config;
    let stream_base_url = if config.stream_links_from_request {
        request_base_url(&req_headers, &config.protocol, &config.hostname)
    } else {
        format!("{}://{}", config.protocol, config.hostname)
    };
    let stream_links = StreamLinks {
        base_url: &stream_base_url,
        format: state.config.stream_format,
    };
    match crate::podimo::rss::podcasts_to_rss(
        &payload,
        podcast_id,
        &auth.locale,
        state.config.public_feeds,
        limit,
        stream_links,
        &state.scraper,
        &state.caches.head,
    )
    .await
    {
        Ok(rss) => {
            let title = payload
                .pointer("/podcast/title")
                .and_then(|t| t.as_str())
                .unwrap_or(podcast_id);
            // The cached payload can hold more episodes than the feed shows.
            let available = payload
                .get("episodes")
                .and_then(|e| e.as_array())
                .map_or(0, Vec::len);
            let episode_count = limit.map_or(available, |n| n.min(available));
            tracing::info!(
                target: "podimo::feed",
                "served feed for \"{title}\" ({podcast_id}) with {episode_count} episodes"
            );
            (StatusCode::OK, [(header::CONTENT_TYPE, "text/xml")], rss).into_response()
        }
        Err(err) => AppError::Internal(format!("rss render: {err}")).into_response(),
    }
}
