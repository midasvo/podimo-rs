//! Shared authorization for `/feed/*` and `/audiobook/*`.
//!
//! Both content-type endpoints share the same gate: resolve credentials (basic
//! auth or `LOCAL_CREDENTIALS`), validate the id format / region / locale,
//! enforce the blocklist, then either reuse a cached login token or log in
//! against Podimo. The two handlers only differ in what they do *after*
//! authorization — which upstream calls they make and how they render the RSS.

use std::collections::HashMap;

use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::config::{is_known_locale, is_known_region};
use crate::error::AppError;
use crate::podimo::{ClientError, PodimoClient};
use crate::state::AppState;
use crate::util::{amp_arg, parse_basic_auth, split_username_region_locale, PODCAST_ID_RE};

/// Result of a successful authorization. The client is already logged-in (its
/// `token` is set, either from the token cache or a fresh login).
pub(crate) struct Authorized {
    pub client: PodimoClient,
    pub locale: String,
    /// The token came from the cache rather than a login just now, so
    /// upstream may have revoked it since. See [`Self::forget_token_after`].
    pub token_from_cache: bool,
}

impl Authorized {
    /// Called when a query with this login failed. Unless the content just
    /// doesn't exist, a cached token may be what upstream rejected: drop it,
    /// so the next request logs in again instead of failing until it expires.
    pub(crate) async fn forget_token_after(&self, state: &AppState, err: &ClientError) {
        if self.token_from_cache && !err.is_not_found() {
            state.caches.tokens.remove(&self.client.key).await;
        }
    }
}

/// 401 with a body that helps the user re-craft a correct Basic-auth URL.
/// Lives here (rather than in `error.rs`) because the body depends on
/// `AppState`'s hostname, which `IntoResponse` doesn't see.
pub(crate) fn unauthorized_response(hostname: &str) -> Response {
    let body = format!(
        "401 Unauthorized.\n\
You need to login with the correct credentials for Podimo.\n\n\
{}",
        crate::handlers::not_found::example_block(hostname)
    );
    let mut resp = (StatusCode::UNAUTHORIZED, body).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm='Podimo credentials'"),
    );
    resp
}

/// Runs every gate the two content endpoints share:
///
/// 1. Resolve credentials (basic auth, or `LOCAL_CREDENTIALS` + query-string region/locale).
/// 2. Validate the content id format.
/// 3. Validate region / locale.
/// 4. Apply the blocklist (substring match on the full path+query).
/// 5. Reuse a cached login token, or log in against Podimo.
///
/// `content_kind_label` is plain English ("podcast" / "audiobook") and only
/// appears in the 400 "Invalid X id format" error body. Order matches the
/// pre-extraction handlers so external behaviour is identical.
// The `Response` error is returned straight to axum, same as `library_or_404`.
#[allow(clippy::result_large_err)]
pub(crate) async fn authorize_request(
    state: &AppState,
    params: &HashMap<String, String>,
    req_headers: &axum::http::HeaderMap,
    uri: &Uri,
    content_id: &str,
    content_kind_label: &str,
) -> Result<Authorized, Response> {
    let (username, password, region, locale) = if state.config.local_credentials {
        let region = amp_arg(|k| params.get(k).map(String::as_str), "region")
            .unwrap_or_else(|| state.config.podimo_region.clone());
        let locale = amp_arg(|k| params.get(k).map(String::as_str), "locale")
            .unwrap_or_else(|| state.config.podimo_locale.clone());
        let Some(email) = state.config.podimo_email.clone() else {
            return Err(AppError::Internal(
                "LOCAL_CREDENTIALS enabled but PODIMO_EMAIL unset".into(),
            )
            .into_response());
        };
        let Some(pw) = state.config.podimo_password.clone() else {
            return Err(AppError::Internal(
                "LOCAL_CREDENTIALS enabled but PODIMO_PASSWORD unset".into(),
            )
            .into_response());
        };
        (email, pw, region, locale)
    } else {
        match parse_basic_auth(req_headers) {
            Some((user_field, password)) => {
                let (username, region, locale) = split_username_region_locale(&user_field);
                (username, password, region, locale)
            }
            None => return Err(unauthorized_response(&state.config.hostname)),
        }
    };

    if !PODCAST_ID_RE.is_match(content_id) {
        return Err(
            AppError::BadRequest(format!("Invalid {content_kind_label} id format")).into_response(),
        );
    }
    if !is_known_region(&region) {
        return Err(AppError::BadRequest("Invalid region".into()).into_response());
    }
    if !is_known_locale(&locale) {
        return Err(AppError::BadRequest("Invalid locale".into()).into_response());
    }

    let url_for_blocklist = uri.path_and_query().map(|p| p.as_str()).unwrap_or("");
    if state.blocklist.contains_substring(url_for_blocklist) {
        return Err(AppError::Gone.into_response());
    }

    let mut client = match PodimoClient::new(&username, &password, &region, &locale) {
        Ok(c) => c,
        Err(_) => return Err(unauthorized_response(&state.config.hostname)),
    };
    let cached_token = state.caches.tokens.get(&client.key).await;
    let token_from_cache = cached_token.is_some();
    // No email in these: on a shared instance, they're other people's.
    if let Some(token) = cached_token {
        tracing::debug!(target: "podimo::auth", "reusing cached token");
        client.token = Some(token);
    } else {
        tracing::info!(target: "podimo::auth", "logging in to Podimo (region: {region})");
        match client.login(&state.scraper, &state.config).await {
            Ok(token) => state.caches.tokens.insert(client.key.clone(), token).await,
            Err(ClientError::InvalidCredentials(_)) => {
                return Err(unauthorized_response(&state.config.hostname));
            }
            Err(err) => {
                tracing::error!(target: "podimo", "upstream auth failure: {err}");
                return Err(AppError::UpstreamUnavailable(err.to_string()).into_response());
            }
        }
    }

    Ok(Authorized {
        client,
        locale,
        token_from_cache,
    })
}
