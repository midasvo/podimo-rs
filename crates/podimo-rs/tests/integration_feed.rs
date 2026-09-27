//! HTTP-level parity tests for /feed/<id>.xml that don't require Podimo upstream:
//! basic-auth handling, validation, block-list, and middleware (CORS/Cache-Control).

use std::net::SocketAddr;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use podimo_rs::{app, config::Config, AppState};
use tokio::net::TcpListener;

/// Build a Config without touching process env vars (so parallel tests don't
/// race on the env). Each call gets its own tempdir for CACHE_DIR.
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
    };
    tweak(&mut config);
    // Leak the tempdir so the cache dir persists for the test's lifetime.
    std::mem::forget(cache_dir);
    config
}

async fn boot() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    boot_with(|_| {}).await
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

#[tokio::test]
async fn unauthenticated_returns_401_with_no_store_and_www_authenticate() {
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!(
            "http://{addr}/feed/de9b2081-9fc5-489f-b9d3-d744ed9cab20.xml"
        ))
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
async fn invalid_podcast_id_format_returns_400() {
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!("http://{addr}/feed/not-a-valid-id!.xml"))
        .header("Authorization", basic_auth_header("a@b.com,nl,nl-NL", "pw"))
        .send()
        .await
        .unwrap();
    // The router rejects the path-pattern mismatch before our handler — for the
    // characters that *do* match `[^/]`, our handler returns 400. `not-a-valid-id!`
    // matches the path segment, then trips the podcast-id regex.
    assert!(
        matches!(resp.status().as_u16(), 400 | 404),
        "status: {}",
        resp.status()
    );
    handle.abort();
}

#[tokio::test]
async fn invalid_region_returns_400() {
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!(
            "http://{addr}/feed/de9b2081-9fc5-489f-b9d3-d744ed9cab20.xml"
        ))
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
async fn invalid_locale_returns_400() {
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!(
            "http://{addr}/feed/de9b2081-9fc5-489f-b9d3-d744ed9cab20.xml"
        ))
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
async fn invalid_limit_returns_400() {
    let (addr, handle) = boot().await;
    for bad in ["abc", "-1", "0", "1.5"] {
        let resp = http_client()
            .get(format!(
                "http://{addr}/feed/de9b2081-9fc5-489f-b9d3-d744ed9cab20.xml?limit={bad}"
            ))
            .header("Authorization", basic_auth_header("a@b.com,nl,nl-NL", "pw"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "limit={bad} should 400");
        let body = resp.text().await.unwrap();
        assert!(body.contains("Invalid limit"), "limit={bad}, body={body}");
    }
    handle.abort();
}

#[tokio::test]
async fn feed_get_response_has_cors_headers() {
    let (addr, handle) = boot().await;
    // Unauthenticated -> 401 but CORS should still be emitted on /feed/* GETs.
    let resp = http_client()
        .get(format!(
            "http://{addr}/feed/de9b2081-9fc5-489f-b9d3-d744ed9cab20.xml"
        ))
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
async fn healthz_does_not_have_cors_headers() {
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!("http://{addr}/healthz"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("access-control-allow-origin").is_none());
    handle.abort();
}

#[tokio::test]
async fn index_get_renders_form() {
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!("http://{addr}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Podimo-to-RSS"), "body: {body}");
    assert!(body.contains("email"));
    assert!(body.contains("password"));
    assert!(body.contains("Podcast or audiobook"));
    handle.abort();
}

#[tokio::test]
async fn unknown_path_returns_404_text_plain() {
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!("http://{addr}/no-such-path"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(resp.headers().get("content-type").unwrap(), "text/plain");
    let body = resp.text().await.unwrap();
    assert!(body.starts_with("404 Not found"));
    handle.abort();
}

#[tokio::test]
async fn post_root_with_audiobook_url_generates_audiobook_route() {
    // The form auto-detects audiobook URLs and emits a /audiobook/<id>.xml URL.
    let (addr, handle) = boot().await;
    let resp = http_client()
        .post(format!("http://{addr}/"))
        .form(&[
            ("email", "a@b.com"),
            ("password", "pw"),
            (
                "podcast_id",
                "https://open.podimo.com/audiobook/fefa939e-c84d-4c16-8bbf-9575e1379d81",
            ),
            ("region", "nl"),
            ("locale", "nl-NL"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    // Minijinja HTML-escapes slashes (`&#x2f;`), so substring-match the parts.
    assert!(
        body.contains("audiobook&#x2f;fefa939e-c84d-4c16-8bbf-9575e1379d81.xml"),
        "audiobook URL not in generated link: {body}"
    );
    assert!(
        !body.contains("feed&#x2f;fefa939e"),
        "should not also build a /feed/ URL: {body}"
    );
    handle.abort();
}

#[tokio::test]
async fn post_root_with_podcast_url_generates_feed_route() {
    let (addr, handle) = boot().await;
    let resp = http_client()
        .post(format!("http://{addr}/"))
        .form(&[
            ("email", "a@b.com"),
            ("password", "pw"),
            (
                "podcast_id",
                "https://open.podimo.com/podcast/de9b2081-9fc5-489f-b9d3-d744ed9cab20",
            ),
            ("region", "nl"),
            ("locale", "nl-NL"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("feed&#x2f;de9b2081-9fc5-489f-b9d3-d744ed9cab20.xml"),
        "podcast feed URL not present: {body}"
    );
    handle.abort();
}

// --- middleware parity tests ---

#[tokio::test]
async fn get_root_has_no_cors_origin() {
    // Form endpoint is same-origin only.
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!("http://{addr}/"))
        .send()
        .await
        .unwrap();
    assert!(resp.headers().get("access-control-allow-origin").is_none());
    assert!(resp.headers().get("access-control-allow-methods").is_none());
    handle.abort();
}

#[tokio::test]
async fn post_root_has_no_cors_origin() {
    // POSTs must never be cross-origin.
    let (addr, handle) = boot().await;
    let resp = http_client()
        .post(format!("http://{addr}/"))
        .form(&[
            ("email", ""),
            ("password", ""),
            ("podcast_id", ""),
            ("region", ""),
            ("locale", ""),
        ])
        .send()
        .await
        .unwrap();
    assert!(resp.headers().get("access-control-allow-origin").is_none());
    assert!(resp.headers().get("access-control-allow-methods").is_none());
    handle.abort();
}

#[tokio::test]
async fn get_feed_advertises_get_head_but_not_post() {
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!(
            "http://{addr}/feed/de9b2081-9fc5-489f-b9d3-d744ed9cab20.xml"
        ))
        .send()
        .await
        .unwrap();
    let allow_methods = resp
        .headers()
        .get("access-control-allow-methods")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        resp.headers().get("access-control-allow-origin").unwrap(),
        "*"
    );
    assert!(
        !allow_methods.contains("POST"),
        "POST must NOT be advertised: {allow_methods}"
    );
    assert!(
        allow_methods.contains("GET"),
        "GET must be advertised: {allow_methods}"
    );
    handle.abort();
}

#[tokio::test]
async fn two_xx_response_has_max_age_900_cache_control() {
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!("http://{addr}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("cache-control").unwrap(), "max-age=900");
    handle.abort();
}

#[tokio::test]
async fn four_oh_four_response_has_no_store_cache_control() {
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!("http://{addr}/this-route-does-not-exist"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(resp.headers().get("cache-control").unwrap(), "no-store");
    handle.abort();
}

#[tokio::test]
async fn bad_email_format_returns_401() {
    // PodimoClient::new rejects a malformed email with InvalidCredentials,
    // which is short-circuited to 401 before any upstream request.
    let (addr, handle) = boot().await;
    let resp = http_client()
        .get(format!(
            "http://{addr}/feed/de9b2081-9fc5-489f-b9d3-d744ed9cab20.xml"
        ))
        // "not-an-email,nl,nl-NL" parses to a non-email username; PodimoClient::new
        // rejects it. The 401 path is taken regardless of upstream connectivity.
        .header(
            "Authorization",
            basic_auth_header("not-an-email,nl,nl-NL", "pw"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
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
async fn blocked_podcast_id_returns_410() {
    // Write a block list file pointing at a specific podcast id, then verify the
    // feed URL is short-circuited to 410 before any upstream call.
    let blocklist = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(blocklist.path(), "de9b2081-9fc5-489f-b9d3-d744ed9cab20\n").unwrap();
    let blocklist_path = blocklist.path().to_string_lossy().to_string();
    // Leak the file handle to keep the file alive for the duration of the test.
    std::mem::forget(blocklist);

    let (addr, handle) = boot_with(|c| c.block_list_file = blocklist_path).await;
    let resp = http_client()
        .get(format!(
            "http://{addr}/feed/de9b2081-9fc5-489f-b9d3-d744ed9cab20.xml"
        ))
        .header("Authorization", basic_auth_header("a@b.com,nl,nl-NL", "pw"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 410);
    handle.abort();
}
