//! HTTP-level tests for /stream that don't require Podimo upstream: routing,
//! request validation and middleware. The transcode itself is covered by the
//! unit tests in `podimo::hls`.

use std::net::SocketAddr;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use podimo_rs::{app, config::Config, AppState};
use tokio::net::TcpListener;

const EPISODE: &str = "71672e78-7a1a-4725-9f89-c3def125e22f";

/// Build a Config without touching process env vars (so parallel tests don't
/// race on the env). Each call gets its own tempdir for CACHE_DIR.
fn make_test_config() -> Config {
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        hostname: "localhost:12104".into(),
        bind_host: "127.0.0.1:12104".into(),
        protocol: "http".into(),
        http_proxy: None,
        zenrows_api: None,
        scraper_api: None,
        cache_dir: cache_dir.path().to_string_lossy().to_string(),
        block_list_file: "/dev/null".into(),
        debug: false,
        local_credentials: false,
        podimo_email: None,
        podimo_password: None,
        podimo_region: "nl".into(),
        podimo_locale: "nl-NL".into(),
        store_tokens_on_disk: false,
        token_cache_time: 60,
        podcast_cache_time: 60,
        head_cache_time: 60,
        audiobook_audio_cache_time: 60,
        enable_library: false,
        library_dir: "./library".into(),
        public_feeds: false,
        graphql_url: "https://example.invalid/graphql".into(),
        stream_format: podimo_rs::podimo::hls::StreamFormat::Mp3,
        stream_links_from_request: true,
    };
    // Leak the tempdir so the cache dir persists for the test's lifetime.
    std::mem::forget(cache_dir);
    config
}

async fn boot() -> SocketAddr {
    boot_with(make_test_config()).await
}

/// Boots with a block list holding `entries`, one per line.
async fn boot_with_block_list(entries: &[&str]) -> SocketAddr {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), entries.join("\n")).unwrap();
    let mut config = make_test_config();
    config.block_list_file = file.path().to_string_lossy().to_string();
    // The list is read at startup, so the file can go once the app is up.
    boot_with(config).await
}

async fn boot_with(config: Config) -> SocketAddr {
    let state = AppState::new(config).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state).await.unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

async fn get(addr: SocketAddr, path: &str) -> reqwest::Response {
    reqwest::get(format!("http://{addr}{path}")).await.unwrap()
}

fn token(src: &str) -> String {
    URL_SAFE_NO_PAD.encode(src)
}

#[tokio::test]
async fn mp3_route_requires_mp3_suffix() {
    let addr = boot().await;
    let tok = token("https://media-cdn-episodes.podimo.com/a/a.m3u8");
    let resp = get(addr, &format!("/stream/{tok}/{EPISODE}.ogg")).await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn mp3_route_rejects_malformed_token() {
    let addr = boot().await;
    let resp = get(addr, &format!("/stream/not*base64/{EPISODE}.mp3")).await;
    assert_eq!(resp.status(), 400);
    assert!(resp.text().await.unwrap().contains("malformed token"));
}

#[tokio::test]
async fn m4a_is_served_on_the_same_route() {
    let addr = boot().await;
    // Gets as far as token validation, so the route and extension are accepted.
    let resp = get(addr, &format!("/stream/not*base64/{EPISODE}.m4a")).await;
    assert_eq!(resp.status(), 400);
    assert!(resp.text().await.unwrap().contains("malformed token"));
}

#[tokio::test]
async fn mp3_route_rejects_non_podimo_sources() {
    let addr = boot().await;
    for src in [
        "https://evil.example/a.m3u8",
        "http://media-cdn-episodes.podimo.com/a/a.m3u8",
        "https://media-cdn-episodes.podimo.com/a/a.mp3",
    ] {
        let resp = get(addr, &format!("/stream/{}/{EPISODE}.mp3", token(src))).await;
        assert_eq!(resp.status(), 400, "{src} should be rejected");
    }
}

#[tokio::test]
async fn mp3_route_rejects_invalid_episode_id() {
    let addr = boot().await;
    let tok = token("https://media-cdn-episodes.podimo.com/a/a.m3u8");
    let resp = get(addr, &format!("/stream/{tok}/not_an_id.mp3")).await;
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn legacy_aac_route_still_validates() {
    let addr = boot().await;
    let resp = get(addr, &format!("/stream/{EPISODE}.aac")).await;
    assert_eq!(resp.status(), 400, "missing src");
    let resp = get(
        addr,
        &format!("/stream/{EPISODE}.aac?src=https%3A%2F%2Fevil.example%2Fa.m3u8"),
    )
    .await;
    assert_eq!(resp.status(), 400, "foreign host");
}

#[tokio::test]
async fn blocked_episode_returns_410_on_both_routes() {
    let addr = boot_with_block_list(&[EPISODE]).await;
    // A playlist on Podimo's CDN: without the block list check, the handler
    // would go on to fetch it.
    let src = "https://media-cdn-episodes.podimo.com/a/a.m3u8";
    let paths = [
        format!("/stream/{}/{EPISODE}.m4a", token(src)),
        format!("/stream/{}/{EPISODE}.mp3", token(src)),
        format!("/stream/{}/{}.m4a", token(src), EPISODE.to_uppercase()),
        format!("/stream/{EPISODE}.aac?src={}", urlencoding::encode(src)),
    ];
    for path in paths {
        assert_eq!(get(addr, &path).await.status(), 410, "{path}");
    }
}

#[tokio::test]
async fn block_list_matches_the_playlist_url_inside_the_token() {
    let addr = boot_with_block_list(&["blocked-show"]).await;
    let src = "https://media-cdn-episodes.podimo.com/blocked-show/ep.m3u8";
    let resp = get(addr, &format!("/stream/{}/{EPISODE}.m4a", token(src))).await;
    assert_eq!(resp.status(), 410);
}

#[tokio::test]
async fn stream_responses_carry_cors_headers() {
    let addr = boot().await;
    let resp = get(addr, &format!("/stream/not*base64/{EPISODE}.mp3")).await;
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("*")
    );
}
