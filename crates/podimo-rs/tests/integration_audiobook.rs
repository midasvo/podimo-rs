//! HTTP-level parity tests for /audiobook/<id>.xml — validation, auth, blocklist,
//! and middleware (CORS/Cache-Control). Mirrors `integration_feed.rs` so the
//! audiobook endpoint stays in lockstep with the podcast endpoint.

use std::net::SocketAddr;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use podimo_rs::{app, config::Config, AppState};
use tokio::net::TcpListener;

fn make_test_config<F: FnOnce(&mut Config)>(tweak: F) -> Config {
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config {
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
    tweak(&mut config);
    std::mem::forget(cache_dir);
    config
}

async fn boot_with<F: FnOnce(&mut Config)>(tweak: F) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let config = make_test_config(tweak);
    let state = AppState::new(config).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state).await.unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (addr, handle)
}

async fn boot() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    boot_with(|_| {}).await
}

fn basic_auth_header(username: &str, password: &str) -> String {
    let raw = format!("{username}:{password}");
    format!("Basic {}", BASE64.encode(raw))
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

const AUDIOBOOK_ID: &str = "fefa939e-c84d-4c16-8bbf-9575e1379d81";

#[tokio::test]
async fn unauthenticated_audiobook_returns_401() {
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!("http://{addr}/audiobook/{AUDIOBOOK_ID}.xml"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert_eq!(resp.headers().get("cache-control").unwrap(), "no-store");
    assert!(resp
        .headers()
        .get("www-authenticate")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("Basic realm="));
    handle.abort();
}

#[tokio::test]
async fn invalid_audiobook_id_returns_400() {
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!("http://{addr}/audiobook/not-a-valid-id!.xml"))
        .header("Authorization", basic_auth_header("a@b.com,nl,nl-NL", "pw"))
        .send()
        .await
        .unwrap();
    assert!(
        matches!(resp.status().as_u16(), 400 | 404),
        "status: {}",
        resp.status()
    );
    handle.abort();
}

#[tokio::test]
async fn audiobook_invalid_region_returns_400() {
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!("http://{addr}/audiobook/{AUDIOBOOK_ID}.xml"))
        .header("Authorization", basic_auth_header("a@b.com,zz,nl-NL", "pw"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Invalid region"));
    handle.abort();
}

#[tokio::test]
async fn audiobook_invalid_locale_returns_400() {
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!("http://{addr}/audiobook/{AUDIOBOOK_ID}.xml"))
        .header("Authorization", basic_auth_header("a@b.com,nl,zz-ZZ", "pw"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Invalid locale"));
    handle.abort();
}

#[tokio::test]
async fn audiobook_get_response_has_cors_headers() {
    // CORS middleware must cover /audiobook/* exactly like /feed/*. Even 401
    // responses on these paths still get the headers attached.
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!("http://{addr}/audiobook/{AUDIOBOOK_ID}.xml"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers().get("access-control-allow-origin").unwrap(),
        "*"
    );
    assert_eq!(
        resp.headers().get("access-control-allow-methods").unwrap(),
        "GET, HEAD"
    );
    handle.abort();
}

#[tokio::test]
async fn audiobook_bad_email_returns_401() {
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!("http://{addr}/audiobook/{AUDIOBOOK_ID}.xml"))
        .header(
            "Authorization",
            basic_auth_header("not-an-email,nl,nl-NL", "pw"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    handle.abort();
}

#[tokio::test]
async fn blocked_audiobook_id_returns_410() {
    let blocklist = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(blocklist.path(), format!("{AUDIOBOOK_ID}\n")).unwrap();
    let blocklist_path = blocklist.path().to_string_lossy().to_string();
    std::mem::forget(blocklist);

    let (addr, handle) = boot_with(|c| c.block_list_file = blocklist_path).await;
    let resp = http_client()
        .get(format!("http://{addr}/audiobook/{AUDIOBOOK_ID}.xml"))
        .header("Authorization", basic_auth_header("a@b.com,nl,nl-NL", "pw"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 410);
    handle.abort();
}

#[tokio::test]
async fn audiobook_missing_xml_extension_returns_404() {
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!("http://{addr}/audiobook/{AUDIOBOOK_ID}"))
        .header("Authorization", basic_auth_header("a@b.com,nl,nl-NL", "pw"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    handle.abort();
}
