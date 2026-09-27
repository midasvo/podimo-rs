//! After-request CORS + Cache-Control middleware and request logging.

use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method};
use axum::middleware::Next;
use axum::response::Response;

use crate::state::AppState;

pub(crate) async fn after_request(
    State(_state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_owned();

    let mut response = next.run(req).await;
    let is_success = response.status().is_success();
    let status = response.status();
    let headers = response.headers_mut();

    // Feeds and episode audio: what podcatchers fetch, from any origin.
    let is_content = matches!(method, Method::GET | Method::HEAD)
        && (path.starts_with("/feed/")
            || path.starts_with("/audiobook/")
            || path.starts_with("/stream/"));
    if is_content {
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, HEAD"),
        );
    }

    // Only content may be cached. The HTML pages show state that changes
    // (library downloads) or credentials (the generated feed URL), and a
    // browser may answer a navigation from a cached copy.
    let cc = if is_content && is_success {
        "max-age=900"
    } else {
        "no-store"
    };
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cc));

    // Milestone access log for content and app routes (silences /healthz polling)
    if path != "/healthz" {
        let display_path = sanitize_path_for_log(&path);
        let elapsed = start.elapsed();
        let status_code = status.as_u16();

        if status.is_server_error() {
            tracing::error!(
                target: "podimo::http",
                "{method} {display_path} -> {status_code} ({elapsed:.1?})",
            );
        } else if status.is_client_error() {
            tracing::warn!(
                target: "podimo::http",
                "{method} {display_path} -> {status_code} ({elapsed:.1?})",
            );
        } else {
            tracing::info!(
                target: "podimo::http",
                "{method} {display_path} -> {status_code} ({elapsed:.1?})",
            );
        }
    }

    response
}

/// Sanitize paths with large tokens (like `/stream/<base64_token>/<id>.ext`)
/// so logs stay readable and signed URLs aren't dumped verbatim.
fn sanitize_path_for_log(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("/stream/") {
        if let Some((_token, file)) = rest.split_once('/') {
            return format!("/stream/[token]/{file}");
        }
    }
    path.to_string()
}
