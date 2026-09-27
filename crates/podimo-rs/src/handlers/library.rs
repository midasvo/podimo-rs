//! Audiobook library endpoints (single-user, opt-in via `ENABLE_LIBRARY=true`).
//!
//! Routes:
//!   - `GET  /library`                — HTML overview of all entries.
//!   - `POST /library/add`            — add a book by URL or UUID, triggers download.
//!   - `POST /library/<id>/retry`     — retry a failed download.
//!   - `POST /library/<id>/remove`    — drop entry + on-disk files.
//!   - `GET  /library/<id>/audio.mp3` — streamed download with attachment headers.
//!   - `GET  /library/<id>/cover.jpg` — cover image.
//!
//! All routes 404 when the library is disabled. The single-user constraint
//! (`LOCAL_CREDENTIALS=true`) is enforced at startup in `AppState::new` — if
//! the constraint is violated the library is simply `None`.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Form, Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::io::ReaderStream;

use crate::error::AppError;
use crate::handlers::auth::Authorized;
use crate::library::{download, Library, LibraryEntry, Status};
use crate::podimo::PodimoClient;
use crate::state::AppState;
use crate::util::{parse_podimo_input, PodimoKind, PODCAST_ID_RE};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/library", get(render_index))
        .route("/library/add", post(handle_add))
        .route("/library/{id}/retry", post(handle_retry))
        .route("/library/{id}/remove", post(handle_remove))
        .route("/library/{id}/audio.mp3", get(serve_audio))
        .route("/library/{id}/cover.jpg", get(serve_cover))
}

// `Response` is a large type but here it's an alternative-path return value,
// not a hot-loop carry — `Box`ing it would obscure the call sites with no
// measurable benefit.
#[allow(clippy::result_large_err)]
fn library_or_404(state: &AppState) -> Result<&Library, Response> {
    state.library.as_ref().ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            "Library is not enabled. Set ENABLE_LIBRARY=true (requires LOCAL_CREDENTIALS=true).",
        )
            .into_response()
    })
}

#[derive(Serialize)]
struct LibraryCtx<'a> {
    error: &'a str,
    entries: Vec<LibraryRow>,
}

#[derive(Serialize)]
struct LibraryRow {
    pub id: String,
    pub title: String,
    pub author: String,
    pub narrators: String,
    pub description: String,
    pub status: &'static str,
    pub status_label: &'static str,
    pub progress_pct: u8,
    pub error: Option<String>,
    pub year: Option<i64>,
    pub publisher: Option<String>,
    pub duration_hhmmss: String,
    pub audio_size_mb: Option<String>,
    pub downloaded_mb: Option<String>,
    pub has_audio: bool,
    pub has_cover: bool,
}

impl From<LibraryEntry> for LibraryRow {
    fn from(e: LibraryEntry) -> Self {
        let progress = match (e.audio_size_bytes, &e.status) {
            (_, Status::Done) => 100,
            (Some(total), Status::Downloading) if total > 0 => {
                ((e.audio_downloaded_bytes.min(total) as f64 / total as f64) * 100.0) as u8
            }
            _ => 0,
        };
        let status_str = match e.status {
            Status::Queued => "queued",
            Status::Downloading => "downloading",
            Status::Done => "done",
            Status::Failed => "failed",
        };
        let status_label = match e.status {
            Status::Queued => "Queued",
            Status::Downloading => "Downloading",
            Status::Done => "Ready",
            Status::Failed => "Failed",
        };
        let audio_size_mb = e.audio_size_bytes.map(format_mb);
        let downloaded_mb = if e.audio_downloaded_bytes > 0 {
            Some(format_mb(e.audio_downloaded_bytes))
        } else {
            None
        };
        Self {
            id: e.id,
            title: e.title,
            author: e.author,
            narrators: e.narrators,
            description: e.description,
            status: status_str,
            status_label,
            progress_pct: progress,
            error: e.error,
            year: e.year,
            publisher: e.publisher,
            duration_hhmmss: format_hhmmss(e.duration_seconds),
            audio_size_mb,
            downloaded_mb,
            has_audio: matches!(e.status, Status::Done),
            has_cover: true,
        }
    }
}

fn format_mb(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 100.0 {
        format!("{mb:.0} MB")
    } else {
        format!("{mb:.1} MB")
    }
}

fn format_hhmmss(seconds: i64) -> String {
    if seconds <= 0 {
        return String::new();
    }
    let s = seconds as u64;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m")
    }
}

async fn render_index(State(state): State<AppState>) -> Response {
    render_index_with_error(&state, "").await
}

async fn render_index_with_error(state: &AppState, error: &str) -> Response {
    let library = match library_or_404(state) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let entries = library
        .list()
        .await
        .into_iter()
        .map(LibraryRow::from)
        .collect::<Vec<_>>();
    let ctx = LibraryCtx { error, entries };
    match state.templates.render("library.html", &ctx) {
        Ok(body) => Html(body).into_response(),
        Err(err) => {
            tracing::error!(target: "podimo::library", "template render failed: {err}");
            AppError::Internal(format!("template render: {err}")).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct AddForm {
    url_or_id: String,
}

async fn handle_add(State(state): State<AppState>, Form(form): Form<AddForm>) -> Response {
    let library = match library_or_404(&state) {
        Ok(l) => l,
        Err(r) => return r,
    };

    let input = form.url_or_id.trim();
    let audiobook_id = match parse_podimo_input(input) {
        Some((PodimoKind::Audiobook, id)) => id.to_string(),
        // A bare UUID says nothing about its kind; on this page it's an
        // audiobook. `parse_podimo_input` calls it a podcast for the home page.
        Some((PodimoKind::Podcast, id)) if id == input => id.to_string(),
        Some((PodimoKind::Podcast, _)) => {
            return render_index_with_error(
                &state,
                "That looks like a podcast URL. The library only stores audiobooks.",
            )
            .await;
        }
        None => {
            return render_index_with_error(
                &state,
                "Couldn't find a UUID in that input. Paste either a bare UUID or the full audiobook URL.",
            )
            .await;
        }
    };

    if !PODCAST_ID_RE.is_match(&audiobook_id) {
        return render_index_with_error(&state, "Invalid audiobook id format.").await;
    }

    let auth = match library_client(&state).await {
        Ok(a) => a,
        Err(err) => return render_index_with_error(&state, &err).await,
    };

    // Fetch metadata synchronously so we can populate the library row before
    // kicking off the (potentially long) audio download. Bail early on a bad
    // id rather than persisting an empty entry.
    let meta = match fetch_audiobook_meta(&state, &auth, &audiobook_id).await {
        Ok(v) => v,
        Err(err) => return render_index_with_error(&state, &err).await,
    };

    if library.contains(&audiobook_id).await {
        return render_index_with_error(&state, "That book is already in your library.").await;
    }

    let entry = match build_entry(&audiobook_id, &meta) {
        Ok(e) => e,
        Err(err) => {
            return render_index_with_error(&state, &format!("Bad metadata: {err}")).await;
        }
    };
    if let Err(err) = library.add(entry).await {
        return render_index_with_error(&state, &format!("Library add failed: {err}")).await;
    }

    let cover_url = cover_url_from_meta(&meta);

    let library_clone = library.clone();
    let scraper_clone = state.scraper.clone();
    let config_clone = Arc::clone(&state.config);
    let caches_clone = state.caches.clone();
    let id_clone = audiobook_id.clone();
    tokio::spawn(async move {
        download::run(
            library_clone,
            auth.client,
            scraper_clone,
            config_clone,
            caches_clone,
            id_clone,
            cover_url,
        )
        .await;
    });

    Redirect::to("/library").into_response()
}

/// Build a logged-in `PodimoClient` from the single-user library's env
/// credentials (`PODIMO_EMAIL` / `PODIMO_PASSWORD`), reusing a cached token
/// when we have one. Shared by `handle_add` and `handle_retry`; the error is
/// the message to show on the page.
async fn library_client(state: &AppState) -> Result<Authorized, String> {
    let email = state
        .config
        .podimo_email
        .as_deref()
        .ok_or_else(|| "PODIMO_EMAIL is unset — library cannot fetch books.".to_string())?;
    let password = state
        .config
        .podimo_password
        .as_deref()
        .ok_or_else(|| "PODIMO_PASSWORD is unset — library cannot fetch books.".to_string())?;

    let locale = &state.config.podimo_locale;
    let mut client = PodimoClient::new(email, password, &state.config.podimo_region, locale)
        .map_err(|err| format!("Credentials invalid: {err}"))?;

    let cached_token = state.caches.tokens.get(&client.key).await;
    let token_from_cache = cached_token.is_some();
    if let Some(token) = cached_token {
        client.token = Some(token);
    } else {
        match client.login(&state.scraper, &state.config).await {
            Ok(token) => state.caches.tokens.insert(client.key.clone(), token).await,
            Err(err) => return Err(format!("Login failed: {err}")),
        }
    }
    Ok(Authorized {
        client,
        locale: locale.to_string(),
        token_from_cache,
    })
}

/// Fetch `audiobookById` metadata for `audiobook_id`, translating client
/// errors into the same user-facing messages the library has always shown.
/// Shared by `handle_add` and `handle_retry`.
async fn fetch_audiobook_meta(
    state: &AppState,
    auth: &Authorized,
    audiobook_id: &str,
) -> Result<Arc<Value>, String> {
    let result = auth
        .client
        .get_audiobook(
            &state.scraper,
            &state.config,
            audiobook_id,
            &state.caches.audiobook_meta,
        )
        .await;
    match result {
        Ok(meta) => Ok(meta),
        Err(err) if err.is_not_found() => Err("Audiobook not found upstream.".to_string()),
        Err(err) => {
            auth.forget_token_after(state, &err).await;
            Err(format!("Couldn't fetch audiobook metadata: {err}"))
        }
    }
}

/// Pull `audiobookById.coverImage.url` out of a metadata payload. The signed
/// cover URL isn't persisted on the library entry, so both a fresh add and a
/// retry re-fetch metadata and re-extract it from scratch.
fn cover_url_from_meta(meta: &Value) -> Option<String> {
    meta.get("audiobookById")
        .and_then(|b| b.get("coverImage"))
        .and_then(|c| c.get("url"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

async fn handle_retry(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let library = match library_or_404(&state) {
        Ok(l) => l,
        Err(r) => return r,
    };
    if !PODCAST_ID_RE.is_match(&id) {
        return AppError::BadRequest("Invalid id".into()).into_response();
    }
    let entry = match library.get(&id).await {
        Some(e) => e,
        None => return AppError::NotFound.into_response(),
    };
    if !matches!(entry.status, Status::Failed) {
        // Already queued, downloading, or done — nothing to retry.
        return Redirect::to("/library").into_response();
    }

    let auth = match library_client(&state).await {
        Ok(a) => a,
        Err(err) => return render_index_with_error(&state, &err).await,
    };
    let meta = match fetch_audiobook_meta(&state, &auth, &id).await {
        Ok(v) => v,
        Err(err) => return render_index_with_error(&state, &err).await,
    };
    let cover_url = cover_url_from_meta(&meta);

    // Flip Failed -> Queued atomically so two clicks can't start two
    // downloads writing into the same `.partial` file.
    let mut restarted = false;
    if let Err(err) = library
        .update(&id, |e| {
            if e.status == Status::Failed {
                e.status = Status::Queued;
                e.error = None;
                e.audio_downloaded_bytes = 0;
                restarted = true;
            }
        })
        .await
    {
        return render_index_with_error(&state, &format!("Library update failed: {err}")).await;
    }

    if restarted {
        let library_clone = library.clone();
        let scraper_clone = state.scraper.clone();
        let config_clone = Arc::clone(&state.config);
        let caches_clone = state.caches.clone();
        let id_clone = id.clone();
        tokio::spawn(async move {
            download::run(
                library_clone,
                auth.client,
                scraper_clone,
                config_clone,
                caches_clone,
                id_clone,
                cover_url,
            )
            .await;
        });
    }

    Redirect::to("/library").into_response()
}

async fn handle_remove(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let library = match library_or_404(&state) {
        Ok(l) => l,
        Err(r) => return r,
    };
    if !PODCAST_ID_RE.is_match(&id) {
        return AppError::BadRequest("Invalid id".into()).into_response();
    }
    match library.remove(&id).await {
        Ok(true) => Redirect::to("/library").into_response(),
        Ok(false) => AppError::NotFound.into_response(),
        Err(err) => AppError::Internal(format!("remove: {err}")).into_response(),
    }
}

async fn serve_audio(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let library = match library_or_404(&state) {
        Ok(l) => l,
        Err(r) => return r,
    };
    if !PODCAST_ID_RE.is_match(&id) {
        return AppError::BadRequest("Invalid id".into()).into_response();
    }
    let entry = match library.get(&id).await {
        Some(e) => e,
        None => return AppError::NotFound.into_response(),
    };
    if !matches!(entry.status, Status::Done) {
        return (
            StatusCode::CONFLICT,
            format!("Audio not ready (status: {:?})", entry.status),
        )
            .into_response();
    }
    let path = library.audio_path(&entry);
    let filename = filename_for(&entry.title, "mp3");
    serve_file(&path, "audio/mpeg", Some(&filename)).await
}

async fn serve_cover(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let library = match library_or_404(&state) {
        Ok(l) => l,
        Err(r) => return r,
    };
    if !PODCAST_ID_RE.is_match(&id) {
        return AppError::BadRequest("Invalid id".into()).into_response();
    }
    let entry = match library.get(&id).await {
        Some(e) => e,
        None => return AppError::NotFound.into_response(),
    };
    let path = library.cover_path(&entry);
    if !path.exists() {
        return AppError::NotFound.into_response();
    }
    serve_file(&path, "image/jpeg", None).await
}

/// Serve a file by streaming it through `tokio_util::io::ReaderStream` instead
/// of buffering it. Critical for the audio path: audiobooks can be several
/// gigabytes, so reading them fully into RAM would (a) blow the heap and
/// (b) delay the first byte until the entire file is loaded.
async fn serve_file(
    path: &PathBuf,
    content_type: &'static str,
    attachment: Option<&str>,
) -> Response {
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return AppError::NotFound.into_response(),
    };
    // Metadata read is essentially free and gives us a `Content-Length` so the
    // browser can render a real progress bar during the download.
    let size = file.metadata().await.ok().map(|m| m.len());

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Some(size) = size {
        if let Ok(v) = HeaderValue::from_str(&size.to_string()) {
            headers.insert(header::CONTENT_LENGTH, v);
        }
    }
    if let Some(name) = attachment {
        if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
            headers.insert(header::CONTENT_DISPOSITION, v);
        }
    }
    (StatusCode::OK, headers, body).into_response()
}

fn filename_for(title: &str, ext: &str) -> String {
    // Strip path-unsafe and HTTP-header-unsafe chars from the title so the
    // browser's Save dialog gets something usable.
    let safe: String = title
        .chars()
        .map(|c| match c {
            '/' | '\\' | '"' | '\r' | '\n' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = safe.trim();
    if trimmed.is_empty() {
        format!("audiobook.{ext}")
    } else {
        format!("{trimmed}.{ext}")
    }
}

fn build_entry(audiobook_id: &str, meta: &Value) -> anyhow::Result<LibraryEntry> {
    let book = meta
        .get("audiobookById")
        .ok_or_else(|| anyhow::anyhow!("no audiobookById in payload"))?;
    let title = book
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("Untitled")
        .to_string();
    let description = book
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let author = book
        .get("authorNames")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| collect_names(book.get("authors")))
        .unwrap_or_default();
    let narrators = collect_names(book.get("narrators")).unwrap_or_default();
    let duration_seconds = book.get("duration").and_then(|v| v.as_i64()).unwrap_or(0);
    let publisher = book
        .get("publisherName")
        .and_then(|v| v.as_str())
        .map(String::from);
    let year = book.get("yearOfBookPublication").and_then(|v| v.as_i64());

    Ok(LibraryEntry {
        id: audiobook_id.to_string(),
        title,
        author,
        narrators,
        description,
        duration_seconds,
        publisher,
        year,
        added_at: chrono::Utc::now().to_rfc3339(),
        status: Status::Queued,
        error: None,
        audio_size_bytes: None,
        audio_downloaded_bytes: 0,
    })
}

fn collect_names(v: Option<&Value>) -> Option<String> {
    let arr = v?.as_array()?;
    let names: Vec<String> = arr
        .iter()
        .filter_map(|item| item.get("name").and_then(|n| n.as_str()).map(String::from))
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names.join(", "))
    }
}
