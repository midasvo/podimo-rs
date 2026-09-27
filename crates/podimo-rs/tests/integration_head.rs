//! Tests for `podimo::head::url_head_info`.

use std::time::Duration;

use podimo_rs::cache::{HeadInfo, Hit, TtlCache};
use podimo_rs::podimo::head::url_head_info;
use reqwest::Client;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn empty_head_cache() -> TtlCache<HeadInfo> {
    TtlCache::new("head_test", None, Duration::from_secs(60)).await
}

/// A head cache holding an expired entry for `key` (a zero TTL expires at once).
async fn head_cache_with_expired(key: &str, content_length: &str) -> TtlCache<HeadInfo> {
    let cache = empty_head_cache().await;
    cache
        .insert_with_ttl(
            key.to_string(),
            HeadInfo {
                content_length: content_length.into(),
                content_type: "audio/mpeg".into(),
            },
            Duration::ZERO,
        )
        .await;
    cache
}

/// Serves `/stalled.mp3` far slower than `impatient_client` waits, so every
/// probe attempt times out. Expects one request per attempt.
async fn stalled_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/stalled.mp3"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .expect(3)
        .mount(&server)
        .await;
    server
}

/// Gives up on a response after 250 ms, well within `url_head_info`'s own
/// 10 s per attempt.
fn impatient_client() -> Client {
    Client::builder()
        .read_timeout(Duration::from_millis(250))
        .build()
        .expect("client")
}

#[tokio::test]
async fn missing_content_length_returns_string_zero() {
    // A HEAD response with no Content-Length must surface as the string "0" so
    // the RSS enclosure builder doesn't crash on a non-string length.
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/episode.mp3"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let cache = empty_head_cache().await;
    let client = Client::new();
    let info = url_head_info(
        &client,
        &cache,
        "ep-fresh",
        &format!("{}/episode.mp3", server.uri()),
        "nl-NL",
    )
    .await
    .expect("HEAD");
    assert_eq!(info.content_length, "0");
}

#[tokio::test]
async fn content_length_from_header_is_passed_through() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/x.mp3"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "9876")
                .insert_header("Content-Type", "audio/mpeg"),
        )
        .mount(&server)
        .await;

    let cache = empty_head_cache().await;
    let client = Client::new();
    let info = url_head_info(
        &client,
        &cache,
        "ep-with-length",
        &format!("{}/x.mp3", server.uri()),
        "nl-NL",
    )
    .await
    .expect("HEAD");
    assert_eq!(info.content_length, "9876");
    // mime_guess from path returns "audio/mpeg" for .mp3, which wins over the
    // response header. Either way, it must be audio/mpeg.
    assert_eq!(info.content_type, "audio/mpeg");
}

#[tokio::test]
async fn cached_head_info_short_circuits_network() {
    // If the cache already has a valid entry, no HTTP request is issued. Verify
    // by pointing at a URL that would otherwise fail (no server listening) and
    // confirming we still get the cached value.
    let cache = empty_head_cache().await;
    cache
        .insert(
            "ep-cached".to_string(),
            HeadInfo {
                content_length: "42".into(),
                content_type: "audio/mpeg".into(),
            },
        )
        .await;
    let info = url_head_info(
        &Client::new(),
        &cache,
        "ep-cached",
        "http://127.0.0.1:1/never-called",
        "nl-NL",
    )
    .await
    .expect("cached");
    assert_eq!(info.content_length, "42");
}

#[tokio::test]
async fn expired_entry_is_returned_when_every_probe_fails() {
    let server = stalled_upstream().await;
    let cache = head_cache_with_expired("ep-stale", "42").await;

    let info = url_head_info(
        &impatient_client(),
        &cache,
        "ep-stale",
        &format!("{}/stalled.mp3", server.uri()),
        "nl-NL",
    )
    .await
    .expect("the expired entry instead of the HEAD error");
    assert_eq!(info.content_length, "42");

    // The failed probe leaves the entry as it was: still expired, so the next
    // feed request probes again.
    match cache.get_stale("ep-stale").await {
        Some(Hit::Expired(info)) => assert_eq!(info.content_length, "42"),
        other => panic!("want the untouched expired entry, got {other:?}"),
    }
}

#[tokio::test]
async fn failed_probe_without_cached_entry_is_an_error() {
    let server = stalled_upstream().await;

    let result = url_head_info(
        &impatient_client(),
        &empty_head_cache().await,
        "ep-uncached",
        &format!("{}/stalled.mp3", server.uri()),
        "nl-NL",
    )
    .await;
    assert!(result.is_err(), "nothing to fall back on: {result:?}");
}

#[tokio::test]
async fn expired_entry_is_replaced_by_a_successful_probe() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/x.mp3"))
        .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "9876"))
        .expect(1)
        .mount(&server)
        .await;
    let cache = head_cache_with_expired("ep-expired", "42").await;

    let info = url_head_info(
        &Client::new(),
        &cache,
        "ep-expired",
        &format!("{}/x.mp3", server.uri()),
        "nl-NL",
    )
    .await
    .expect("HEAD");
    assert_eq!(info.content_length, "9876");
    let cached = cache.get("ep-expired").await.expect("a fresh entry");
    assert_eq!(cached.content_length, "9876");
}
