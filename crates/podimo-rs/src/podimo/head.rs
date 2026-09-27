//! Episode-media HEAD probe with retries + cache.

use std::time::Duration;

use reqwest::Client;

use crate::cache::{HeadInfo, Hit, TtlCache};

const RETRIES: u32 = 3;
const TIMEOUT_PER_TRY: Duration = Duration::from_secs(10);

/// Content length and type of the media at `url`, cached under `episode_id`.
///
/// A fresh cache entry skips the probe. If every attempt fails, an expired
/// entry is returned instead of the error, so a flaky upstream doesn't drop
/// the episode from its feed. Only request errors (a timeout, a refused
/// connection) fail an attempt: any HTTP response, error statuses included,
/// is cached as the answer.
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
    for attempt in 0..RETRIES {
        let mut req = scraper.head(url).timeout(TIMEOUT_PER_TRY);
        for (k, v) in &headers {
            req = req.header(k, v);
        }

        match req.send().await {
            Ok(resp) => {
                let content_length = resp
                    .headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("0")
                    .to_string();

                let guessed = mime_guess::from_path(strip_query(url)).first_raw();
                let content_type = guessed
                    .map(|s| s.to_string())
                    .or_else(|| {
                        resp.headers()
                            .get(reqwest::header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_else(|| "audio/mpeg".to_string());

                let info = HeadInfo {
                    content_length,
                    content_type,
                };
                cache.insert(episode_id.to_string(), info.clone()).await;
                return Ok(info);
            }
            Err(err) => {
                if attempt + 1 < RETRIES {
                    let delay = 2u64.saturating_pow(attempt);
                    tracing::info!(target: "podimo", "retrying HEAD {url} after {err} (attempt {}/{RETRIES})", attempt + 2);
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                }
                last_err = Some(err);
            }
        }
    }
    let err = last_err.expect("loop ran at least once");
    match expired {
        Some(info) => {
            tracing::warn!(target: "podimo", "HEAD probe failed for {episode_id}, using its expired cached size: {err}");
            Ok(info)
        }
        None => Err(err),
    }
}

fn strip_query(url: &str) -> &str {
    url.split_once('?').map(|(p, _)| p).unwrap_or(url)
}
