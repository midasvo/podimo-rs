//! HTTP-level tests for the audiobook library. Library is single-user, so all
//! tests boot with `LOCAL_CREDENTIALS=true` + `ENABLE_LIBRARY=true`. The book
//! list is seeded directly via `AppState.library` to keep the tests offline —
//! no Podimo upstream is exercised here.

use std::net::SocketAddr;
use std::time::Duration;

use podimo_rs::library::{LibraryEntry, Status};
use podimo_rs::{app, config::Config, AppState};
use tokio::net::TcpListener;

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
    }
}

async fn boot_with_library_dir(dir: String) -> (SocketAddr, tokio::task::JoinHandle<()>, AppState) {
    let config = make_test_config(dir);
    let state = AppState::new(config).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state.clone()).await.unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (addr, handle, state)
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
