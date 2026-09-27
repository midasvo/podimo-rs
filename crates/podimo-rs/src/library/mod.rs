//! Audiobook library — persistent on-disk store with an in-memory index.
//!
//! On-disk layout follows Audiobookshelf's convention so the same directory
//! can be mounted into an Audiobookshelf library root without rearranging:
//!
//! ```text
//! LIBRARY_DIR/
//!   <Author Name>/
//!     <Book Title>/
//!       <Book Title>.mp3      # the audio (written via `.partial` → rename)
//!       cover.jpg             # cover image
//!       metadata.json         # Audiobookshelf-format metadata (consumed by ABS)
//!       podimo-state.json     # our internal state: UUID, status, progress
//! ```
//!
//! Hydration: on startup the library walks `LIBRARY_DIR/**/podimo-state.json`
//! and rebuilds the in-memory map. Any entries left in `Queued`/`Downloading`
//! are forced to `Failed("interrupted by restart")` — we don't try to resume
//! cross-process because the signed audio URL is short-lived and would be
//! invalid by the time we got here.
//!
//! Migration: if any top-level directory matches the legacy UUID layout
//! (`LIBRARY_DIR/<uuid>/meta.json`), the contents are moved into the new
//! Author/Title layout on first startup.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::sync::RwLock;

pub mod download;

/// Filename for our internal state record inside each book directory.
const STATE_FILE: &str = "podimo-state.json";
/// Filename Audiobookshelf reads for explicit metadata overrides.
const ABS_METADATA_FILE: &str = "metadata.json";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Downloading,
    Done,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryEntry {
    pub id: String,
    pub title: String,
    pub author: String,
    pub narrators: String,
    pub description: String,
    pub duration_seconds: i64,
    pub publisher: Option<String>,
    pub year: Option<i64>,
    /// RFC-3339 timestamp.
    pub added_at: String,
    pub status: Status,
    pub error: Option<String>,
    pub audio_size_bytes: Option<u64>,
    pub audio_downloaded_bytes: u64,
}

#[derive(Clone)]
pub struct Library {
    root: PathBuf,
    entries: Arc<RwLock<BTreeMap<String, LibraryEntry>>>,
}

impl std::fmt::Debug for Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Library").field("root", &self.root).finish()
    }
}

impl Library {
    /// Construct a Library rooted at `root`. Migrates any legacy UUID-layout
    /// entries, then hydrates the in-memory index from the on-disk tree.
    /// Creates `<root>` if missing.
    pub async fn new(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).await?;
        migrate_legacy_layout(&root).await?;
        let entries = hydrate(&root).await?;
        Ok(Self {
            root,
            entries: Arc::new(RwLock::new(entries)),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// On-disk directory for `entry`, derived from sanitized author + title.
    pub fn entry_dir(&self, entry: &LibraryEntry) -> PathBuf {
        self.root
            .join(sanitize_segment(&entry.author, "Unknown Author"))
            .join(sanitize_segment(&entry.title, "Untitled"))
    }

    /// `<entry_dir>/<sanitized title>.mp3` — Audiobookshelf doesn't care about
    /// the exact name as long as there's exactly one recognized audio file in
    /// the book dir, but a title-based filename makes the tree readable when
    /// browsing the NAS by hand.
    pub fn audio_path(&self, entry: &LibraryEntry) -> PathBuf {
        let mut p = self.entry_dir(entry);
        p.push(format!(
            "{}.mp3",
            sanitize_segment(&entry.title, "Untitled")
        ));
        p
    }

    pub fn audio_partial_path(&self, entry: &LibraryEntry) -> PathBuf {
        let mut p = self.audio_path(entry);
        p.set_extension("mp3.partial");
        p
    }

    pub fn cover_path(&self, entry: &LibraryEntry) -> PathBuf {
        self.entry_dir(entry).join("cover.jpg")
    }

    pub fn state_path(&self, entry: &LibraryEntry) -> PathBuf {
        self.entry_dir(entry).join(STATE_FILE)
    }

    /// Convenience: look up `id` and compute its audio path. Returns `None` if
    /// the entry doesn't exist.
    pub async fn audio_path_for(&self, id: &str) -> Option<PathBuf> {
        let entry = self.entries.read().await.get(id).cloned()?;
        Some(self.audio_path(&entry))
    }

    /// Convenience: look up `id` and compute its cover path.
    pub async fn cover_path_for(&self, id: &str) -> Option<PathBuf> {
        let entry = self.entries.read().await.get(id).cloned()?;
        Some(self.cover_path(&entry))
    }

    /// Convenience: look up `id` and compute its partial-download path.
    pub async fn audio_partial_path_for(&self, id: &str) -> Option<PathBuf> {
        let entry = self.entries.read().await.get(id).cloned()?;
        Some(self.audio_partial_path(&entry))
    }

    /// Snapshot of all entries, newest-added first.
    pub async fn list(&self) -> Vec<LibraryEntry> {
        let guard = self.entries.read().await;
        let mut v: Vec<LibraryEntry> = guard.values().cloned().collect();
        // BTreeMap iterates by id (UUID order). Sort by added_at descending so the
        // most recently added book appears at the top of the overview.
        v.sort_by(|a, b| b.added_at.cmp(&a.added_at));
        v
    }

    pub async fn get(&self, id: &str) -> Option<LibraryEntry> {
        self.entries.read().await.get(id).cloned()
    }

    pub async fn contains(&self, id: &str) -> bool {
        self.entries.read().await.contains_key(id)
    }

    /// Insert a fresh entry. Writes both `podimo-state.json` (our state) and
    /// `metadata.json` (Audiobookshelf format) so ABS can pick the book up as
    /// soon as the audio file lands.
    ///
    /// **Refuses to overwrite anything.** Fails if:
    ///   - `id` is already in the in-memory index, or
    ///   - the target `<author>/<title>/` directory already exists on disk.
    ///
    /// The second check is the safety net for libraries shared with an
    /// existing Audiobookshelf collection: if the user already has a book at
    /// the same author/title path, we refuse rather than write our files
    /// into their directory. The user must move or rename their existing
    /// book first.
    pub async fn add(&self, entry: LibraryEntry) -> anyhow::Result<()> {
        let dir = self.entry_dir(&entry);
        {
            let guard = self.entries.read().await;
            if guard.contains_key(&entry.id) {
                anyhow::bail!("library already contains {}", entry.id);
            }
        }
        if fs::try_exists(&dir).await.unwrap_or(false) {
            // Distinguish "already a podimo entry on disk" from "foreign
            // content not managed by podimo-rs" so the user knows whether to
            // delete it themselves or whether something needs investigation.
            let has_state = fs::try_exists(dir.join(STATE_FILE)).await.unwrap_or(false);
            if has_state {
                anyhow::bail!(
                    "target directory `{}` already contains a podimo-rs entry — remove it first",
                    dir.display(),
                );
            } else {
                anyhow::bail!(
                    "target directory `{}` already exists and is not managed by podimo-rs — \
                    move or rename it first; podimo-rs refuses to overlay foreign content",
                    dir.display(),
                );
            }
        }
        fs::create_dir_all(&dir).await?;
        let mut guard = self.entries.write().await;
        // Re-check after taking the write lock in case of a concurrent add.
        if guard.contains_key(&entry.id) {
            anyhow::bail!("library already contains {}", entry.id);
        }
        write_state(&dir, &entry).await?;
        write_abs_metadata(&dir, &entry).await?;
        guard.insert(entry.id.clone(), entry);
        Ok(())
    }

    /// "Forget" an entry — drop it from the in-memory index and delete the
    /// `podimo-state.json` marker plus any leftover `<title>.mp3.partial`
    /// (an in-progress download only podimo-rs ever writes), but **leave
    /// every other file in place** (`metadata.json`, a finished audio file,
    /// the cover image, the directory itself, the author directory). The
    /// book stays visible to Audiobookshelf scanning the same volume; only
    /// podimo-rs stops tracking it.
    ///
    /// This is the safe semantics for a library shared with an existing ABS
    /// collection: removing a book from the podimo-rs UI never deletes the
    /// audio file. If the user wants to wipe the file too they do it with
    /// `rm -rf` (or the NAS UI) themselves.
    ///
    /// Returns `Ok(true)` if the entry was forgotten, `Ok(false)` if no
    /// entry with that id existed.
    pub async fn remove(&self, id: &str) -> anyhow::Result<bool> {
        let mut guard = self.entries.write().await;
        let entry = match guard.remove(id) {
            Some(e) => e,
            None => return Ok(false),
        };
        let state_file = self.state_path(&entry);
        if let Err(err) = fs::remove_file(&state_file).await {
            if err.kind() != io::ErrorKind::NotFound {
                tracing::warn!(target: "podimo::library", "delete {}: {err}", state_file.display());
            }
        }
        let partial_file = self.audio_partial_path(&entry);
        if let Err(err) = fs::remove_file(&partial_file).await {
            if err.kind() != io::ErrorKind::NotFound {
                tracing::warn!(target: "podimo::library", "delete {}: {err}", partial_file.display());
            }
        }
        Ok(true)
    }

    /// Atomically read-modify-write an entry. The callback gets a `&mut` to the
    /// in-memory copy; on return the changes are persisted to
    /// `podimo-state.json` before the lock is dropped. Returns `Ok(None)` if
    /// `id` doesn't exist.
    pub async fn update<F>(&self, id: &str, f: F) -> anyhow::Result<Option<LibraryEntry>>
    where
        F: FnOnce(&mut LibraryEntry),
    {
        let mut guard = self.entries.write().await;
        let entry = match guard.get_mut(id) {
            Some(e) => e,
            None => return Ok(None),
        };
        f(entry);
        let snapshot = entry.clone();
        let dir = self.entry_dir(&snapshot);
        write_state(&dir, &snapshot).await?;
        Ok(Some(snapshot))
    }
}

/// Filesystem-safe form of a path segment. Replaces FS-unsafe + control chars
/// with whitespace, collapses runs of whitespace into a single space, trims
/// leading/trailing whitespace + trailing dots (Windows can't handle either),
/// and falls back to `fallback` when the result would otherwise be empty.
fn sanitize_segment(raw: &str, fallback: &str) -> String {
    let replaced: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    // `split_whitespace` collapses any run of Unicode whitespace into a single
    // ASCII space, which is exactly what we want.
    let collapsed = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() {
        fallback.to_string()
    } else if trimmed.chars().count() > 200 {
        // Keep filenames well under the 255-byte limit on common filesystems.
        trimmed.chars().take(200).collect()
    } else {
        trimmed.to_string()
    }
}

/// Audiobookshelf consumes a `metadata.json` next to the audio file as
/// authoritative metadata. We write the subset Podimo gives us.
#[derive(Debug, Serialize)]
struct AbsMetadata<'a> {
    title: &'a str,
    authors: Vec<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    narrators: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "publishedYear")]
    published_year: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    publisher: Option<&'a str>,
}

fn split_names(joined: &str) -> Vec<&str> {
    joined
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

async fn write_abs_metadata(dir: &Path, entry: &LibraryEntry) -> anyhow::Result<()> {
    let meta = AbsMetadata {
        title: &entry.title,
        authors: split_names(&entry.author),
        narrators: split_names(&entry.narrators),
        description: if entry.description.is_empty() {
            None
        } else {
            Some(&entry.description)
        },
        published_year: entry.year.map(|y| y.to_string()),
        publisher: entry.publisher.as_deref().filter(|s| !s.is_empty()),
    };
    let json = serde_json::to_vec_pretty(&meta)?;
    let final_path = dir.join(ABS_METADATA_FILE);
    let tmp_path = dir.join(format!("{ABS_METADATA_FILE}.tmp"));
    fs::write(&tmp_path, json).await?;
    fs::rename(&tmp_path, &final_path).await?;
    Ok(())
}

/// Write `podimo-state.json` atomically (`tmp` → rename). Cheap because the
/// state is small; the audio file uses the same pattern in `download.rs`.
async fn write_state(dir: &Path, entry: &LibraryEntry) -> anyhow::Result<()> {
    let json = serde_json::to_vec_pretty(entry)?;
    let final_path = dir.join(STATE_FILE);
    let tmp_path = dir.join(format!("{STATE_FILE}.tmp"));
    fs::write(&tmp_path, json).await?;
    fs::rename(&tmp_path, &final_path).await?;
    Ok(())
}

/// Walk `<root>/**/podimo-state.json` two levels deep (Author/Title). Force
/// in-flight states (`Queued` / `Downloading`) to `Failed("interrupted")`.
async fn hydrate(root: &Path) -> anyhow::Result<BTreeMap<String, LibraryEntry>> {
    let mut map = BTreeMap::new();
    let mut top = match fs::read_dir(root).await {
        Ok(r) => r,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(map),
        Err(err) => return Err(err.into()),
    };
    while let Some(author_dir) = top.next_entry().await? {
        let author_path = author_dir.path();
        if !author_path.is_dir() {
            continue;
        }
        let mut second = match fs::read_dir(&author_path).await {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(target: "podimo::library", "skip {}: {err}", author_path.display());
                continue;
            }
        };
        while let Some(title_dir) = second.next_entry().await? {
            let title_path = title_dir.path();
            if !title_path.is_dir() {
                continue;
            }
            let state_path = title_path.join(STATE_FILE);
            let raw = match fs::read(&state_path).await {
                Ok(b) => b,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    tracing::warn!(target: "podimo::library", "skip {}: {err}", state_path.display());
                    continue;
                }
            };
            let mut book: LibraryEntry = match serde_json::from_slice(&raw) {
                Ok(b) => b,
                Err(err) => {
                    tracing::warn!(target: "podimo::library", "corrupt {}: {err}", state_path.display());
                    continue;
                }
            };
            if matches!(book.status, Status::Queued | Status::Downloading) {
                book.status = Status::Failed;
                book.error = Some("interrupted by restart".into());
                if let Err(err) = write_state(&title_path, &book).await {
                    tracing::warn!(target: "podimo::library", "rewrite {}: {err}", state_path.display());
                }
            }
            map.insert(book.id.clone(), book);
        }
    }
    Ok(map)
}

/// Migrate any legacy `LIBRARY_DIR/<uuid>/meta.json` entries from the
/// UUID-keyed layout to the new Author/Title layout. Best-effort: failures on
/// individual books are logged and left in place. Runs every startup; on a
/// clean install it scans the top-level dir and does nothing.
async fn migrate_legacy_layout(root: &Path) -> anyhow::Result<()> {
    let mut top = match fs::read_dir(root).await {
        Ok(r) => r,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    static UUID_DIR_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(
            r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$",
        )
        .expect("static regex compiles")
    });

    while let Some(dir_entry) = top.next_entry().await? {
        let path = dir_entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !UUID_DIR_RE.is_match(name) {
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        let legacy_meta = path.join("meta.json");
        let raw = match fs::read(&legacy_meta).await {
            Ok(b) => b,
            Err(_) => continue, // not a podimo dir, leave alone
        };
        let entry: LibraryEntry = match serde_json::from_slice(&raw) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(target: "podimo::library", "legacy meta unreadable, skipping migration of {}: {err}", path.display());
                continue;
            }
        };
        let new_dir = root
            .join(sanitize_segment(&entry.author, "Unknown Author"))
            .join(sanitize_segment(&entry.title, "Untitled"));
        if new_dir.exists() {
            tracing::warn!(target: "podimo::library", "migration target {} already exists; leaving legacy {} in place", new_dir.display(), path.display());
            continue;
        }
        if let Some(parent) = new_dir.parent() {
            fs::create_dir_all(parent).await?;
        }
        // Move the whole legacy dir to the new location.
        if let Err(err) = fs::rename(&path, &new_dir).await {
            tracing::warn!(target: "podimo::library", "rename {} → {} failed: {err}", path.display(), new_dir.display());
            continue;
        }
        // Rename audio.mp3 → <sanitized_title>.mp3 inside the moved dir.
        let old_audio = new_dir.join("audio.mp3");
        let new_audio = new_dir.join(format!(
            "{}.mp3",
            sanitize_segment(&entry.title, "Untitled")
        ));
        if old_audio.exists() && old_audio != new_audio {
            if let Err(err) = fs::rename(&old_audio, &new_audio).await {
                tracing::warn!(target: "podimo::library", "rename {} → {} failed: {err}", old_audio.display(), new_audio.display());
            }
        }
        // Rename meta.json → podimo-state.json.
        let old_state = new_dir.join("meta.json");
        let new_state = new_dir.join(STATE_FILE);
        if old_state.exists() && !new_state.exists() {
            if let Err(err) = fs::rename(&old_state, &new_state).await {
                tracing::warn!(target: "podimo::library", "rename {} → {} failed: {err}", old_state.display(), new_state.display());
            }
        }
        // Drop an ABS-format metadata.json next to it.
        if let Err(err) = write_abs_metadata(&new_dir, &entry).await {
            tracing::warn!(target: "podimo::library", "write abs metadata in {} failed: {err}", new_dir.display());
        }
        tracing::info!(target: "podimo::library", "migrated legacy entry {} → {}", path.display(), new_dir.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(id: &str) -> LibraryEntry {
        LibraryEntry {
            id: id.into(),
            title: "The Reapers Are the Angels".into(),
            author: "Alden Bell".into(),
            narrators: "A Narrator".into(),
            description: "Desc".into(),
            duration_seconds: 3600,
            publisher: Some("Pub".into()),
            year: Some(2024),
            added_at: "2026-05-14T10:00:00Z".into(),
            status: Status::Queued,
            error: None,
            audio_size_bytes: None,
            audio_downloaded_bytes: 0,
        }
    }

    #[tokio::test]
    async fn add_creates_author_title_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::new(tmp.path()).await.unwrap();
        let e = sample_entry("a1");
        lib.add(e.clone()).await.unwrap();
        let expected_dir = tmp
            .path()
            .join("Alden Bell")
            .join("The Reapers Are the Angels");
        assert!(
            expected_dir.exists(),
            "expected {} to exist",
            expected_dir.display()
        );
        assert!(
            expected_dir.join(STATE_FILE).exists(),
            "podimo-state.json missing"
        );
        assert!(
            expected_dir.join(ABS_METADATA_FILE).exists(),
            "metadata.json (ABS) missing"
        );
        // Audio path is the sanitized title + .mp3.
        let audio = lib.audio_path(&e);
        assert_eq!(audio.file_name().unwrap(), "The Reapers Are the Angels.mp3");
        assert_eq!(audio.parent().unwrap(), expected_dir);
    }

    #[tokio::test]
    async fn abs_metadata_has_split_authors_and_narrators() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::new(tmp.path()).await.unwrap();
        let mut e = sample_entry("a1");
        e.author = "First Author, Second Author".into();
        e.narrators = "Narrator A, Narrator B".into();
        lib.add(e.clone()).await.unwrap();
        let meta_raw =
            std::fs::read(lib.entry_dir(&e).join(ABS_METADATA_FILE)).expect("metadata.json exists");
        let v: serde_json::Value = serde_json::from_slice(&meta_raw).unwrap();
        assert_eq!(v["title"], "The Reapers Are the Angels");
        let authors: Vec<&str> = v["authors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert_eq!(authors, vec!["First Author", "Second Author"]);
        let narrators: Vec<&str> = v["narrators"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert_eq!(narrators, vec!["Narrator A", "Narrator B"]);
        assert_eq!(v["publishedYear"], "2024");
    }

    #[tokio::test]
    async fn sanitize_strips_unsafe_chars() {
        assert_eq!(sanitize_segment("A / B", "x"), "A B");
        assert_eq!(sanitize_segment("With: colon", "x"), "With colon");
        assert_eq!(
            sanitize_segment(" leading and trailing ", "x"),
            "leading and trailing"
        );
        assert_eq!(sanitize_segment("trailing.", "x"), "trailing");
        assert_eq!(sanitize_segment("", "fallback"), "fallback");
        assert_eq!(sanitize_segment("   ", "fallback"), "fallback");
        // Long titles are truncated.
        let long = "x".repeat(500);
        let s = sanitize_segment(&long, "x");
        assert!(s.len() <= 200);
    }

    #[tokio::test]
    async fn add_twice_same_id_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::new(tmp.path()).await.unwrap();
        lib.add(sample_entry("a1")).await.unwrap();
        let err = lib.add(sample_entry("a1")).await.unwrap_err();
        assert!(err.to_string().contains("already contains"));
    }

    #[tokio::test]
    async fn forget_only_removes_state_file_leaves_audio_intact() {
        // "remove" semantics post-safety-layer: the on-disk audio + cover +
        // ABS metadata stay; only `podimo-state.json` (our marker) goes.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::new(tmp.path()).await.unwrap();
        let e = sample_entry("a1");
        lib.add(e.clone()).await.unwrap();
        let dir = lib.entry_dir(&e);
        // Plant a fake audio + cover next to the metadata to simulate a
        // completed download.
        let audio = lib.audio_path(&e);
        let cover = lib.cover_path(&e);
        std::fs::write(&audio, b"FAKE-AUDIO").unwrap();
        std::fs::write(&cover, b"FAKE-COVER").unwrap();

        assert!(lib.remove("a1").await.unwrap());

        assert!(
            lib.get("a1").await.is_none(),
            "in-memory entry should be gone"
        );
        assert!(!lib.state_path(&e).exists(), "state file should be deleted");
        // Everything else stays.
        assert!(dir.exists(), "directory should remain");
        assert!(audio.exists(), "audio file should remain");
        assert!(cover.exists(), "cover should remain");
        assert!(
            dir.join(ABS_METADATA_FILE).exists(),
            "ABS metadata should remain"
        );
        assert!(dir.parent().unwrap().exists(), "author dir should remain");
    }

    #[tokio::test]
    async fn forget_deletes_partial_download_but_leaves_everything_else() {
        // Forget must clean up our own in-progress `.mp3.partial` file — it's
        // the one file besides `podimo-state.json` that only podimo-rs ever
        // writes — while still leaving the finished audio, cover, and ABS
        // metadata untouched.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::new(tmp.path()).await.unwrap();
        let e = sample_entry("a1");
        lib.add(e.clone()).await.unwrap();
        let dir = lib.entry_dir(&e);
        let audio = lib.audio_path(&e);
        let cover = lib.cover_path(&e);
        let partial = lib.audio_partial_path(&e);
        std::fs::write(&audio, b"FAKE-AUDIO").unwrap();
        std::fs::write(&cover, b"FAKE-COVER").unwrap();
        std::fs::write(&partial, b"IN-PROGRESS").unwrap();

        assert!(lib.remove("a1").await.unwrap());

        assert!(!partial.exists(), "partial download should be deleted");
        assert!(!lib.state_path(&e).exists(), "state file should be deleted");
        // Everything else stays — this is still "Forget", not "delete".
        assert!(audio.exists(), "finished audio should remain");
        assert!(cover.exists(), "cover should remain");
        assert!(
            dir.join(ABS_METADATA_FILE).exists(),
            "ABS metadata should remain"
        );
    }

    #[tokio::test]
    async fn remove_unknown_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::new(tmp.path()).await.unwrap();
        assert!(!lib.remove("nope").await.unwrap());
    }

    #[tokio::test]
    async fn add_refuses_existing_dir_with_foreign_content() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::new(tmp.path()).await.unwrap();
        let e = sample_entry("a1");
        // Plant a pre-existing book at the would-be target path with random
        // content (simulating a user's existing Audiobookshelf book).
        let foreign_dir = tmp
            .path()
            .join("Alden Bell")
            .join("The Reapers Are the Angels");
        std::fs::create_dir_all(&foreign_dir).unwrap();
        std::fs::write(foreign_dir.join("Some Other Book.mp3"), b"NOT MINE").unwrap();

        let err = lib.add(e.clone()).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not managed by podimo-rs"),
            "expected foreign-content error: {msg}"
        );
        // The foreign content should be untouched.
        assert!(foreign_dir.join("Some Other Book.mp3").exists());
        assert!(
            !foreign_dir.join(STATE_FILE).exists(),
            "state file must not have been written"
        );
        assert!(
            !foreign_dir.join(ABS_METADATA_FILE).exists(),
            "ABS metadata must not have been written"
        );
    }

    #[tokio::test]
    async fn add_refuses_existing_dir_with_prior_podimo_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::new(tmp.path()).await.unwrap();
        let e = sample_entry("a1");
        lib.add(e.clone()).await.unwrap();
        // Forget the entry (state file gone, dir stays).
        lib.remove("a1").await.unwrap();
        // Re-adding must refuse with the "prior podimo entry"-flavoured
        // error since the state file isn't there, OR the foreign-content
        // flavour since there's no state file either way. After remove the
        // state file is gone, so this lands on the foreign-content branch.
        let err = lib.add(e.clone()).await.unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn add_refuses_when_a_podimo_state_file_lingers() {
        // Edge case: the state file lingers in a dir we somehow don't know
        // about. Add should refuse with the "already a podimo entry"
        // message.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::new(tmp.path()).await.unwrap();
        let e = sample_entry("a1");
        let dir = tmp
            .path()
            .join("Alden Bell")
            .join("The Reapers Are the Angels");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(STATE_FILE), b"{}").unwrap();
        let err = lib.add(e).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("already contains a podimo-rs entry"),
            "expected already-managed message: {}",
            err
        );
    }

    #[tokio::test]
    async fn update_persists_to_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::new(tmp.path()).await.unwrap();
        lib.add(sample_entry("a1")).await.unwrap();
        lib.update("a1", |e| {
            e.status = Status::Done;
            e.audio_size_bytes = Some(42);
        })
        .await
        .unwrap();

        // Rehydrate from disk to confirm persistence.
        let lib2 = Library::new(tmp.path()).await.unwrap();
        let e = lib2.get("a1").await.unwrap();
        assert_eq!(e.status, Status::Done);
        assert_eq!(e.audio_size_bytes, Some(42));
    }

    #[tokio::test]
    async fn hydrate_marks_interrupted_downloads_as_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::new(tmp.path()).await.unwrap();
        let mut e = sample_entry("a1");
        e.status = Status::Downloading;
        lib.add(e).await.unwrap();

        let lib2 = Library::new(tmp.path()).await.unwrap();
        let e = lib2.get("a1").await.unwrap();
        assert_eq!(e.status, Status::Failed);
        assert_eq!(e.error.as_deref(), Some("interrupted by restart"));
    }

    #[tokio::test]
    async fn migrates_legacy_uuid_layout_on_startup() {
        let tmp = tempfile::tempdir().unwrap();
        // Plant a legacy entry: LIBRARY_DIR/<uuid>/meta.json + audio.mp3.
        let uuid = "fefa939e-c84d-4c16-8bbf-9575e1379d81";
        let legacy_dir = tmp.path().join(uuid);
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let mut e = sample_entry(uuid);
        e.status = Status::Done;
        e.audio_size_bytes = Some(123);
        e.audio_downloaded_bytes = 123;
        let meta_json = serde_json::to_vec_pretty(&e).unwrap();
        std::fs::write(legacy_dir.join("meta.json"), meta_json).unwrap();
        std::fs::write(legacy_dir.join("audio.mp3"), b"FAKE").unwrap();

        // Boot a Library at this root: migration should move things.
        let lib = Library::new(tmp.path()).await.unwrap();
        let new_dir = tmp
            .path()
            .join("Alden Bell")
            .join("The Reapers Are the Angels");
        assert!(new_dir.exists(), "new dir not created");
        assert!(!legacy_dir.exists(), "legacy dir should be gone");
        assert!(new_dir.join(STATE_FILE).exists());
        assert!(new_dir.join(ABS_METADATA_FILE).exists());
        assert!(
            new_dir.join("The Reapers Are the Angels.mp3").exists(),
            "audio not renamed"
        );

        // Hydrated entry still has its UUID + Done status.
        let entry = lib.get(uuid).await.unwrap();
        assert_eq!(entry.status, Status::Done);
        assert_eq!(entry.audio_size_bytes, Some(123));
    }
}
