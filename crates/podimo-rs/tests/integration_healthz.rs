//! Boots the service on an ephemeral port and hits /healthz over real HTTP.

use std::net::SocketAddr;
use std::time::Duration;

use podimo_rs::{app, config::Config, AppState};
use tokio::net::TcpListener;

#[tokio::test]
async fn healthz_returns_200_json() {
    // Construct Config directly so parallel tests don't race on env vars.
    let cache_dir = tempfile::tempdir().unwrap();
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
    let state = AppState::new(config).await.expect("state");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");

    let router = app(state).await.expect("build app");
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    std::mem::forget(cache_dir);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    let url = format!("http://{addr}/healthz");
    let resp = client.get(&url).send().await.expect("request");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(
        resp.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    assert_eq!(
        resp.headers().get(reqwest::header::CACHE_CONTROL).unwrap(),
        "no-store"
    );

    let body = resp.text().await.expect("body");
    assert_eq!(body, r#"{"status":"ok"}"#);

    handle.abort();
}
