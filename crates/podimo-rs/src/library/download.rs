//! Background download task: fetches the signed audio URL + cover image and
//! streams them to disk, updating the library entry's status as it goes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::cache::Caches;
use crate::config::Config;
use crate::library::{Library, Status};
use crate::podimo::PodimoClient;

/// Throttle for in-progress `audio_downloaded_bytes` writes. We update the
/// library entry's progress at most once per interval so the UI gets fresh
/// numbers without thrashing the write lock for every chunk.
const PROGRESS_UPDATE_INTERVAL: Duration = Duration::from_millis(500);

/// Spawned download task. Owns the work of:
///   1. Re-minting the short-lived signed audio URL (and a cover URL from meta).
///   2. Streaming the audio to `audio.mp3.partial`, then atomically renaming.
///   3. Downloading the cover.
///   4. Marking the entry `Done` or `Failed`.
///
/// Errors don't propagate — they're recorded on the entry. The library is
/// already in `Queued` state when this is called; we set `Downloading` once
/// the first GraphQL hop succeeds.
pub async fn run(
    library: Library,
    podimo: PodimoClient,
    scraper: reqwest::Client,
    config: Arc<Config>,
    caches: Caches,
    audiobook_id: String,
    cover_url: Option<String>,
) {
    tracing::info!(target: "podimo::library", "starting download for audiobook {audiobook_id}");
    let start = Instant::now();
    let result = perform(
        &library,
        &podimo,
        &scraper,
        &config,
        &caches,
        &audiobook_id,
        cover_url.as_deref(),
    )
    .await;

    match &result {
        Ok(size) => {
            let mb = *size as f64 / (1024.0 * 1024.0);
            tracing::info!(
                target: "podimo::library",
                "completed download for audiobook {audiobook_id} ({mb:.2} MB in {:.1?})",
                start.elapsed()
            );
        }
        Err(msg) => {
            tracing::error!(
                target: "podimo::library",
                "download failed for audiobook {audiobook_id}: {msg}"
            );
        }
    }

    let _ = library
        .update(&audiobook_id, |e| match &result {
            Ok(size) => {
                e.status = Status::Done;
                e.audio_size_bytes = Some(*size);
                e.audio_downloaded_bytes = *size;
                e.error = None;
            }
            Err(msg) => {
                e.status = Status::Failed;
                e.error = Some(msg.clone());
            }
        })
        .await;
}

async fn perform(
    library: &Library,
    podimo: &PodimoClient,
    scraper: &reqwest::Client,
    config: &Config,
    caches: &Caches,
    audiobook_id: &str,
    cover_url: Option<&str>,
) -> Result<u64, String> {
    // Flip to Downloading as soon as we start; failures after this point still
    // overwrite the status to Failed.
    if library
        .update(audiobook_id, |e| {
            e.status = Status::Downloading;
            e.error = None;
            e.audio_downloaded_bytes = 0;
        })
        .await
        .map_err(|e| format!("library update: {e}"))?
        .is_none()
    {
        return Err("entry vanished mid-download".into());
    }

    let audio_url = match podimo
        .get_audiobook_audio_url(scraper, config, audiobook_id, &caches.audiobook_audio)
        .await
    {
        Ok(url) => url,
        Err(err) => {
            // The token may be a cached one upstream has revoked since: drop
            // it so the next attempt logs in again.
            if !err.is_not_found() {
                caches.tokens.remove(&podimo.key).await;
            }
            return Err(format!("fetch audio url: {err}"));
        }
    };

    // Cover is best-effort — a failed cover should not kill the audio download.
    if let Some(url) = cover_url {
        let cover_dest = library
            .cover_path_for(audiobook_id)
            .await
            .ok_or_else(|| "entry vanished mid-download".to_string())?;
        if let Err(err) = download_cover(scraper, url, &cover_dest).await {
            tracing::warn!(target: "podimo::library", "cover download failed for {audiobook_id}: {err}");
        }
    }

    let audio_size = download_audio(scraper, &audio_url, library, audiobook_id)
        .await
        .map_err(|e| format!("audio download: {e}"))?;

    Ok(audio_size)
}

/// Per-request override for the audio download. The shared `scraper` Client
/// has a 30 s total-request timeout (good for GraphQL + HEAD), but the same
/// limit applied to a multi-GB audio body cuts the stream off mid-flight and
/// surfaces as `error decoding response body`. Four hours is plenty for any
/// audiobook on any reasonable connection without being indefinite.
const AUDIO_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(4 * 3600);

/// Same idea but smaller — cover images are typically <500 KB, but a 30 s
/// limit shared with the rest of the client is still too tight if the CDN
/// is slow on the day. One minute is comfortably generous.
const COVER_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

async fn download_audio(
    scraper: &reqwest::Client,
    url: &str,
    library: &Library,
    audiobook_id: &str,
) -> anyhow::Result<u64> {
    // Take a snapshot of the entry to resolve the on-disk paths — author/title
    // are stable for an entry's lifetime, so it's fine to compute paths once.
    let entry = library
        .get(audiobook_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("entry vanished mid-download"))?;
    let partial = library.audio_partial_path(&entry);
    let final_path = library.audio_path(&entry);

    // Per-request timeout overrides the client default. Without this the
    // streamed body would inherit the scraper Client's 30 s total-request
    // timeout and any audiobook over ~50 MB on a typical line would fail
    // mid-stream with `error decoding response body`.
    let response = scraper
        .get(url)
        .timeout(AUDIO_DOWNLOAD_TIMEOUT)
        .send()
        .await?
        .error_for_status()?;
    let content_length = response.content_length();

    if let Some(total) = content_length {
        let _ = library
            .update(audiobook_id, |e| {
                e.audio_size_bytes = Some(total);
            })
            .await;
    }

    let mut file = fs::File::create(&partial).await?;
    let mut stream = response.bytes_stream();
    let mut written: u64 = 0;
    let mut last_progress_push = Instant::now();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;
        if last_progress_push.elapsed() >= PROGRESS_UPDATE_INTERVAL {
            last_progress_push = Instant::now();
            let _ = library
                .update(audiobook_id, |e| {
                    e.audio_downloaded_bytes = written;
                })
                .await;
        }
    }
    file.flush().await?;
    drop(file);

    fs::rename(&partial, &final_path).await?;
    Ok(written)
}

async fn download_cover(
    scraper: &reqwest::Client,
    url: &str,
    dest: &std::path::Path,
) -> anyhow::Result<()> {
    let bytes = scraper
        .get(url)
        .timeout(COVER_DOWNLOAD_TIMEOUT)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).await?;
    }
    let tmp = dest.with_extension("jpg.partial");
    fs::write(&tmp, &bytes).await?;
    fs::rename(&tmp, dest).await?;
    Ok(())
}
