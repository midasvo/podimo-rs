//! Caches with TTL semantics.
//!
//! Five caches:
//!   - `tokens`           — login token by `sha256(username~password)`. Optionally persisted to disk.
//!   - `podcasts`         — full episode list per podcast id, JSON value. Persisted.
//!   - `audiobook_meta`   — `audiobookById` metadata payload per audiobook id. Persisted.
//!   - `audiobook_audio`  — short-lived signed audio URL per audiobook id. Persisted; short TTL.
//!   - `head`             — `(content_length, content_type)` per episode/audiobook id. Persisted.
//!
//! Disk format: one JSON file per entry under `<cache_dir>/<name>/<key>.json`,
//! holding `{"expiry": <unix seconds>, "value": …}`. It has to be a
//! self-describing format: bincode and postcard can't read a `serde_json::Value`
//! back. Wipe `<cache_dir>` to reset.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use moka::future::Cache;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::fs;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeadInfo {
    pub content_length: String,
    pub content_type: String,
}

#[derive(Debug, Clone)]
pub struct Caches {
    pub(crate) tokens: TtlCache<String>,
    pub(crate) podcasts: TtlCache<Arc<serde_json::Value>>,
    pub(crate) audiobook_meta: TtlCache<Arc<serde_json::Value>>,
    pub(crate) audiobook_audio: TtlCache<String>,
    pub head: TtlCache<HeadInfo>,
}

impl Caches {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn init(
        cache_dir: &str,
        store_tokens_on_disk: bool,
        token_ttl: u64,
        podcast_ttl: u64,
        audiobook_audio_ttl: u64,
        head_ttl: u64,
    ) -> Self {
        let root = PathBuf::from(cache_dir);
        for dir in LEGACY_DIRS {
            remove_legacy_dir(&root.join(dir)).await;
        }
        let tokens = TtlCache::new(
            "tokens",
            if store_tokens_on_disk {
                Some(root.join("tokens"))
            } else {
                None
            },
            Duration::from_secs(token_ttl),
        )
        .await;
        let podcasts = TtlCache::new(
            "podcasts",
            Some(root.join("podcasts")),
            Duration::from_secs(podcast_ttl),
        )
        .await;
        // Audiobook metadata is static-ish — reuse the podcast TTL.
        let audiobook_meta = TtlCache::new(
            "audiobook_meta",
            Some(root.join("audiobook_meta")),
            Duration::from_secs(podcast_ttl),
        )
        .await;
        // Audio URL is signed and expires upstream; keep our cache TTL short so
        // podcatchers never play through a dead link.
        let audiobook_audio = TtlCache::new(
            "audiobook_audio",
            Some(root.join("audiobook_audio")),
            Duration::from_secs(audiobook_audio_ttl),
        )
        .await;
        let head = TtlCache::new(
            "head",
            Some(root.join("head")),
            Duration::from_secs(head_ttl),
        )
        .await;

        Self {
            tokens,
            podcasts,
            audiobook_meta,
            audiobook_audio,
            head,
        }
    }
}

/// In-memory TTL cache with optional persistence to disk. Inserts write the
/// disk file before returning; reads check moka first and load from disk on a
/// miss. Expired entries stay on disk, so [`Self::get_stale`] still finds one
/// after its TTL, restarts included.
#[derive(Clone)]
pub struct TtlCache<V>
where
    V: Clone + Send + Sync + Serialize + DeserializeOwned + 'static,
{
    name: &'static str,
    inner: Cache<String, Entry<V>>,
    dir: Option<PathBuf>,
    default_ttl: Duration,
}

impl<V> std::fmt::Debug for TtlCache<V>
where
    V: Clone + Send + Sync + Serialize + DeserializeOwned + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TtlCache")
            .field("name", &self.name)
            .finish()
    }
}

/// A [`TtlCache::get_stale`] result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit<V> {
    Fresh(V),
    /// Past its TTL.
    Expired(V),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry<V> {
    expiry: u64,
    value: V,
}

impl<V> TtlCache<V>
where
    V: Clone + Send + Sync + Serialize + DeserializeOwned + 'static,
{
    pub async fn new(name: &'static str, dir: Option<PathBuf>, default_ttl: Duration) -> Self {
        let cache = Cache::builder()
            .max_capacity(10_000)
            .time_to_live(default_ttl.saturating_mul(2))
            .build();
        let this = Self {
            name,
            inner: cache,
            dir: dir.clone(),
            default_ttl,
        };

        if let Some(dir) = &dir {
            if let Err(err) = fs::create_dir_all(dir).await {
                tracing::warn!(target: "podimo::cache", "create_dir_all({}): {err}", dir.display());
            }
        }
        this
    }

    /// The value under `key`, unless it has expired. An expired entry is
    /// dropped from memory but not from disk.
    pub async fn get(&self, key: &str) -> Option<V> {
        if let Some(entry) = self.inner.get(key).await {
            if entry.expiry > now_secs() {
                return Some(entry.value);
            } else {
                self.inner.invalidate(key).await;
            }
        }
        if let Some(dir) = &self.dir {
            if let Some(entry) = read_entry::<V>(&entry_path(dir, key)).await {
                if entry.expiry > now_secs() {
                    self.inner.insert(key.to_string(), entry.clone()).await;
                    return Some(entry.value);
                }
            }
        }
        None
    }

    /// Like [`Self::get`], but an expired entry is returned too, as
    /// [`Hit::Expired`], and kept in memory. For callers that would rather use
    /// a stale value than none.
    pub async fn get_stale(&self, key: &str) -> Option<Hit<V>> {
        let entry = match self.inner.get(key).await {
            Some(entry) => entry,
            None => {
                let dir = self.dir.as_ref()?;
                let entry = read_entry::<V>(&entry_path(dir, key)).await?;
                self.inner.insert(key.to_string(), entry.clone()).await;
                entry
            }
        };
        Some(if entry.expiry > now_secs() {
            Hit::Fresh(entry.value)
        } else {
            Hit::Expired(entry.value)
        })
    }

    pub async fn insert(&self, key: String, value: V) {
        self.insert_with_ttl(key, value, self.default_ttl).await
    }

    pub async fn insert_with_ttl(&self, key: String, value: V, ttl: Duration) {
        let entry = Entry {
            expiry: now_secs() + ttl.as_secs(),
            value,
        };
        if let Some(dir) = &self.dir {
            if let Err(err) = write_entry(&entry_path(dir, &key), &entry).await {
                tracing::warn!(target: "podimo::cache", "persist {} key {}: {err}", self.name, key);
            }
        }
        self.inner.insert(key, entry).await;
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn entry_path(dir: &Path, key: &str) -> PathBuf {
    // Hash isn't required for safety here (cache keys are already opaque hashes
    // or podcast ids), but we still sanitize: replace any path separator.
    let safe: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("{safe}.json"))
}

async fn read_entry<V>(path: &Path) -> Option<Entry<V>>
where
    V: DeserializeOwned,
{
    let bytes = fs::read(path).await.ok()?;
    serde_json::from_slice::<Entry<V>>(&bytes).ok()
}

async fn write_entry<V>(path: &Path, entry: &Entry<V>) -> std::io::Result<()>
where
    V: Serialize,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let bytes = serde_json::to_vec(entry)?;
    fs::write(path, bytes).await
}

/// Where 1.2.0 and earlier kept their entries, as bincode `.bin` files.
const LEGACY_DIRS: [&str; 5] = [
    "tokens_cache",
    "podcast_cache",
    "audiobook_meta_cache",
    "audiobook_audio_cache",
    "head_cache",
];

/// Deletes the `.bin` files in a legacy cache directory, then the directory
/// itself unless something else is left in it. Nothing reads those files any
/// more, and the login tokens among them would otherwise stay on disk forever.
async fn remove_legacy_dir(dir: &Path) {
    let Ok(mut entries) = fs::read_dir(dir).await else {
        return;
    };
    let mut removed = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let is_file = entry.file_type().await.is_ok_and(|t| t.is_file());
        if is_file && path.extension().is_some_and(|ext| ext == "bin") {
            match fs::remove_file(&path).await {
                Ok(()) => removed += 1,
                Err(err) => {
                    tracing::warn!(target: "podimo::cache", "remove {}: {err}", path.display())
                }
            }
        }
    }
    if removed > 0 {
        tracing::info!(target: "podimo::cache", "removed {removed} bincode files from {}", dir.display());
    }
    // Fails, keeping the directory, if anything else is in it.
    let _ = fs::remove_dir(dir).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn insert_then_retrieve_before_expiry() {
        let cache: TtlCache<String> = TtlCache::new("test", None, Duration::from_secs(60)).await;
        cache.insert("k".into(), "v".into()).await;
        assert_eq!(cache.get("k").await, Some("v".into()));
        // Hit doesn't evict the entry.
        assert_eq!(cache.get("k").await, Some("v".into()));
    }

    #[tokio::test]
    async fn missing_key_returns_none() {
        let cache: TtlCache<String> = TtlCache::new("test", None, Duration::from_secs(60)).await;
        assert_eq!(cache.get("missing").await, None);
    }

    #[tokio::test]
    async fn expired_entry_returns_none_and_is_evicted_from_memory() {
        let cache: TtlCache<String> = TtlCache::new("test", None, Duration::from_millis(50)).await;
        cache
            .insert_with_ttl("k".into(), "v".into(), Duration::from_millis(1))
            .await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(cache.get("k").await, None);
        // After expiry, a subsequent direct check sees an evicted moka entry.
        assert!(
            cache.inner.get("k").await.is_none(),
            "key should be invalidated"
        );
    }

    #[tokio::test]
    async fn get_stale_returns_fresh_entry_as_fresh() {
        let cache: TtlCache<String> = TtlCache::new("test", None, Duration::from_secs(60)).await;
        cache.insert("k".into(), "v".into()).await;
        assert_eq!(cache.get_stale("k").await, Some(Hit::Fresh("v".into())));
        assert_eq!(cache.get_stale("missing").await, None);
    }

    #[tokio::test]
    async fn get_stale_returns_expired_entry_from_memory() {
        let cache: TtlCache<String> = TtlCache::new("test", None, Duration::from_secs(60)).await;
        // A zero TTL expires at once.
        cache
            .insert_with_ttl("k".into(), "v".into(), Duration::ZERO)
            .await;
        assert_eq!(cache.get_stale("k").await, Some(Hit::Expired("v".into())));
        // Unlike `get`, reading it doesn't evict it.
        assert_eq!(cache.get_stale("k").await, Some(Hit::Expired("v".into())));
    }

    #[tokio::test]
    async fn get_stale_loads_expired_entry_from_disk_after_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();

        {
            let c: TtlCache<String> =
                TtlCache::new("test", Some(dir.clone()), Duration::from_secs(60)).await;
            c.insert_with_ttl("k".into(), "v".into(), Duration::ZERO)
                .await;
        }
        let c2: TtlCache<String> =
            TtlCache::new("test", Some(dir.clone()), Duration::from_secs(60)).await;
        // `get` skips the expired entry but leaves its file in place.
        assert_eq!(c2.get("k").await, None);
        assert_eq!(c2.get_stale("k").await, Some(Hit::Expired("v".into())));
    }

    /// Inserts `value` into a disk-backed cache, then reads it back through a
    /// new cache on the same directory, as after a restart.
    async fn restart_round_trip<V>(value: V) -> Option<V>
    where
        V: Clone + Send + Sync + Serialize + DeserializeOwned + 'static,
    {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();

        {
            let c: TtlCache<V> =
                TtlCache::new("test", Some(dir.clone()), Duration::from_secs(60)).await;
            c.insert("key1".into(), value).await;
        }
        let c2: TtlCache<V> = TtlCache::new("test", Some(dir), Duration::from_secs(60)).await;
        c2.get("key1").await
    }

    #[tokio::test]
    async fn disk_persistence_survives_restart() {
        // One value of each type in `Caches`.
        assert_eq!(
            restart_round_trip(String::from("val1")).await,
            Some("val1".into())
        );
        let head = HeadInfo {
            content_length: "12345".into(),
            content_type: "audio/mpeg".into(),
        };
        assert_eq!(restart_round_trip(head.clone()).await, Some(head));
        // Only a self-describing format can read a `serde_json::Value` back.
        let podcast = Arc::new(serde_json::json!({
            "episodes": [{ "id": "ep1", "audio": null, "streamMedia": { "duration": 1234.5 } }],
            "podcast": { "title": "Show" },
        }));
        assert_eq!(
            restart_round_trip(Arc::clone(&podcast)).await,
            Some(podcast)
        );
    }

    #[tokio::test]
    async fn init_removes_legacy_bincode_files() {
        let tmp = tempfile::tempdir().unwrap();
        let tokens = tmp.path().join("tokens_cache");
        let head = tmp.path().join("head_cache");
        std::fs::create_dir(&tokens).unwrap();
        std::fs::create_dir(&head).unwrap();
        std::fs::write(tokens.join("abc.bin"), b"old").unwrap();
        std::fs::write(head.join("ep1.bin"), b"old").unwrap();
        std::fs::write(head.join("notes.txt"), b"not ours").unwrap();

        Caches::init(&tmp.path().to_string_lossy(), true, 60, 60, 60, 60).await;

        assert!(!tokens.exists(), "emptied legacy directory is removed");
        assert!(!head.join("ep1.bin").exists());
        assert!(head.join("notes.txt").exists(), "other files are kept");
    }
}
