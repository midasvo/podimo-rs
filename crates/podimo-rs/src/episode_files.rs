//! Complete episode files, prepared once and then served from disk.
//!
//! An M4A episode has to be assembled in full before its first byte can go
//! out (see [`hls::remux_m4a`](crate::podimo::hls::remux_m4a)). The finished
//! files stay in a private temp directory for an hour after they were last
//! requested, [`CAPACITY_BYTES`] at most, so a player that seeks or resumes
//! with a `Range` request gets them straight away. A request for an episode
//! that's still being prepared waits for that preparation instead of starting
//! another.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt, Shared};
use moka::notification::RemovalCause;
use moka::policy::EvictionPolicy;
use moka::sync::Cache;
use tempfile::TempDir;

use crate::podimo::hls::HlsError;

/// Files not requested for this long are deleted.
const TIME_TO_IDLE: Duration = Duration::from_secs(60 * 60);
/// Total size of the kept files; the least recently used go first.
const CAPACITY_BYTES: u64 = 2 << 30;

pub(crate) struct EpisodeFile {
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
}

pub(crate) type PrepareResult = Result<Arc<EpisodeFile>, Arc<HlsError>>;
type Preparation = Shared<BoxFuture<'static, PrepareResult>>;

#[derive(Clone)]
pub(crate) struct EpisodeFiles {
    inner: Arc<Inner>,
}

struct Inner {
    ready: Cache<String, Arc<EpisodeFile>>,
    preparing: Mutex<HashMap<String, Preparation>>,
    // Declared last so the directory goes after everything pointing into it.
    dir: TempDir,
}

impl EpisodeFiles {
    pub(crate) fn new() -> std::io::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("podimo-episodes-")
            .tempdir()?;
        let ready = Cache::builder()
            // LRU rather than moka's default TinyLFU, which may turn away a
            // file that was just prepared for a waiting request.
            .eviction_policy(EvictionPolicy::lru())
            .weigher(|_, file: &Arc<EpisodeFile>| {
                u32::try_from(file.size / 1024).unwrap_or(u32::MAX)
            })
            .max_capacity(CAPACITY_BYTES / 1024)
            .time_to_idle(TIME_TO_IDLE)
            .eviction_listener(|_, file: Arc<EpisodeFile>, cause| {
                // A replacement has the same path, which is still in use. A
                // file still being sent is fine to delete on Unix: the open
                // handle keeps reading it.
                if cause != RemovalCause::Replaced {
                    let _ = std::fs::remove_file(&file.path);
                }
            })
            .build();
        Ok(Self {
            inner: Arc::new(Inner {
                ready,
                preparing: Mutex::default(),
                dir,
            }),
        })
    }

    pub(crate) fn dir(&self) -> &Path {
        self.inner.dir.path()
    }

    /// The finished file stored under `key`, if there is one.
    pub(crate) fn get(&self, key: &str) -> Option<Arc<EpisodeFile>> {
        self.inner.get(key)
    }

    /// The file stored under `key` (a file name), running `prepare` to write
    /// it unless it's there already or being prepared. `prepare` gets the
    /// path to write to and runs in a task of its own, so the file is still
    /// finished and kept when every request waiting for it gives up — a
    /// player that timed out will be back.
    pub(crate) async fn get_or_prepare<F, Fut>(&self, key: &str, prepare: F) -> PrepareResult
    where
        F: FnOnce(PathBuf) -> Fut,
        Fut: Future<Output = Result<(), HlsError>> + Send + 'static,
    {
        let preparation = {
            let mut preparing = self.inner.preparing.lock().expect("not poisoned");
            // Checked under the lock: a preparation stores its file before
            // it leaves `preparing`, so one of the two has it.
            if let Some(file) = self.inner.get(key) {
                return Ok(file);
            }
            match preparing.get(key) {
                Some(preparation) => preparation.clone(),
                None => {
                    let preparation = self.start(key, prepare);
                    preparing.insert(key.to_string(), preparation.clone());
                    preparation
                }
            }
        };
        preparation.await
    }

    fn start<F, Fut>(&self, key: &str, prepare: F) -> Preparation
    where
        F: FnOnce(PathBuf) -> Fut,
        Fut: Future<Output = Result<(), HlsError>> + Send + 'static,
    {
        let inner = self.inner.clone();
        let key = key.to_string();
        let path = inner.dir.path().join(&key);
        let part = inner.dir.path().join(format!("{key}.part"));
        let work = prepare(part.clone());
        let task = tokio::spawn(async move {
            // Dropped last, after the file is in `ready`.
            let _finished = Finished {
                inner: inner.clone(),
                key: key.clone(),
            };
            let result = match work.await {
                Ok(()) => store(&part, path).await,
                Err(err) => Err(err),
            };
            if result.is_err() {
                let _ = tokio::fs::remove_file(&part).await;
            }
            let result = result.map(Arc::new).map_err(Arc::new);
            if let Ok(file) = &result {
                inner.ready.insert(key, file.clone());
            }
            result
        });
        async move {
            task.await.unwrap_or_else(|err| {
                Err(Arc::new(HlsError::Transcode(format!(
                    "preparing the episode: {err}"
                ))))
            })
        }
        .boxed()
        .shared()
    }
}

/// Takes a preparation off `preparing` when its task ends, however it ends:
/// after a panic the next request starts afresh instead of getting the same
/// failure forever.
struct Finished {
    inner: Arc<Inner>,
    key: String,
}

impl Drop for Finished {
    fn drop(&mut self) {
        if let Ok(mut preparing) = self.inner.preparing.lock() {
            preparing.remove(&self.key);
        }
    }
}

impl Inner {
    fn get(&self, key: &str) -> Option<Arc<EpisodeFile>> {
        let file = self.ready.get(key)?;
        if file.path.exists() {
            Some(file)
        } else {
            // Deleted behind our back (a temp dir cleaner, say): prepare anew.
            self.ready.invalidate(key);
            None
        }
    }
}

async fn store(part: &Path, path: PathBuf) -> Result<EpisodeFile, HlsError> {
    let failed = |err: std::io::Error| HlsError::Transcode(format!("storing the episode: {err}"));
    tokio::fs::rename(part, &path).await.map_err(failed)?;
    let size = tokio::fs::metadata(&path).await.map_err(failed)?.len();
    Ok(EpisodeFile { path, size })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn write(
        content: &'static [u8],
    ) -> impl FnOnce(PathBuf) -> BoxFuture<'static, Result<(), HlsError>> {
        move |dest| {
            async move {
                tokio::fs::write(&dest, content).await.unwrap();
                Ok(())
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn prepares_once_and_then_serves_the_stored_file() {
        let files = EpisodeFiles::new().unwrap();
        let file = files
            .get_or_prepare("a.m4a", write(b"audio"))
            .await
            .unwrap();
        assert_eq!(file.path, files.dir().join("a.m4a"));
        assert_eq!(file.size, 5);
        assert_eq!(std::fs::read(&file.path).unwrap(), b"audio");
        assert!(!files.dir().join("a.m4a.part").exists());

        let again = files
            .get_or_prepare("a.m4a", |_| async { panic!("prepared twice") })
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&file, &again));
        assert!(files.get("a.m4a").is_some());
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_preparation() {
        let files = EpisodeFiles::new().unwrap();
        let runs = Arc::new(AtomicUsize::new(0));
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let released = released.shared();

        let request = |files: EpisodeFiles| {
            let runs = runs.clone();
            let released = released.clone();
            async move {
                files
                    .get_or_prepare("a.m4a", move |dest| async move {
                        runs.fetch_add(1, Ordering::SeqCst);
                        let _ = released.await;
                        tokio::fs::write(&dest, b"audio").await.unwrap();
                        Ok(())
                    })
                    .await
            }
        };
        let first = tokio::spawn(request(files.clone()));
        let second = tokio::spawn(request(files.clone()));
        tokio::task::yield_now().await;
        release.send(()).unwrap();
        let (first, second) = (first.await.unwrap(), second.await.unwrap());
        assert!(Arc::ptr_eq(&first.unwrap(), &second.unwrap()));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_preparation_outlives_the_request_that_started_it() {
        let files = EpisodeFiles::new().unwrap();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let request = files.get_or_prepare("a.m4a", move |dest| async move {
            let _ = released.await;
            tokio::fs::write(&dest, b"audio").await.unwrap();
            Ok(())
        });
        // The client gives up before the file is ready.
        assert!(tokio::time::timeout(Duration::from_millis(10), request)
            .await
            .is_err());
        release.send(()).unwrap();

        let file = files
            .get_or_prepare("a.m4a", |_| async { panic!("prepared twice") })
            .await
            .unwrap();
        assert_eq!(std::fs::read(&file.path).unwrap(), b"audio");
    }

    #[tokio::test]
    async fn failures_are_not_kept() {
        let files = EpisodeFiles::new().unwrap();
        let err = files
            .get_or_prepare("a.m4a", |dest| async move {
                tokio::fs::write(&dest, b"half").await.unwrap();
                Err(HlsError::Upstream("segment gone".into()))
            })
            .await
            .err()
            .unwrap();
        assert!(matches!(*err, HlsError::Upstream(_)));
        assert!(
            !files.dir().join("a.m4a.part").exists(),
            "partial file removed"
        );
        assert!(files.get("a.m4a").is_none());

        let file = files
            .get_or_prepare("a.m4a", write(b"audio"))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&file.path).unwrap(), b"audio");
    }

    #[tokio::test]
    async fn a_crashed_preparation_is_retried() {
        let files = EpisodeFiles::new().unwrap();
        let err = files
            .get_or_prepare("a.m4a", |_| async { panic!("crashed") })
            .await
            .err()
            .unwrap();
        assert!(matches!(*err, HlsError::Transcode(_)));
        let file = files
            .get_or_prepare("a.m4a", write(b"audio"))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&file.path).unwrap(), b"audio");
    }

    #[tokio::test]
    async fn a_deleted_file_is_prepared_again() {
        let files = EpisodeFiles::new().unwrap();
        let file = files
            .get_or_prepare("a.m4a", write(b"audio"))
            .await
            .unwrap();
        std::fs::remove_file(&file.path).unwrap();
        assert!(files.get("a.m4a").is_none());
        let file = files
            .get_or_prepare("a.m4a", write(b"again"))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&file.path).unwrap(), b"again");
    }

    #[tokio::test]
    async fn evicted_files_are_deleted() {
        let files = EpisodeFiles::new().unwrap();
        let file = files
            .get_or_prepare("a.m4a", write(b"audio"))
            .await
            .unwrap();
        files.inner.ready.invalidate("a.m4a");
        files.inner.ready.run_pending_tasks();
        assert!(!file.path.exists());
    }
}
