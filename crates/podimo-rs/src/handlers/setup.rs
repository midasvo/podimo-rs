//! GET /setup — library diagnostics + path testing.
//!
//! Read-only by design: the actual `LIBRARY_DIR` is an env var read at
//! startup, so changing it persistently still means editing `.env` and
//! restarting. What this page does for you:
//!
//!   1. Shows whether the currently-configured `LIBRARY_DIR` is reachable,
//!      writable, and readable from inside the container.
//!   2. Lets you type any other path and run the same checks against it,
//!      so you can verify a candidate mount before committing.
//!   3. Summarises what the library currently holds (entry count + total
//!      bytes on disk).
//!
//! The path checks (1 and 2) write a probe file, and the page has no auth,
//! so they only run with `LOCAL_CREDENTIALS=true`: an instance that's
//! shared with other people doesn't let them probe its filesystem.

use std::path::{Path, PathBuf};

use axum::extract::{Form, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::error::AppError;
use crate::state::AppState;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/setup", get(render_setup))
        .route("/setup/test-path", post(handle_test_path))
}

#[derive(Serialize)]
struct SetupCtx<'a> {
    enable_library: bool,
    /// `LIBRARY_DIR` as configured.
    library_dir: &'a str,
    /// Whether `LIBRARY_DIR` would also actually function (gated on
    /// `LOCAL_CREDENTIALS=true`).
    library_active: bool,
    /// Outcome of probing `LIBRARY_DIR`, when `local_credentials` is true.
    library_check: Option<PathCheckView>,
    /// Number of hydrated library entries, when active. `None` otherwise.
    entry_count: Option<usize>,
    /// Sum of `audio_size_bytes` across entries (rounded MB). `None` when not
    /// active.
    total_size_mb: Option<String>,
    /// Whether path probing (via `/setup/test-path` and `LIBRARY_DIR` checks)
    /// is enabled. Only true when `LOCAL_CREDENTIALS=true`.
    probe_enabled: bool,
    /// Optional result of the user-submitted path test.
    test_path: Option<&'a str>,
    test_result: Option<PathCheckView>,
    /// Reason `LOCAL_CREDENTIALS=true` matters, surfaced inline.
    requires_local_credentials: bool,
    local_credentials_on: bool,
}

#[derive(Serialize)]
struct PathCheckView {
    path: String,
    exists: bool,
    is_dir: bool,
    writable: bool,
    readable: bool,
    /// One-line error message when any check failed; `None` when everything
    /// passed or the path didn't exist (no error to report).
    error: Option<String>,
    /// Overall status: a short label the template can render as a badge.
    summary: &'static str,
    summary_kind: &'static str, // "ok" | "warn" | "fail"
}

async fn render_setup(State(state): State<AppState>) -> Response {
    render_with(&state, None, None).await
}

#[derive(Debug, Deserialize)]
struct TestPathForm {
    path: String,
}

async fn handle_test_path(
    State(state): State<AppState>,
    Form(form): Form<TestPathForm>,
) -> Response {
    if !state.config.local_credentials {
        return (
            StatusCode::NOT_FOUND,
            "Path probing is only available with LOCAL_CREDENTIALS=true.",
        )
            .into_response();
    }
    let trimmed = form.path.trim();
    if trimmed.is_empty() {
        return render_with(&state, None, None).await;
    }
    let check = check_path(Path::new(trimmed)).await;
    let path_owned = trimmed.to_string();
    let view = view_from_check(check, &path_owned);
    render_with(&state, Some(path_owned), Some(view)).await
}

async fn render_with(
    state: &AppState,
    test_path: Option<String>,
    test_result: Option<PathCheckView>,
) -> Response {
    let library_view = if state.config.local_credentials {
        let library_check = check_path(Path::new(&state.config.library_dir)).await;
        Some(view_from_check(library_check, &state.config.library_dir))
    } else {
        None
    };

    let (entry_count, total_size_mb) = if let Some(library) = &state.library {
        let entries = library.list().await;
        let count = entries.len();
        let total: u64 = entries.iter().filter_map(|e| e.audio_size_bytes).sum();
        let mb = if total == 0 {
            "0 MB".to_string()
        } else {
            format!("{:.1} MB", total as f64 / (1024.0 * 1024.0))
        };
        (Some(count), Some(mb))
    } else {
        (None, None)
    };

    let ctx = SetupCtx {
        enable_library: state.config.enable_library,
        library_dir: &state.config.library_dir,
        library_active: state.library.is_some(),
        library_check: library_view,
        entry_count,
        total_size_mb,
        probe_enabled: state.config.local_credentials,
        test_path: test_path.as_deref(),
        test_result,
        requires_local_credentials: state.config.enable_library && !state.config.local_credentials,
        local_credentials_on: state.config.local_credentials,
    };
    match state.templates.render("setup.html", &ctx) {
        Ok(body) => Html(body).into_response(),
        Err(err) => {
            tracing::error!(target: "podimo::setup", "template render failed: {err}");
            AppError::Internal(format!("template render: {err}")).into_response()
        }
    }
}

struct PathCheck {
    exists: bool,
    is_dir: bool,
    writable: bool,
    readable: bool,
    error: Option<String>,
}

/// Runs four cheap checks on `p`: exists, is_dir, writable (create + remove
/// a temp file), readable (read that temp file back before removal).
///
/// We deliberately don't `create_dir_all` here — that would mask "path is
/// completely wrong" mistakes by silently creating the dir tree. If you need
/// to create a fresh dir, do it explicitly outside this function.
async fn check_path(p: &Path) -> PathCheck {
    if !p.exists() {
        return PathCheck {
            exists: false,
            is_dir: false,
            writable: false,
            readable: false,
            error: None,
        };
    }
    let is_dir = p.is_dir();
    if !is_dir {
        return PathCheck {
            exists: true,
            is_dir: false,
            writable: false,
            readable: false,
            error: Some("Path exists but is not a directory".into()),
        };
    }

    // Probe with a uniquely-named temp file so we don't collide with anything
    // the library legitimately wrote.
    let probe_name = format!(".podimo-rs-probe-{}", std::process::id());
    let probe_path: PathBuf = p.join(&probe_name);
    let writable = fs::write(&probe_path, b"podimo-rs probe\n").await;
    let (writable_ok, write_err) = match writable {
        Ok(_) => (true, None),
        Err(err) => (false, Some(format!("write failed: {err}"))),
    };

    let mut readable_ok = false;
    let mut read_err = None;
    if writable_ok {
        match fs::read(&probe_path).await {
            Ok(bytes) if bytes == b"podimo-rs probe\n" => readable_ok = true,
            Ok(_) => read_err = Some("read-back returned unexpected bytes".to_string()),
            Err(err) => read_err = Some(format!("read failed: {err}")),
        }
        let _ = fs::remove_file(&probe_path).await;
    }

    PathCheck {
        exists: true,
        is_dir: true,
        writable: writable_ok,
        readable: readable_ok,
        error: write_err.or(read_err),
    }
}

fn view_from_check(c: PathCheck, path: &str) -> PathCheckView {
    let (summary, summary_kind) = match (c.exists, c.is_dir, c.writable, c.readable) {
        (false, _, _, _) => ("Doesn't exist", "fail"),
        (true, false, _, _) => ("Not a directory", "fail"),
        (true, true, false, _) => ("Read-only (or no permission)", "fail"),
        (true, true, true, false) => ("Writable but read-back failed", "warn"),
        (true, true, true, true) => ("OK", "ok"),
    };
    PathCheckView {
        path: path.to_string(),
        exists: c.exists,
        is_dir: c.is_dir,
        writable: c.writable,
        readable: c.readable,
        error: c.error,
        summary,
        summary_kind,
    }
}
