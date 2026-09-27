//! Episode-media HEAD probe with retries + cache.

use std::time::Duration;

use reqwest::{Client, StatusCode};

use crate::cache::{HeadInfo, Hit, TtlCache};

const RETRIES: u32 = 3;
const TIMEOUT_PER_TRY: Duration = Duration::from_secs(10);

/// Content length and type of the media at `url`, cached under `episode_id`.
///
/// A fresh cache entry skips the probe. Only a 2xx answer is cached: an error
/// page's length says nothing about the file. A 5xx fails the attempt like a
/// timeout or a refused connection and is retried; any other error status
/// ends the probe at once.
///
/// When the probe fails, an expired entry is returned if there is one, so a
/// flaky upstream doesn't drop the episode from its feed. Without one, an
/// error status gives length `"0"`, the conventional unknown length, which
/// keeps the episode in the feed and isn't cached, so the next feed request
/// probes again. Only when no attempt got any answer is the result an error.
pub async fn url_head_info(
    scraper: &Client,
    cache: &TtlCache<HeadInfo>,
    episode_id: &str,
    url: &str,
    locale: &str,
) -> Result<HeadInfo, reqwest::Error> {
    let expired = match cache.get_stale(episode_id).await {
        Some(Hit::Fresh(info)) => return Ok(info),
        Some(Hit::Expired(info)) => Some(info),
        None => None,
    };

    let headers = crate::util::generate_headers(None, locale);

    let mut last_err = None;
    let mut error_status: Option<StatusCode> = None;
    let mut failure = String::new();
    for attempt in 0..RETRIES {
        let mut req = scraper.head(url).timeout(TIMEOUT_PER_TRY);
        for (k, v) in &headers {
            req = req.header(k, v);
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                let content_length = resp
                    .headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("0")
                    .to_string();
                let content_type = content_type(
                    url,
                    resp.headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok()),
                );

                let info = HeadInfo {
                    content_length,
                    content_type,
                };
                cache.insert(episode_id.to_string(), info.clone()).await;
                return Ok(info);
            }
            Ok(resp) => {
                let status = resp.status();
                error_status = Some(status);
                failure = format!("status {status}");
                // A 5xx may pass; a 403 or 404 won't change on a retry.
                if !status.is_server_error() {
                    break;
                }
            }
            Err(err) => {
                failure = err.to_string();
                last_err = Some(err);
            }
        }
        if attempt + 1 < RETRIES {
            let delay = 2u64.saturating_pow(attempt);
            tracing::info!(target: "podimo", "retrying HEAD {url} after {failure} (attempt {}/{RETRIES})", attempt + 2);
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    }

    if let Some(info) = expired {
        tracing::warn!(target: "podimo", "HEAD probe failed for {episode_id}, using its expired cached size: {failure}");
        return Ok(info);
    }
    match error_status {
        Some(status) => {
            tracing::info!(target: "podimo", "HEAD probe for {episode_id} answered {status}; length unknown");
            Ok(HeadInfo {
                content_length: "0".into(),
                content_type: content_type(url, None),
            })
        }
        None => Err(last_err.expect("no response, so every attempt was a request error")),
    }
}

/// The media type of `url`: guessed from its extension, else the one the
/// response gave, else `audio/mpeg`.
fn content_type(url: &str, from_response: Option<&str>) -> String {
    mime_guess::from_path(strip_query(url))
        .first_raw()
        .or(from_response)
        .unwrap_or("audio/mpeg")
        .to_string()
}

fn strip_query(url: &str) -> &str {
    url.split_once('?').map(|(p, _)| p).unwrap_or(url)
}
