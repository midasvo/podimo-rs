//! After-request CORS + Cache-Control middleware.

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
    let method = req.method().clone();
    let path = req.uri().path().to_owned();

    let mut response = next.run(req).await;
    let is_success = response.status().is_success();
    let headers = response.headers_mut();

    if matches!(method, Method::GET | Method::HEAD)
        && (path.starts_with("/feed/")
            || path.starts_with("/audiobook/")
            || path.starts_with("/stream/"))
    {
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, HEAD"),
        );
    }

    let cc = if path == "/healthz" {
        "no-store"
    } else if is_success {
        "max-age=900"
    } else {
        "no-store"
    };
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cc));

    response
}
