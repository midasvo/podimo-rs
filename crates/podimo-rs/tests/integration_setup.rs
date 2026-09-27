//! HTTP-level tests for `/setup`: status rendering for the configured
//! `LIBRARY_DIR`, the path-probe form, and the library-disabled fall-through.

use std::net::SocketAddr;
use std::time::Duration;

use podimo_rs::{app, config::Config, AppState};
use tokio::net::TcpListener;

fn base_config(library_dir: String, enable_library: bool, local_creds: bool) -> Config {
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let path = cache_dir.path().to_string_lossy().to_string();
    std::mem::forget(cache_dir);
    Config {
        hostname: "localhost:12104".into(),
        bind_host: "127.0.0.1:12104".into(),
        protocol: "http".into(),
        http_proxy: None,
        zenrows_api: None,
        scraper_api: None,
        cache_dir: path,
        block_list_file: "/dev/null".into(),
        debug: false,
        local_credentials: local_creds,
        podimo_email: if local_creds {
            Some("a@b.com".into())
        } else {
            None
        },
        podimo_password: if local_creds { Some("pw".into()) } else { None },
        podimo_region: "nl".into(),
        podimo_locale: "nl-NL".into(),
        store_tokens_on_disk: false,
        token_cache_time: 60,
        podcast_cache_time: 60,
        head_cache_time: 60,
        audiobook_audio_cache_time: 60,
        enable_library,
        library_dir,
        public_feeds: false,
        graphql_url: "https://example.invalid/graphql".into(),
        stream_format: podimo_rs::podimo::hls::StreamFormat::Mp3,
        stream_links_from_request: true,
    }
}

async fn boot(config: Config) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let state = AppState::new(config).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state).await.unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (addr, handle)
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

#[tokio::test]
async fn setup_renders_when_library_active() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle) = boot(base_config(dir.clone(), true, true)).await;

    let resp = http_client()
        .get(format!("http://{addr}/setup"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Library status"), "header missing: {body}");
    // The configured LIBRARY_DIR appears as the value; probe of a real dir
    // should report OK.
    assert!(
        body.contains("badge ok\">OK"),
        "expected OK badge for a writable temp dir: {body}"
    );
    assert!(body.contains("Active"), "expected Active badge: {body}");
    handle.abort();
}

#[tokio::test]
async fn setup_renders_when_library_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle) = boot(base_config(dir, false, false)).await;

    let resp = http_client()
        .get(format!("http://{addr}/setup"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "/setup must work even when library is off"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("Library status"));
    // With both enable_library=false and local_credentials=false, the Setup
    // page shows the "Off" badge and the LOCAL_CREDENTIALS-required warning
    // is not surfaced (because enable_library is off, not because of the
    // creds-mismatch path).
    assert!(body.contains("Off"), "expected Off badge: {body}");
    handle.abort();
}

#[tokio::test]
async fn setup_flags_local_credentials_mismatch() {
    // ENABLE_LIBRARY=true but LOCAL_CREDENTIALS=false: AppState skips library
    // construction (with a warning log). /setup should explain.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle) = boot(base_config(dir, true, false)).await;

    let resp = http_client()
        .get(format!("http://{addr}/setup"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("Library is gated"),
        "expected mismatch warning: {body}"
    );
    handle.abort();
}

#[tokio::test]
async fn test_path_probe_ok_on_writable_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    // Use a separate probe target to avoid interfering with the library dir
    // hydration.
    let probe = tempfile::tempdir().unwrap();
    let probe_path = probe.path().to_string_lossy().to_string();
    std::mem::forget(probe);

    let (addr, handle) = boot(base_config(dir, true, true)).await;
    let resp = http_client()
        .post(format!("http://{addr}/setup/test-path"))
        .form(&[("path", probe_path.as_str())])
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    // The probe result section should appear with OK.
    assert!(
        body.contains("Probe result"),
        "probe result missing: {body}"
    );
    assert!(
        body.contains("badge ok\">OK"),
        "probe should pass on a writable tempdir: {body}"
    );
    handle.abort();
}

#[tokio::test]
async fn test_path_probe_fails_on_nonexistent_path() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle) = boot(base_config(dir, true, true)).await;

    let resp = http_client()
        .post(format!("http://{addr}/setup/test-path"))
        .form(&[("path", "/no/such/path/here")])
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("Probe result"));
    assert!(
        body.contains("Doesn&#x27;t exist") || body.contains("Doesn't exist"),
        "probe should report nonexistent path: {body}"
    );
    handle.abort();
}

#[tokio::test]
async fn test_path_probe_with_empty_input_just_re_renders() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle) = boot(base_config(dir, true, true)).await;

    let resp = http_client()
        .post(format!("http://{addr}/setup/test-path"))
        .form(&[("path", "")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    // No probe-result section when nothing was submitted.
    assert!(!body.contains("Probe result"));
    handle.abort();
}

#[tokio::test]
async fn test_path_probe_returns_404_without_local_credentials() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let probe = tempfile::tempdir().unwrap();
    let probe_path = probe.path().to_string_lossy().to_string();

    let (addr, handle) = boot(base_config(dir, false, false)).await;
    let resp = http_client()
        .post(format!("http://{addr}/setup/test-path"))
        .form(&[("path", probe_path.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        404,
        "path probing should return 404 when LOCAL_CREDENTIALS=false"
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("LOCAL_CREDENTIALS=true"),
        "error message should mention LOCAL_CREDENTIALS: {body}"
    );
    // Verify that no probe file was created
    assert!(
        !probe
            .path()
            .read_dir()
            .unwrap()
            .any(|e| e.is_ok_and(|f| f.file_name().to_string_lossy().contains("podimo-rs-probe"))),
        "no probe file should have been created"
    );
    std::mem::forget(probe);
    handle.abort();
}

#[tokio::test]
async fn test_path_probe_returns_404_with_enable_library_but_no_local_credentials() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let probe = tempfile::tempdir().unwrap();
    let probe_path = probe.path().to_string_lossy().to_string();
    std::mem::forget(probe);

    // enable_library=true, local_credentials=false
    let (addr, handle) = boot(base_config(dir, true, false)).await;
    let resp = http_client()
        .post(format!("http://{addr}/setup/test-path"))
        .form(&[("path", probe_path.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        404,
        "path probing should return 404 even when ENABLE_LIBRARY=true but LOCAL_CREDENTIALS=false"
    );
    handle.abort();
}

#[tokio::test]
async fn setup_page_hides_probe_section_without_local_credentials() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle) = boot(base_config(dir, false, false)).await;

    let resp = http_client()
        .get(format!("http://{addr}/setup"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "/setup must render even without LOCAL_CREDENTIALS"
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("Library status"),
        "Library status section should be present: {body}"
    );
    assert!(
        !body.contains("Probe another path"),
        "/setup should not show probe section without LOCAL_CREDENTIALS: {body}"
    );
    handle.abort();
}
