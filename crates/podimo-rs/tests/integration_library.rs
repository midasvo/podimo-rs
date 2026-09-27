//! HTTP-level tests for the audiobook library. Library is single-user, so all
//! tests boot with `LOCAL_CREDENTIALS=true` + `ENABLE_LIBRARY=true`. The book
//! list is seeded directly via `AppState.library` to keep the tests offline;
//! the few that need Podimo's API get a wiremock stand-in.

use std::net::SocketAddr;
use std::time::Duration;

use podimo_rs::library::{LibraryEntry, Status};
use podimo_rs::{app, config::Config, AppState};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_test_config(library_dir: String) -> Config {
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
        local_credentials: true,
        podimo_email: Some("a@b.com".into()),
        podimo_password: Some("pw".into()),
        podimo_region: "nl".into(),
        podimo_locale: "nl-NL".into(),
        store_tokens_on_disk: false,
        token_cache_time: 60,
        podcast_cache_time: 60,
        head_cache_time: 60,
        audiobook_audio_cache_time: 60,
        enable_library: true,
        library_dir,
        public_feeds: false,
        graphql_url: "https://example.invalid/graphql".into(),
        stream_format: podimo_rs::podimo::hls::StreamFormat::Mp3,
        stream_links_from_request: true,
    }
}

/// Same as `make_test_config` but with `graphql_url` overridden — used by the
/// retry happy-path test, which points it at a wiremock server. Kept as a
/// separate function so every other test's config is unaffected.
fn make_test_config_with_graphql_url(library_dir: String, graphql_url: String) -> Config {
    let mut config = make_test_config(library_dir);
    config.graphql_url = graphql_url;
    config
}

async fn boot_with_config(config: Config) -> (SocketAddr, tokio::task::JoinHandle<()>, AppState) {
    let state = AppState::new(config).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state.clone()).await.unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (addr, handle, state)
}

async fn boot_with_library_dir(dir: String) -> (SocketAddr, tokio::task::JoinHandle<()>, AppState) {
    boot_with_config(make_test_config(dir)).await
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        // Library uses 303-style redirects on POSTs (Redirect::to); follow them
        // so we land on /library and can assert the rendered page.
        .build()
        .unwrap()
}

fn sample(id: &str, status: Status) -> LibraryEntry {
    LibraryEntry {
        id: id.into(),
        title: format!("Book {id}"),
        author: "An Author".into(),
        narrators: "A Narrator".into(),
        description: "A test description.".into(),
        duration_seconds: 7200,
        publisher: Some("A Publisher".into()),
        year: Some(2024),
        added_at: "2026-05-14T10:00:00Z".into(),
        status,
        error: None,
        audio_size_bytes: Some(1_048_576),
        audio_downloaded_bytes: 0,
    }
}

#[tokio::test]
async fn library_disabled_returns_404() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = make_test_config(tmp.path().to_string_lossy().to_string());
    config.enable_library = false;
    std::mem::forget(tmp);
    let state = AppState::new(config).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state).await.unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let resp = http_client()
        .get(format!("http://{addr}/library"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    handle.abort();
}

#[tokio::test]
async fn empty_library_renders_with_empty_marker() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle, _state) = boot_with_library_dir(dir).await;
    let resp = http_client()
        .get(format!("http://{addr}/library"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Nothing here yet"), "body: {body}");
    handle.abort();
}

#[tokio::test]
async fn seeded_entry_appears_in_overview() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle, state) = boot_with_library_dir(dir).await;

    let library = state.library.as_ref().unwrap();
    library
        .add(sample("aaaa1111-2222-3333-4444-555566667777", Status::Done))
        .await
        .unwrap();

    let resp = http_client()
        .get(format!("http://{addr}/library"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Book aaaa1111"), "title missing: {body}");
    assert!(body.contains("An Author"), "author missing");
    assert!(body.contains("A Narrator"), "narrator missing");
    // Done = download link is present.
    assert!(
        body.contains("/audio.mp3"),
        "download link missing for Done entry: {body}"
    );
    handle.abort();
}

#[tokio::test]
async fn downloading_entry_renders_progress() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle, state) = boot_with_library_dir(dir).await;

    let library = state.library.as_ref().unwrap();
    let mut e = sample("bbbb2222-3333-4444-5555-666677778888", Status::Downloading);
    e.audio_downloaded_bytes = 512_000;
    library.add(e).await.unwrap();

    let resp = http_client()
        .get(format!("http://{addr}/library"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("badge downloading"),
        "downloading badge missing: {body}"
    );
    // Progress percent should appear, somewhere between 1 and 99.
    assert!(
        body.contains("class=\"progress\""),
        "progress bar missing: {body}"
    );
    handle.abort();
}

#[tokio::test]
async fn audio_returns_409_when_not_done() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle, state) = boot_with_library_dir(dir).await;

    let library = state.library.as_ref().unwrap();
    library
        .add(sample(
            "cccc3333-4444-5555-6666-777788889999",
            Status::Queued,
        ))
        .await
        .unwrap();

    let resp = http_client()
        .get(format!(
            "http://{addr}/library/cccc3333-4444-5555-6666-777788889999/audio.mp3"
        ))
        .send()
        .await
        .unwrap();
    // 409 Conflict — the file isn't ready yet.
    assert_eq!(resp.status(), 409);
    handle.abort();
}

#[tokio::test]
async fn audio_serves_done_file_with_attachment_header() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle, state) = boot_with_library_dir(dir).await;

    let library = state.library.as_ref().unwrap();
    let id = "dddd4444-5555-6666-7777-888899990000";
    let entry = sample(id, Status::Done);
    library.add(entry.clone()).await.unwrap();

    // Manually write the audio file the handler will serve.
    std::fs::write(library.audio_path(&entry), b"FAKE-MP3-BYTES").unwrap();

    let resp = http_client()
        .get(format!("http://{addr}/library/{id}/audio.mp3"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-type").unwrap(), "audio/mpeg");
    let disp = resp
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        disp.starts_with("attachment;"),
        "Content-Disposition: {disp}"
    );
    assert!(disp.contains("Book dddd4444"), "filename: {disp}");
    // Streaming response advertises a Content-Length so the browser shows a
    // real progress bar instead of an indeterminate spinner.
    assert_eq!(
        resp.headers().get("content-length").unwrap(),
        &b"FAKE-MP3-BYTES".len().to_string()
    );
    let body = resp.bytes().await.unwrap();
    assert_eq!(&body[..], b"FAKE-MP3-BYTES");
    handle.abort();
}

#[tokio::test]
async fn cover_returns_404_when_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle, state) = boot_with_library_dir(dir).await;

    let library = state.library.as_ref().unwrap();
    let id = "eeee5555-6666-7777-8888-999900001111";
    library.add(sample(id, Status::Queued)).await.unwrap();

    let resp = http_client()
        .get(format!("http://{addr}/library/{id}/cover.jpg"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    handle.abort();
}

#[tokio::test]
async fn remove_drops_entry_and_redirects() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle, state) = boot_with_library_dir(dir).await;

    let library = state.library.as_ref().unwrap();
    let id = "ffff6666-7777-8888-9999-000011112222";
    library.add(sample(id, Status::Done)).await.unwrap();
    assert!(library.contains(id).await);

    let resp = http_client()
        .post(format!("http://{addr}/library/{id}/remove"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "redirect should resolve to /library");
    let body = resp.text().await.unwrap();
    // After redirect we land on the library page — entry should be gone from
    // the rendered HTML.
    assert!(!body.contains(&format!("Book {id}")), "entry still listed");
    assert!(!library.contains(id).await, "entry should be removed");
    handle.abort();
}

#[tokio::test]
async fn add_with_podcast_url_shows_error() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle, _state) = boot_with_library_dir(dir).await;

    let resp = http_client()
        .post(format!("http://{addr}/library/add"))
        .form(&[(
            "url_or_id",
            "https://open.podimo.com/podcast/de9b2081-9fc5-489f-b9d3-d744ed9cab20",
        )])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("only stores audiobooks"),
        "expected podcast-rejection error: {body}"
    );
    handle.abort();
}

#[tokio::test]
async fn forget_leaves_audio_and_metadata_on_disk() {
    // End-to-end safety: the HTTP /library/<id>/remove endpoint must never
    // delete the audio file, cover, or ABS metadata. Only the podimo-rs
    // state marker should go.
    let tmp = tempfile::tempdir().unwrap();
    let dir_str = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle, state) = boot_with_library_dir(dir_str.clone()).await;

    let library = state.library.as_ref().unwrap();
    let id = "aabbccdd-1111-2222-3333-444455556666";
    let entry = sample(id, Status::Done);
    library.add(entry.clone()).await.unwrap();
    let book_dir = library.entry_dir(&entry);
    // Plant audio + cover (download would do this in real flow).
    std::fs::write(library.audio_path(&entry), b"AUDIO").unwrap();
    std::fs::write(library.cover_path(&entry), b"COVER").unwrap();

    let resp = http_client()
        .post(format!("http://{addr}/library/{id}/remove"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    assert!(
        !library.contains(id).await,
        "entry should be forgotten from memory"
    );
    assert!(
        !library.state_path(&entry).exists(),
        "state file should be deleted"
    );
    // Everything else stays. This is the entire point of "Forget".
    assert!(book_dir.exists(), "book dir must remain");
    assert!(library.audio_path(&entry).exists(), "audio must remain");
    assert!(library.cover_path(&entry).exists(), "cover must remain");
    assert!(
        book_dir.join("metadata.json").exists(),
        "ABS metadata must remain"
    );
    handle.abort();
}

#[tokio::test]
async fn add_refuses_pre_existing_dir() {
    // End-to-end safety: hitting `/library/add` when the target Author/Title/
    // dir already exists (e.g. user has an existing ABS book at the same
    // path) must surface an error and write nothing.
    let tmp = tempfile::tempdir().unwrap();
    let dir_str = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle, state) = boot_with_library_dir(dir_str.clone()).await;

    // Pre-create an "existing book" at the target path. `sample()`'s entry
    // would land at `Book aaaa1111-.../`-style title path, but the
    // `/library/add` flow uses the metadata Podimo returns. Since this test
    // can't hit the real Podimo, it instead seeds the library directly and
    // then attempts to re-add via the in-memory API.
    let library = state.library.as_ref().unwrap();
    let id = "abcd1234-5678-90ab-cdef-1234567890ab";
    let mut entry = sample(id, Status::Queued);
    entry.title = "Some Real Book".into();
    entry.author = "Some Real Author".into();
    // Plant a foreign book at the path podimo-rs would want to use.
    let foreign_dir = std::path::Path::new(&dir_str)
        .join("Some Real Author")
        .join("Some Real Book");
    std::fs::create_dir_all(&foreign_dir).unwrap();
    std::fs::write(foreign_dir.join("Their Book.mp3"), b"NOT MINE").unwrap();

    // Direct add via in-memory API to verify the safety check.
    let err = library.add(entry).await.unwrap_err();
    assert!(
        err.to_string().contains("not managed by podimo-rs"),
        "expected foreign-content refusal: {err}"
    );
    // Foreign content should be untouched.
    assert!(foreign_dir.join("Their Book.mp3").exists());
    assert!(!foreign_dir.join("podimo-state.json").exists());
    assert!(!foreign_dir.join("metadata.json").exists());

    // sanity: /library should still render an empty list because the add
    // failed.
    let resp = http_client()
        .get(format!("http://{addr}/library"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("Nothing here yet"),
        "library should be empty: {body}"
    );
    handle.abort();
}

#[tokio::test]
async fn add_with_garbage_input_shows_error() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle, _state) = boot_with_library_dir(dir).await;

    let resp = http_client()
        .post(format!("http://{addr}/library/add"))
        .form(&[("url_or_id", "not a url and not an id")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    // Apostrophe is HTML-escaped to `&#x27;` by minijinja autoescape.
    assert!(
        body.contains("find a UUID in that input"),
        "expected uuid-not-found error: {body}"
    );
    handle.abort();
}

#[tokio::test]
async fn index_shows_library_link_when_enabled() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle, _state) = boot_with_library_dir(dir).await;

    let resp = http_client()
        .get(format!("http://{addr}/"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("audiobook library"),
        "library link missing from index: {body}"
    );
    handle.abort();
}

/// Mock dispatcher for the retry happy-path test: routes `POST /graphql` by
/// GraphQL operation name, mirroring the pattern in
/// `tests/integration_audiobook_upstream.rs`. Login succeeds, metadata
/// returns cover art hosted on this same mock server, and the short-lived
/// audio query returns an audio URL hosted here too, so nothing in this test
/// reaches the real Podimo.
async fn install_retry_upstream_mock(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/audio.mp3"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"audio-bytes".to_vec()))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/cover.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"cover-bytes".to_vec()))
        .mount(server)
        .await;

    let cover_url = format!("{}/cover.jpg", server.uri());
    let audio_url = format!("{}/audio.mp3", server.uri());
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let query = body.get("query").and_then(|q| q.as_str()).unwrap_or("");
            if query.contains("AuthorizationPreregisterUser") {
                ResponseTemplate::new(200).set_body_json(json!({
                    "data": { "tokenWithPreregisterUser": { "token": "preauth-token" } }
                }))
            } else if query.contains("OnboardingQuery") {
                ResponseTemplate::new(200).set_body_json(json!({
                    "data": { "userOnboardingFlow": { "id": "onboarding-id" } }
                }))
            } else if query.contains("AuthorizationAuthorize") {
                ResponseTemplate::new(200).set_body_json(json!({
                    "data": { "tokenWithCredentials": { "token": "user-token" } }
                }))
            } else if query.contains("AudiobookResultsQuery") {
                ResponseTemplate::new(200).set_body_json(json!({
                    "data": {
                        "audiobookById": {
                            "id": "22223333-4444-5555-6666-777788889999",
                            "title": "Retried Book",
                            "authorNames": "An Author",
                            "description": "A retried description.",
                            "duration": 3600,
                            "publisherName": "A Publisher",
                            "yearOfBookPublication": 2024,
                            "authors": [{"name": "An Author"}],
                            "narrators": [{"name": "A Narrator"}],
                            "coverImage": { "url": cover_url.clone() },
                            "language": { "isoLanguage": "nl" }
                        }
                    }
                }))
            } else if query.contains("ShortLivedAudiobookMediaUrlQuery") {
                ResponseTemplate::new(200).set_body_json(json!({
                    "data": { "audiobookAudioById": { "url": audio_url.clone() } }
                }))
            } else {
                ResponseTemplate::new(500).set_body_string("unexpected graphql query")
            }
        })
        .mount(server)
        .await;
}

#[tokio::test]
async fn retry_failed_download_completes_against_mock_upstream() {
    let server = MockServer::start().await;
    install_retry_upstream_mock(&server).await;

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let config = make_test_config_with_graphql_url(dir, format!("{}/graphql", server.uri()));
    let (addr, handle, state) = boot_with_config(config).await;

    let library = state.library.as_ref().unwrap();
    let id = "22223333-4444-5555-6666-777788889999";
    library.add(sample(id, Status::Failed)).await.unwrap();

    let resp = http_client()
        .post(format!("http://{addr}/library/{id}/retry"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "redirect should resolve to /library");

    // The download runs in a spawned background task; poll until it settles.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let entry = loop {
        let entry = library.get(id).await.unwrap();
        if !matches!(entry.status, Status::Queued | Status::Downloading) {
            break entry;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("retry did not finish within 5s (last seen: {entry:?})");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    assert_eq!(entry.status, Status::Done, "unexpected status: {entry:?}");
    assert!(
        library.audio_path(&entry).exists(),
        "audio file should exist after a successful retry"
    );
    handle.abort();
}

#[tokio::test]
async fn retry_done_entry_is_a_noop_and_redirects() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle, state) = boot_with_library_dir(dir).await;

    let library = state.library.as_ref().unwrap();
    let id = "33334444-5555-6666-7777-888899990000";
    library.add(sample(id, Status::Done)).await.unwrap();

    let resp = http_client()
        .post(format!("http://{addr}/library/{id}/retry"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "redirect should resolve to /library");

    let entry = library.get(id).await.unwrap();
    assert_eq!(entry.status, Status::Done, "status must be unchanged");
    handle.abort();
}

#[tokio::test]
async fn retry_unknown_id_returns_404() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle, _state) = boot_with_library_dir(dir).await;

    let resp = http_client()
        .post(format!(
            "http://{addr}/library/44445555-6666-7777-8888-999900001111/retry"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    handle.abort();
}

#[tokio::test]
async fn retry_invalid_id_returns_400() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle, _state) = boot_with_library_dir(dir).await;

    let resp = http_client()
        .post(format!("http://{addr}/library/not-a-valid-id/retry"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    handle.abort();
}

#[tokio::test]
async fn retry_button_shown_only_for_failed_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let (addr, handle, state) = boot_with_library_dir(dir).await;

    let library = state.library.as_ref().unwrap();
    let failed_id = "55556666-7777-8888-9999-000011112222";
    let done_id = "66667777-8888-9999-0000-111122223333";
    library
        .add(sample(failed_id, Status::Failed))
        .await
        .unwrap();
    library.add(sample(done_id, Status::Done)).await.unwrap();

    let resp = http_client()
        .get(format!("http://{addr}/library"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains(&format!("/library/{failed_id}/retry")),
        "retry form missing for failed entry: {body}"
    );
    assert!(
        !body.contains(&format!("/library/{done_id}/retry")),
        "retry form should not appear for a done entry: {body}"
    );
    handle.abort();
}

#[tokio::test]
async fn add_drops_a_cached_token_upstream_rejects() {
    // Upstream rejects every query, as it would a revoked token. The cached
    // token must go, so the next attempt logs in again instead of failing
    // until the token's TTL runs out.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "errors": [{ "message": "Unauthorized" }]
        })))
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let config = make_test_config_with_graphql_url(dir, format!("{}/graphql", server.uri()));
    let (addr, handle, state) = boot_with_config(config).await;
    // What `util::token_key` computes for the configured a@b.com / pw.
    let key = hex::encode(Sha256::digest(b"a@b.com~pw"));
    state
        .caches
        .tokens
        .insert(key.clone(), "stale-token".into())
        .await;

    let resp = http_client()
        .post(format!("http://{addr}/library/add"))
        .form(&[(
            "url_or_id",
            "https://open.podimo.com/audiobook/fefa939e-c84d-4c16-8bbf-9575e1379d81",
        )])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp
        .text()
        .await
        .unwrap()
        .contains("fetch audiobook metadata"));
    assert_eq!(state.caches.tokens.get(&key).await, None);
    assert!(state.library.as_ref().unwrap().list().await.is_empty());
    handle.abort();
}

#[tokio::test]
async fn add_takes_a_bare_uuid_as_an_audiobook() {
    // The home page reads a bare UUID as a podcast; the library page asks for
    // an "Audiobook URL or UUID", so there it must mean an audiobook. The
    // mock fails every call, so the flow stops at login.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().to_string();
    std::mem::forget(tmp);
    let config = make_test_config_with_graphql_url(dir, format!("{}/graphql", server.uri()));
    let (addr, handle, _state) = boot_with_config(config).await;

    let resp = http_client()
        .post(format!("http://{addr}/library/add"))
        .form(&[("url_or_id", " fefa939e-c84d-4c16-8bbf-9575e1379d81 ")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(!body.contains("looks like a podcast"), "{body}");
    assert!(body.contains("Login failed"), "{body}");
    handle.abort();
}
