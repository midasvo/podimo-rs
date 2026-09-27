//! HLS → progressive audio for podcast episodes.
//!
//! Podimo serves episode audio as HLS: a master playlist with a few bitrate
//! variants, each a media playlist of ~10 s MPEG-TS segments carrying AAC.
//! Podcatchers want a single progressive file, so we walk the
//! highest-bandwidth variant, strip the TS/PES framing to get raw ADTS AAC,
//! and pipe that through `ffmpeg` into the feed's [`StreamFormat`]: MP3
//! (re-encoded, streamed as it's produced) or M4A (the same AAC, repackaged
//! into a complete file). ffmpeg reads stdin and writes stdout or a local
//! file; all network I/O stays in here.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use futures::{Stream, StreamExt};
use once_cell::sync::Lazy;
use reqwest::{Client, Url};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::io::ReaderStream;
use tokio_util::task::AbortOnDropHandle;

/// Content type of the legacy raw AAC stream (`/stream/<id>.aac`).
pub const AAC_CONTENT_TYPE: &str = "audio/aac";

/// Looked up on `PATH`; the Docker image installs Debian's ffmpeg.
const FFMPEG: &str = "ffmpeg";
/// Simultaneous `/stream` responses of any format. Each one holds an ffmpeg
/// process (except the legacy .aac route) and a few upstream connections.
/// M4A is a stream copy, so this can be well above the MP3 limit.
const MAX_CONCURRENT_STREAMS: usize = 16;
/// Simultaneous MP3 encodes, each on top of a stream slot. M4A only
/// repackages and doesn't take one.
const MAX_CONCURRENT_TRANSCODES: usize = 4;
/// How long a request waits for a free slot before it gets a 503.
const SLOT_WAIT: Duration = Duration::from_secs(10);
/// How much of ffmpeg's stderr to keep for the error message.
const STDERR_TAIL_BYTES: usize = 2048;

static STREAM_SLOTS: Lazy<Arc<Semaphore>> =
    Lazy::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS)));
static TRANSCODE_SLOTS: Lazy<Arc<Semaphore>> =
    Lazy::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_TRANSCODES)));

/// Only playlists, segments and redirects on Podimo's own hosts are fetched,
/// so `/stream` can't be abused as an open relay.
const ALLOWED_HOST_SUFFIX: &str = ".podimo.com";

/// Segments fetched ahead of the one currently being written to the client.
pub const SEGMENT_PREFETCH: usize = 4;
/// Segments fetched at once while assembling a complete file, where nothing
/// is sent until the last one is in.
pub const PREPARE_PREFETCH: usize = 8;
const SEGMENT_RETRIES: u32 = 3;
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

const TS_PACKET_LEN: usize = 188;
const TS_SYNC_BYTE: u8 = 0x47;
const PAT_PID: u16 = 0x0000;
const PMT_TABLE_ID: u8 = 0x02;
const STREAM_TYPE_ADTS_AAC: u8 = 0x0F;

#[derive(Debug, Error)]
pub enum HlsError {
    #[error("invalid stream source: {0}")]
    InvalidSource(&'static str),
    #[error("upstream: {0}")]
    Upstream(String),
    #[error("unsupported stream: {0}")]
    Unsupported(String),
    #[error("demux: {0}")]
    Demux(&'static str),
    #[error("transcode: {0}")]
    Transcode(String),
    #[error("all stream slots are busy")]
    Busy,
}

/// What `/stream/<token>/<id>.<ext>` hands out. Both are always served; the
/// `STREAM_FORMAT` setting only picks which one the feed links to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamFormat {
    /// Re-encoded with LAME ([`transcode_mp3`]): plays everywhere, costs CPU.
    Mp3,
    /// Podimo's AAC untouched in an MP4 ([`remux_m4a`]): near-zero CPU,
    /// original quality, and an index players can seek with.
    M4a,
}

impl StreamFormat {
    /// The format for a file extension (`mp3`, `m4a`); also parses the
    /// `STREAM_FORMAT` setting.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "mp3" => Some(Self::Mp3),
            "m4a" => Some(Self::M4a),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::M4a => "m4a",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Mp3 => "audio/mpeg",
            // What Apple's podcast spec (and so most podcatchers) expects.
            Self::M4a => "audio/x-m4a",
        }
    }
}

/// ffmpeg options for ADTS AAC on stdin, before the output options. `-y`
/// replaces whatever an interrupted earlier attempt left at the output path.
const FFMPEG_INPUT_ARGS: &[&str] = &[
    "-hide_banner",
    "-nostats",
    "-loglevel",
    "error",
    "-y",
    "-f",
    "aac",
    "-i",
    "pipe:0",
    "-map",
    "0:a:0",
];

/// LAME at -q 7 costs ~40% less CPU than its default for no audible
/// difference on speech. CBR keeps player duration estimates exact, since
/// there's no Xing header on a pipe.
const MP3_OUTPUT_ARGS: &[&str] = &[
    "-c:a",
    "libmp3lame",
    "-b:a",
    "128k",
    "-compression_level",
    "7",
    "-f",
    "mp3",
];

/// Stream copy into a regular MP4 with its index (`moov`) moved to the
/// front, which takes a seekable output: a file, not a pipe.
const M4A_OUTPUT_ARGS: &[&str] = &[
    "-c:a",
    "copy",
    "-bsf:a",
    "aac_adtstoasc",
    "-movflags",
    "+faststart",
    "-f",
    "mp4",
];

/// True when `url` points at an HLS playlist rather than a progressive file.
pub fn is_hls_url(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.ends_with(".m3u8")
}

/// The proxy URL a feed enclosure should point at for an HLS episode:
/// `{base}/stream/<token>/<episode_id>.<ext>`, where `token` is the signed
/// playlist URL in unpadded base64url. Keeping it in the path means the URL
/// ends in a plain `.mp3`/`.m4a` with no query string, so clients that sniff
/// the extension (Audiobookshelf does) never see the `.m3u8` inside it.
pub fn stream_enclosure_url(
    base_url: &str,
    episode_id: &str,
    hls_url: &str,
    format: StreamFormat,
) -> String {
    format!(
        "{base_url}/stream/{}/{}.{}",
        URL_SAFE_NO_PAD.encode(hls_url),
        urlencoding::encode(episode_id),
        format.extension()
    )
}

/// Inverse of the token in [`stream_enclosure_url`]. The result still needs
/// [`validate_source`].
pub fn decode_source_token(token: &str) -> Result<String, HlsError> {
    URL_SAFE_NO_PAD
        .decode(token)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or(HlsError::InvalidSource("malformed token"))
}

/// Parse and vet the `src` query parameter of a `/stream` request.
pub fn validate_source(src: &str) -> Result<Url, HlsError> {
    let url = Url::parse(src).map_err(|_| HlsError::InvalidSource("not a URL"))?;
    check_podimo_https(&url)?;
    if !is_hls_url(src) {
        return Err(HlsError::InvalidSource("not an .m3u8 playlist"));
    }
    Ok(url)
}

/// Every URL `/stream` fetches must be https on one of Podimo's hosts.
fn check_podimo_https(url: &Url) -> Result<(), HlsError> {
    if url.scheme() != "https" {
        return Err(HlsError::InvalidSource("must be https"));
    }
    let host = url.host_str().unwrap_or_default();
    if !host.ends_with(ALLOWED_HOST_SUFFIX) {
        return Err(HlsError::InvalidSource("host not allowed"));
    }
    Ok(())
}

/// A file-name-safe key for the audio behind a validated `src`: its host and
/// path, without the signature, which changes whenever Podimo re-signs the
/// same playlist. Derived from the playlist rather than the episode ID in
/// the request, so a request can't file one episode's audio under another's
/// name.
pub fn source_key(src: &Url) -> String {
    let mut hasher = Sha256::new();
    hasher.update(src.host_str().unwrap_or_default().as_bytes());
    hasher.update(src.path().as_bytes());
    hex::encode(hasher.finalize())
}

/// Fetch the playlist at `src` and return the segment URLs of its
/// highest-bandwidth variant (or of `src` itself if it's a media playlist).
pub async fn resolve_segments(client: &Client, src: &Url) -> Result<Vec<Url>, HlsError> {
    let text = fetch_text(client, src).await?;
    let (media_url, text) = match select_variant(&text, src)? {
        Some(variant) => {
            let text = fetch_text(client, &variant).await?;
            (variant, text)
        }
        None => (src.clone(), text),
    };
    let segments = media_segments(&text, &media_url)?;
    if segments.is_empty() {
        return Err(HlsError::Unsupported(
            "media playlist has no segments".into(),
        ));
    }
    Ok(segments)
}

/// For a master playlist, the URL of the highest-`BANDWIDTH` variant; `None`
/// when `text` is already a media playlist.
pub fn select_variant(text: &str, base: &Url) -> Result<Option<Url>, HlsError> {
    let mut best: Option<(u64, &str)> = None;
    let mut pending_bandwidth: Option<u64> = None;
    for line in text.lines().map(str::trim) {
        if let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending_bandwidth = Some(attribute(attrs, "BANDWIDTH").unwrap_or(0));
        } else if !line.is_empty() && !line.starts_with('#') {
            if let Some(bandwidth) = pending_bandwidth.take() {
                if best.is_none_or(|(b, _)| bandwidth > b) {
                    best = Some((bandwidth, line));
                }
            }
        }
    }
    best.map(|(_, uri)| resolve_entry(base, uri)).transpose()
}

/// Segment URLs of a media playlist, resolved against `base`.
pub fn media_segments(text: &str, base: &Url) -> Result<Vec<Url>, HlsError> {
    let mut segments = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(attrs) = line.strip_prefix("#EXT-X-KEY:") {
            let method = attrs
                .split(',')
                .find_map(|kv| kv.strip_prefix("METHOD="))
                .unwrap_or("NONE");
            if method != "NONE" {
                return Err(HlsError::Unsupported(format!("encrypted ({method})")));
            }
        } else if line.starts_with("#EXT-X-MAP:") {
            return Err(HlsError::Unsupported("fragmented MP4 segments".into()));
        } else if !line.is_empty() && !line.starts_with('#') {
            segments.push(resolve_entry(base, line)?);
        }
    }
    Ok(segments)
}

/// Stream the concatenated ADTS AAC of `segments`, fetching `prefetch` at a
/// time. An `Err` item aborts the response mid-body, so a client never
/// mistakes a truncated episode for a complete one.
///
/// Each fetch runs as its own task. A future inside `buffered` only makes
/// progress while the stream is polled, and a slow or paused client can keep
/// the consumer away for longer than a fetch's timeout: the prefetched
/// download would then fail on that timeout even though the CDN answered
/// long ago. A task finishes on its own and holds its bytes until they're
/// wanted. Dropping the stream aborts the tasks.
pub fn aac_stream(
    client: Client,
    segments: Vec<Url>,
    prefetch: usize,
) -> impl Stream<Item = Result<Bytes, HlsError>> {
    aac_stream_with_timeout(client, segments, prefetch, FETCH_TIMEOUT)
}

fn aac_stream_with_timeout(
    client: Client,
    segments: Vec<Url>,
    prefetch: usize,
    fetch_timeout: Duration,
) -> impl Stream<Item = Result<Bytes, HlsError>> {
    let mut demuxer = TsDemuxer::default();
    futures::stream::iter(segments)
        .map(move |url| {
            let client = client.clone();
            AbortOnDropHandle::new(tokio::spawn(async move {
                fetch_segment(&client, &url, fetch_timeout).await
            }))
        })
        .buffered(prefetch)
        .map(move |joined| {
            let segment =
                joined.map_err(|err| HlsError::Upstream(format!("segment task: {err}")))??;
            let mut out = Vec::with_capacity(segment.len());
            demuxer.push(&segment, &mut out)?;
            Ok(Bytes::from(out))
        })
}

/// Re-encode an ADTS AAC stream to MP3 with ffmpeg, streaming the output as
/// it's produced. First waits for a stream slot and a transcode slot;
/// [`HlsError::Busy`] if one doesn't free up in time. Both are held until the
/// stream ends or is dropped.
///
/// Failures — upstream errors in `aac` as well as ffmpeg itself failing —
/// arrive as a trailing `Err` item, so the response is cut short instead of
/// ending in a truncated file that looks complete. Dropping the stream (the
/// client went away) kills ffmpeg and stops the upstream fetch.
pub async fn transcode_mp3<S>(
    aac: S,
) -> Result<impl Stream<Item = Result<Bytes, HlsError>>, HlsError>
where
    S: Stream<Item = Result<Bytes, HlsError>> + Send + 'static,
{
    let stream_slot = acquire_slot(&STREAM_SLOTS, SLOT_WAIT).await?;
    let transcode_slot = acquire_slot(&TRANSCODE_SLOTS, SLOT_WAIT).await?;
    let mut ffmpeg = Ffmpeg::spawn(aac, MP3_OUTPUT_ARGS, "pipe:1", Stdio::piped())?;
    let stdout = ffmpeg.child.stdout.take().expect("stdout is piped");

    let output = ReaderStream::with_capacity(stdout, 64 * 1024).map(|chunk| {
        chunk.map_err(|err| HlsError::Transcode(format!("reading ffmpeg output: {err}")))
    });

    // Polled once ffmpeg has closed stdout, i.e. when it's finished.
    let outcome = futures::stream::once(async move {
        let _slots = (stream_slot, transcode_slot);
        ffmpeg.finish().await
    })
    .filter_map(|outcome| async move { outcome.err().map(Err) });

    Ok(output.chain(outcome))
}

/// Repackage an ADTS AAC stream into an MP4 file at `dest`, index first.
/// First waits for a stream slot; [`HlsError::Busy`] if one doesn't free up
/// in time.
///
/// A complete file rather than a fragmented stream, because that's what
/// players can seek in: ExoPlayer (AntennaPod and most Android podcast apps)
/// treats a fragmented MP4 without a segment index as unseekable, so resuming
/// or skipping ahead jumps back to the start. The index needs every sample,
/// so nothing can be served before the whole episode is in.
///
/// On failure `dest` may hold a partial file, which is the caller's to
/// remove.
pub async fn remux_m4a<S>(aac: S, dest: &Path) -> Result<(), HlsError>
where
    S: Stream<Item = Result<Bytes, HlsError>> + Send + 'static,
{
    let _stream_slot = acquire_slot(&STREAM_SLOTS, SLOT_WAIT).await?;
    Ffmpeg::spawn(aac, M4A_OUTPUT_ARGS, dest, Stdio::null())?
        .finish()
        .await
}

/// An ffmpeg process reading ADTS AAC on stdin, fed by a background task.
struct Ffmpeg {
    child: Child,
    feeder: AbortOnDropHandle<Result<(), HlsError>>,
    stderr_tail: AbortOnDropHandle<String>,
}

impl Ffmpeg {
    fn spawn<S>(
        aac: S,
        output_args: &[&str],
        output: impl AsRef<std::ffi::OsStr>,
        stdout: Stdio,
    ) -> Result<Self, HlsError>
    where
        S: Stream<Item = Result<Bytes, HlsError>> + Send + 'static,
    {
        let mut child = Command::new(FFMPEG)
            .args(FFMPEG_INPUT_ARGS)
            .args(output_args)
            .arg(output)
            .stdin(Stdio::piped())
            .stdout(stdout)
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| HlsError::Transcode(format!("could not start {FFMPEG}: {err}")))?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let feeder = AbortOnDropHandle::new(tokio::spawn(feed_ffmpeg(aac, stdin)));
        // Drained concurrently: a chatty ffmpeg would otherwise block on a
        // full stderr pipe and stall.
        let stderr_tail = AbortOnDropHandle::new(tokio::spawn(read_tail(stderr)));
        Ok(Self {
            child,
            feeder,
            stderr_tail,
        })
    }

    /// Wait for ffmpeg to exit. Input cut short by an upstream error counts
    /// as a failure too, although ffmpeg itself ends cleanly on that EOF.
    async fn finish(mut self) -> Result<(), HlsError> {
        let status = self
            .child
            .wait()
            .await
            .map_err(|err| HlsError::Transcode(format!("waiting for ffmpeg: {err}")))?;
        if !status.success() {
            let stderr = self.stderr_tail.await.unwrap_or_default();
            return Err(HlsError::Transcode(format!(
                "ffmpeg exited with {status}: {stderr}"
            )));
        }
        // A clean exit means ffmpeg saw EOF on stdin, so the feeder is done.
        self.feeder
            .await
            .map_err(|err| HlsError::Transcode(format!("feeding ffmpeg: {err}")))?
    }
}

/// `body` holding a stream slot until it's dropped, for the legacy `.aac`
/// route, which streams without ffmpeg. [`HlsError::Busy`] if no slot frees
/// up in time.
pub(crate) async fn with_stream_slot<S: Stream>(
    body: S,
) -> Result<impl Stream<Item = S::Item>, HlsError> {
    let slot = acquire_slot(&STREAM_SLOTS, SLOT_WAIT).await?;
    Ok(holding(slot, body))
}

/// `body`, keeping `slot` until the stream is dropped.
fn holding<S: Stream>(slot: OwnedSemaphorePermit, body: S) -> impl Stream<Item = S::Item> {
    body.map(move |item| {
        let _slot = &slot;
        item
    })
}

/// A permit from `slots`, waiting at most `wait` for one to free up.
async fn acquire_slot(
    slots: &Arc<Semaphore>,
    wait: Duration,
) -> Result<OwnedSemaphorePermit, HlsError> {
    match tokio::time::timeout(wait, Arc::clone(slots).acquire_owned()).await {
        Ok(permit) => Ok(permit.expect("slot semaphores are never closed")),
        Err(_) => Err(HlsError::Busy),
    }
}

/// Log which ffmpeg will convert episodes, or warn that there is none,
/// so a missing binary shows up at startup instead of on the first download.
pub async fn log_ffmpeg_status() {
    let result = Command::new(FFMPEG)
        .arg("-version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await;
    match result {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let version: Vec<&str> = stdout.split_whitespace().take(3).collect();
            tracing::info!(target: "podimo", "episode conversion via {}", version.join(" "));
        }
        Ok(out) => tracing::warn!(
            target: "podimo",
            "`{FFMPEG} -version` failed ({}); HLS episodes can't be served",
            out.status
        ),
        Err(err) => tracing::warn!(
            target: "podimo",
            "{FFMPEG} not found ({err}); HLS episodes can't be served"
        ),
    }
}

async fn feed_ffmpeg<S>(aac: S, mut stdin: ChildStdin) -> Result<(), HlsError>
where
    S: Stream<Item = Result<Bytes, HlsError>>,
{
    let mut aac = std::pin::pin!(aac);
    while let Some(chunk) = aac.next().await {
        stdin
            .write_all(&chunk?)
            .await
            .map_err(|err| HlsError::Transcode(format!("writing to ffmpeg: {err}")))?;
    }
    // Returning drops stdin, closing the pipe: ffmpeg sees EOF and flushes.
    Ok(())
}

/// Read `stream` to the end, keeping the last few KB as text.
async fn read_tail(mut stream: impl AsyncRead + Unpin) -> String {
    let mut tail = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n @ 1..) = stream.read(&mut buf).await {
        tail.extend_from_slice(&buf[..n]);
        let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
        tail.drain(..excess);
    }
    String::from_utf8_lossy(&tail).trim().to_string()
}

async fn fetch_text(client: &Client, url: &Url) -> Result<String, HlsError> {
    let resp = client
        .get(url.clone())
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|err| HlsError::Upstream(describe(err)))?;
    check_redirect(url, &resp)?;
    resp.error_for_status()
        .map_err(|err| HlsError::Upstream(describe(err)))?
        .text()
        .await
        .map_err(|err| HlsError::Upstream(describe(err)))
}

async fn fetch_segment(client: &Client, url: &Url, timeout: Duration) -> Result<Bytes, HlsError> {
    let mut last_err = None;
    for attempt in 0..SEGMENT_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1 << (attempt - 1))).await;
        }
        let result = client.get(url.clone()).timeout(timeout).send().await;
        let result = match result {
            Ok(resp) => {
                // Not worth a retry: the same URL redirects the same way.
                check_redirect(url, &resp)?;
                match resp.error_for_status() {
                    Ok(resp) => resp.bytes().await,
                    Err(err) => Err(err),
                }
            }
            Err(err) => Err(err),
        };
        match result {
            Ok(bytes) => return Ok(bytes),
            Err(err) => {
                let msg = describe(err);
                tracing::info!(
                    target: "podimo",
                    "segment fetch failed (attempt {}/{SEGMENT_RETRIES}): {msg}",
                    attempt + 1,
                );
                last_err = Some(msg);
            }
        }
    }
    Err(HlsError::Upstream(
        last_err.expect("loop ran at least once"),
    ))
}

/// Error text without the signed query string, which would otherwise end up
/// in logs verbatim.
fn describe(err: reqwest::Error) -> String {
    let location = err
        .url()
        .map(|url| format!(" ({}{})", url.host_str().unwrap_or("?"), url.path()));
    let mut msg = err.without_url().to_string();
    msg.push_str(&location.unwrap_or_default());
    msg
}

/// The client follows redirects, so a vetted URL can still be answered from
/// somewhere else. Refuse a response that ended up off Podimo's hosts.
fn check_redirect(requested: &Url, resp: &reqwest::Response) -> Result<(), HlsError> {
    if resp.url() == requested {
        return Ok(());
    }
    check_podimo_https(resp.url())
        .map_err(|_| HlsError::InvalidSource("redirected to a foreign host"))
}

/// A playlist entry resolved against the playlist's URL. The playlist body
/// decides where it points, so it gets the same check as the playlist.
fn resolve_entry(base: &Url, uri: &str) -> Result<Url, HlsError> {
    let url = base
        .join(uri)
        .map_err(|_| HlsError::Unsupported(format!("unresolvable playlist entry: {uri}")))?;
    check_podimo_https(&url)
        .map_err(|_| HlsError::InvalidSource("playlist entry on a foreign host"))?;
    Ok(url)
}

fn attribute(attrs: &str, name: &str) -> Option<u64> {
    attrs
        .split(',')
        .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
        .and_then(|v| v.parse().ok())
}

/// Minimal MPEG-TS demuxer: follows PAT → PMT to the first ADTS AAC
/// elementary stream and emits its PES payloads. State carries across
/// `push` calls, but every HLS segment repeats PAT/PMT anyway.
#[derive(Debug, Default)]
pub struct TsDemuxer {
    pmt_pid: Option<u16>,
    audio_pid: Option<u16>,
}

impl TsDemuxer {
    /// Append the AAC payload carried in `data` (one or more whole TS
    /// packets) to `out`.
    pub fn push(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(), HlsError> {
        for packet in data.as_chunks::<TS_PACKET_LEN>().0 {
            if packet[0] != TS_SYNC_BYTE {
                return Err(HlsError::Demux("lost MPEG-TS sync"));
            }
            let unit_start = packet[1] & 0x40 != 0;
            let pid = (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2]);
            let payload_start = match (packet[3] >> 4) & 0x3 {
                0b01 => 4,
                0b11 => 5 + usize::from(packet[4]),
                _ => continue, // adaptation field only, or reserved
            };
            let Some(payload) = packet.get(payload_start..) else {
                continue;
            };

            if pid == PAT_PID {
                if unit_start {
                    self.pmt_pid = parse_pat(payload).or(self.pmt_pid);
                }
            } else if Some(pid) == self.pmt_pid {
                if unit_start {
                    self.audio_pid = parse_pmt(payload)?.or(self.audio_pid);
                }
            } else if Some(pid) == self.audio_pid {
                if unit_start {
                    out.extend_from_slice(strip_pes_header(payload)?);
                } else {
                    out.extend_from_slice(payload);
                }
            }
        }
        if self.audio_pid.is_none() {
            return Err(HlsError::Demux("no AAC stream found"));
        }
        Ok(())
    }
}

/// PSI section body (after the pointer field), trimmed to `section_length`
/// minus the trailing CRC.
fn psi_section(payload: &[u8]) -> Option<&[u8]> {
    let pointer = usize::from(*payload.first()?);
    let section = payload.get(1 + pointer..)?;
    let len = (usize::from(*section.get(1)? & 0x0F) << 8) | usize::from(*section.get(2)?);
    let end = (3 + len).saturating_sub(4).min(section.len());
    section.get(..end)
}

fn parse_pat(payload: &[u8]) -> Option<u16> {
    let section = psi_section(payload)?;
    let (entries, _) = section.get(8..)?.as_chunks::<4>();
    entries.iter().find_map(|entry| {
        let program = (u16::from(entry[0]) << 8) | u16::from(entry[1]);
        (program != 0).then(|| (u16::from(entry[2] & 0x1F) << 8) | u16::from(entry[3]))
    })
}

fn parse_pmt(payload: &[u8]) -> Result<Option<u16>, HlsError> {
    let Some(section) = psi_section(payload) else {
        return Ok(None);
    };
    if section.first() != Some(&PMT_TABLE_ID) || section.len() < 12 {
        return Ok(None);
    }
    let info_len = (usize::from(section[10] & 0x0F) << 8) | usize::from(section[11]);
    let mut i = 12 + info_len;
    let mut seen = Vec::new();
    while i + 5 <= section.len() {
        let stream_type = section[i];
        let pid = (u16::from(section[i + 1] & 0x1F) << 8) | u16::from(section[i + 2]);
        if stream_type == STREAM_TYPE_ADTS_AAC {
            return Ok(Some(pid));
        }
        seen.push(format!("0x{stream_type:02x}"));
        let es_info_len = (usize::from(section[i + 3] & 0x0F) << 8) | usize::from(section[i + 4]);
        i += 5 + es_info_len;
    }
    Err(HlsError::Unsupported(format!(
        "no ADTS AAC stream (stream types: {})",
        seen.join(", ")
    )))
}

fn strip_pes_header(payload: &[u8]) -> Result<&[u8], HlsError> {
    if payload.len() < 9 || payload[..3] != [0x00, 0x00, 0x01] {
        return Err(HlsError::Demux("malformed PES header"));
    }
    let header_len = 9 + usize::from(payload[8]);
    payload
        .get(header_len..)
        .ok_or(HlsError::Demux("PES header exceeds packet"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PMT_PID: u16 = 0x1000;
    const AUDIO_PID: u16 = 0x0100;

    /// One 188-byte TS packet, padded with adaptation-field stuffing.
    fn ts_packet(pid: u16, unit_start: bool, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() <= 184);
        let mut p = vec![
            TS_SYNC_BYTE,
            (if unit_start { 0x40 } else { 0 }) | (pid >> 8) as u8,
            pid as u8,
        ];
        if payload.len() == 184 {
            p.push(0x10);
        } else {
            let af_len = 183 - payload.len();
            p.push(0x30);
            p.push(af_len as u8);
            if af_len > 0 {
                p.push(0x00); // adaptation flags
                p.extend(std::iter::repeat_n(0xFF, af_len - 1));
            }
        }
        p.extend_from_slice(payload);
        assert_eq!(p.len(), TS_PACKET_LEN);
        p
    }

    fn pat() -> Vec<u8> {
        let mut section = vec![0x00, 0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00];
        section.extend([0x00, 0x01, 0xE0 | (PMT_PID >> 8) as u8, PMT_PID as u8]);
        section.extend([0; 4]); // CRC (unchecked)
        ts_packet(PAT_PID, true, &section)
    }

    fn pmt(stream_type: u8) -> Vec<u8> {
        let mut section = vec![0x00, PMT_TABLE_ID, 0xB0, 0x12, 0x00, 0x01, 0xC1, 0x00, 0x00];
        section.extend([0xE1, 0x00, 0xF0, 0x00]); // PCR PID, program_info_length 0
        section.extend([stream_type, 0xE1, AUDIO_PID as u8, 0xF0, 0x00]);
        section.extend([0; 4]);
        ts_packet(PMT_PID, true, &section)
    }

    fn pes_start(data: &[u8]) -> Vec<u8> {
        let mut payload = vec![0x00, 0x00, 0x01, 0xC0, 0x00, 0x00, 0x80, 0x80, 0x05];
        payload.extend([0x21, 0x00, 0x01, 0x00, 0x01]); // PTS
        payload.extend_from_slice(data);
        ts_packet(AUDIO_PID, true, &payload)
    }

    #[test]
    fn demuxer_concatenates_pes_payloads_across_packets_and_segments() {
        let a: Vec<u8> = (0..100).collect();
        let b: Vec<u8> = (100..=255).collect();
        let c = vec![0xAB; 184];

        let mut seg1 = [pat(), pmt(STREAM_TYPE_ADTS_AAC), pes_start(&a)].concat();
        seg1.extend(ts_packet(AUDIO_PID, false, &b));
        seg1.extend(ts_packet(0x1FFF, false, &[0xEE; 10])); // null packet, ignored
        let seg2 = [pat(), pmt(STREAM_TYPE_ADTS_AAC), pes_start(&[])].concat();
        let mut seg2 = seg2;
        seg2.extend(ts_packet(AUDIO_PID, false, &c));

        let mut demuxer = TsDemuxer::default();
        let mut out = Vec::new();
        demuxer.push(&seg1, &mut out).unwrap();
        demuxer.push(&seg2, &mut out).unwrap();

        assert_eq!(out, [a, b, c].concat());
    }

    #[test]
    fn demuxer_rejects_non_aac_streams() {
        let seg = [pat(), pmt(0x03)].concat(); // MPEG-1 audio
        let err = TsDemuxer::default()
            .push(&seg, &mut Vec::new())
            .unwrap_err();
        assert!(matches!(err, HlsError::Unsupported(_)), "{err}");
    }

    #[test]
    fn demuxer_rejects_garbage() {
        let err = TsDemuxer::default()
            .push(&[0u8; TS_PACKET_LEN], &mut Vec::new())
            .unwrap_err();
        assert!(matches!(err, HlsError::Demux(_)), "{err}");
    }

    #[test]
    fn select_variant_picks_highest_bandwidth_and_resolves_relative() {
        let base = Url::parse("https://cdn.podimo.com/ep/ep.m3u8?sig=a").unwrap();
        let master = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=192000\n\
            hls/ep_medium/ep.m3u8?sig=m\n\
            #EXT-X-STREAM-INF:BANDWIDTH=320000,CODECS=\"mp4a.40.2\"\n\
            hls/ep_high/ep.m3u8?sig=h\n";
        let variant = select_variant(master, &base).unwrap().unwrap();
        assert_eq!(
            variant.as_str(),
            "https://cdn.podimo.com/ep/hls/ep_high/ep.m3u8?sig=h"
        );
    }

    #[test]
    fn select_variant_returns_none_for_media_playlist() {
        let base = Url::parse("https://cdn.podimo.com/ep/ep.m3u8").unwrap();
        let media = "#EXTM3U\n#EXTINF:10.0,\nseg0.ts\n#EXT-X-ENDLIST\n";
        assert!(select_variant(media, &base).unwrap().is_none());
    }

    #[test]
    fn media_segments_resolves_in_order() {
        let base = Url::parse("https://cdn.podimo.com/ep/hls/high/ep.m3u8?x=1").unwrap();
        let media = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n\
            #EXTINF:10.0,\nep_0.ts?sig=1\n#EXTINF:9.9,\nep_1.ts?sig=2\n#EXT-X-ENDLIST\n";
        let segs = media_segments(media, &base).unwrap();
        let segs: Vec<&str> = segs.iter().map(Url::as_str).collect();
        assert_eq!(
            segs,
            [
                "https://cdn.podimo.com/ep/hls/high/ep_0.ts?sig=1",
                "https://cdn.podimo.com/ep/hls/high/ep_1.ts?sig=2",
            ]
        );
    }

    #[test]
    fn media_segments_rejects_encryption() {
        let base = Url::parse("https://cdn.podimo.com/ep.m3u8").unwrap();
        let media = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:10,\ns.ts\n";
        assert!(matches!(
            media_segments(media, &base),
            Err(HlsError::Unsupported(_))
        ));
    }

    #[test]
    fn validate_source_only_allows_podimo_https_playlists() {
        assert!(validate_source("https://media-cdn-episodes.podimo.com/a/a.m3u8?s=1").is_ok());
        assert!(validate_source("http://media-cdn-episodes.podimo.com/a/a.m3u8").is_err());
        assert!(validate_source("https://evil.example/a.m3u8").is_err());
        assert!(validate_source("https://podimo.com.evil.example/a.m3u8").is_err());
        assert!(validate_source("https://media-cdn-episodes.podimo.com/a.mp3").is_err());
        assert!(validate_source("not a url").is_err());
    }

    #[test]
    fn select_variant_rejects_a_variant_on_another_host() {
        let base = Url::parse("https://cdn.podimo.com/ep/ep.m3u8").unwrap();
        let master = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=320000\n\
            https://evil.example/x.m3u8\n";
        assert!(matches!(
            select_variant(master, &base),
            Err(HlsError::InvalidSource(_))
        ));
    }

    #[test]
    fn media_segments_rejects_entries_off_podimo_https() {
        let base = Url::parse("https://cdn.podimo.com/ep/hls/high/ep.m3u8").unwrap();
        for entry in [
            "https://evil.example/seg.ts",
            "http://cdn.podimo.com/seg.ts",
            "//evil.example/seg.ts",
        ] {
            let media = format!("#EXTM3U\n#EXTINF:10.0,\nep_0.ts\n#EXTINF:10.0,\n{entry}\n");
            assert!(
                matches!(
                    media_segments(&media, &base),
                    Err(HlsError::InvalidSource(_))
                ),
                "{entry}"
            );
        }
    }

    #[tokio::test]
    async fn fetches_refuse_a_redirect_off_podimo() {
        use wiremock::matchers::path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(path("/moved.ts"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/seg.ts", server.uri())),
            )
            .mount(&server)
            .await;
        Mock::given(path("/seg.ts"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"#EXTM3U\n".to_vec()))
            .mount(&server)
            .await;
        let client = Client::new();
        let url = |p: &str| Url::parse(&format!("{}{p}", server.uri())).unwrap();

        // Callers vet the URLs they pass in; only where a redirect led is
        // checked here. The mock server is off Podimo's hosts, so a redirect
        // on it counts as leaving them.
        let seg = fetch_segment(&client, &url("/seg.ts"), FETCH_TIMEOUT).await;
        assert!(seg.is_ok());
        assert!(fetch_text(&client, &url("/seg.ts")).await.is_ok());
        let err = fetch_segment(&client, &url("/moved.ts"), FETCH_TIMEOUT)
            .await
            .unwrap_err();
        assert!(matches!(err, HlsError::InvalidSource(_)), "{err}");
        let err = fetch_text(&client, &url("/moved.ts")).await.unwrap_err();
        assert!(matches!(err, HlsError::InvalidSource(_)), "{err}");
    }

    #[tokio::test]
    async fn acquire_slot_gives_up_when_every_slot_stays_busy() {
        let slots = Arc::new(Semaphore::new(1));
        let held = acquire_slot(&slots, Duration::from_millis(50))
            .await
            .unwrap();
        let err = acquire_slot(&slots, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(err, HlsError::Busy), "{err}");
        drop(held);
        assert!(acquire_slot(&slots, Duration::from_millis(50))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn a_slot_is_held_until_the_body_is_dropped() {
        let slots = Arc::new(Semaphore::new(1));
        let slot = acquire_slot(&slots, Duration::from_millis(50))
            .await
            .unwrap();
        let mut body = Box::pin(holding(slot, futures::stream::iter([1, 2])));
        assert_eq!(body.next().await, Some(1));
        assert_eq!(body.next().await, Some(2));
        assert_eq!(body.next().await, None);
        assert_eq!(slots.available_permits(), 0, "held while the body lives");
        drop(body);
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn prefetched_segments_survive_a_consumer_slower_than_the_timeout() {
        use wiremock::matchers::path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // A client reading slower than real time stalls the consumer between
        // items for longer than the fetch timeout. Segments fetched ahead
        // must not fail on their timeouts meanwhile.
        let fetch_timeout = Duration::from_secs(1);
        let consumer_pause = Duration::from_millis(1250);
        let server = MockServer::start().await;
        let segment = [pat(), pmt(STREAM_TYPE_ADTS_AAC), pes_start(&[1, 2, 3])].concat();
        let mut segments = Vec::new();
        for (i, delay_ms) in [0, 50, 250, 600].into_iter().enumerate() {
            let seg_path = format!("/seg{i}.ts");
            Mock::given(path(seg_path.as_str()))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_bytes(segment.clone())
                        .set_delay(Duration::from_millis(delay_ms)),
                )
                .mount(&server)
                .await;
            segments.push(Url::parse(&format!("{}{seg_path}", server.uri())).unwrap());
        }

        let stream =
            aac_stream_with_timeout(Client::new(), segments, SEGMENT_PREFETCH, fetch_timeout);
        let mut stream = std::pin::pin!(stream);
        let mut items = 0;
        while let Some(item) = stream.next().await {
            let item = item.unwrap_or_else(|err| panic!("segment {items}: {err}"));
            assert_eq!(item.as_ref(), [1, 2, 3]);
            items += 1;
            tokio::time::sleep(consumer_pause).await;
        }
        assert_eq!(items, 4);
    }

    #[test]
    fn is_hls_url_ignores_query_and_fragment() {
        assert!(is_hls_url("https://x/a.m3u8?b=c.mp3"));
        assert!(!is_hls_url("https://x/a.mp3?b=c.m3u8"));
    }

    #[test]
    fn stream_enclosure_url_is_a_plain_file_path_carrying_the_playlist() {
        let src = "https://media-cdn-episodes.podimo.com/a/a.m3u8?u=1&KeyName=k&Signature=s";
        for format in [StreamFormat::Mp3, StreamFormat::M4a] {
            let url = stream_enclosure_url("http://host:1", "ep-1", src, format);
            let file = format!("/ep-1.{}", format.extension());
            assert!(url.starts_with("http://host:1/stream/"), "{url}");
            assert!(url.ends_with(&file), "{url}");
            assert!(!url.contains('?') && !url.contains(".m3u8"), "{url}");

            let token = url
                .trim_start_matches("http://host:1/stream/")
                .trim_end_matches(&file);
            assert_eq!(decode_source_token(token).unwrap(), src);
            assert_eq!(
                StreamFormat::from_extension(format.extension()),
                Some(format)
            );
        }
        assert_eq!(StreamFormat::from_extension("ogg"), None);
    }

    #[test]
    fn decode_source_token_rejects_garbage() {
        assert!(decode_source_token("not base64!").is_err());
        assert!(decode_source_token(&URL_SAFE_NO_PAD.encode([0xFF, 0xFE])).is_err());
    }

    /// `seconds` of a 440 Hz tone as ADTS AAC, generated by ffmpeg itself.
    fn adts_tone(seconds: u32) -> Vec<u8> {
        let out = std::process::Command::new(FFMPEG)
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={seconds}"))
            .args(["-c:a", "aac", "-b:a", "128k", "-f", "adts", "pipe:1"])
            .output()
            .expect("the transcode tests need ffmpeg on PATH");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    /// The first `n` whole ADTS frames of `aac`.
    fn adts_prefix(aac: &[u8], n: usize) -> &[u8] {
        let mut end = 0;
        for _ in 0..n {
            let h = &aac[end..];
            end += (usize::from(h[3] & 0x03) << 11)
                | (usize::from(h[4]) << 3)
                | usize::from(h[5] >> 5);
        }
        &aac[..end]
    }

    async fn collect(
        stream: impl Stream<Item = Result<Bytes, HlsError>>,
    ) -> (Vec<u8>, Option<HlsError>) {
        let mut stream = std::pin::pin!(stream);
        let mut body = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => body.extend_from_slice(&chunk),
                Err(err) => return (body, Some(err)),
            }
        }
        (body, None)
    }

    /// `bytes` in uneven chunks, like demuxed segments arrive.
    fn chunked(bytes: &[u8]) -> impl Stream<Item = Result<Bytes, HlsError>> {
        let chunks: Vec<Result<Bytes, HlsError>> = bytes
            .chunks(7_000)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        futures::stream::iter(chunks)
    }

    /// The boxes in `data` as (type, body); enough for ffmpeg's small files.
    fn boxes(data: &[u8]) -> Vec<(&[u8], &[u8])> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos + 8 <= data.len() {
            let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            out.push((&data[pos + 4..pos + 8], &data[pos + 8..pos + size]));
            pos += size;
        }
        out
    }

    /// The descriptor that follows DecoderConfigDescriptor's fixed fields in
    /// an `esds` body, as (tag, payload). ExoPlayer takes it as the
    /// AudioSpecificConfig without checking the tag.
    fn esds_decoder_specific_info(esds: &[u8]) -> (u8, &[u8]) {
        fn header(d: &mut &[u8]) -> (u8, usize) {
            let tag = d[0];
            let mut size = 0;
            let mut i = 1;
            loop {
                size = (size << 7) | usize::from(d[i] & 0x7F);
                i += 1;
                if d[i - 1] & 0x80 == 0 {
                    break;
                }
            }
            *d = &d[i..];
            (tag, size)
        }
        let mut d = &esds[4..]; // version and flags
        assert_eq!(header(&mut d).0, 0x03, "ES_Descriptor");
        assert_eq!(d[2], 0, "no optional ES_Descriptor fields");
        d = &d[3..];
        assert_eq!(header(&mut d).0, 0x04, "DecoderConfigDescriptor");
        d = &d[13..];
        let (tag, size) = header(&mut d);
        (tag, &d[..size])
    }

    #[tokio::test]
    async fn transcode_mp3_is_128k_cbr() {
        let stream = transcode_mp3(chunked(&adts_tone(4))).await.unwrap();
        let (mp3, err) = collect(stream).await;
        assert!(err.is_none(), "{err:?}");

        // ID3v2 tag, then MPEG-1 Layer III frames at 128 kbps (index 0x9).
        assert_eq!(&mp3[..3], b"ID3");
        let tag_len = mp3[6..10]
            .iter()
            .fold(0usize, |acc, b| (acc << 7) | usize::from(*b));
        let frame = &mp3[10 + tag_len..];
        assert_eq!(frame[..2], [0xFF, 0xFB], "MPEG-1 Layer III frame header");
        assert_eq!(frame[2] >> 4, 0x9, "128 kbps");
        // ~4 s at 16 KB/s, plus encoder delay and padding.
        assert!((60_000..75_000).contains(&mp3.len()), "size {}", mp3.len());
    }

    #[tokio::test]
    async fn remux_m4a_writes_a_seekable_mp4_that_keeps_the_aac() {
        let aac = adts_tone(4);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("episode.m4a");
        remux_m4a(chunked(&aac), &path).await.unwrap();
        let m4a = std::fs::read(&path).unwrap();

        // One index up front, then the audio: no fragments, which ExoPlayer
        // can't seek in without a segment index.
        let types: Vec<&[u8]> = boxes(&m4a).iter().map(|(t, _)| *t).collect();
        assert_eq!(types.first(), Some(&&b"ftyp"[..]), "{types:?}");
        let moov_at = types.iter().position(|t| t == b"moov");
        let mdat_at = types.iter().position(|t| t == b"mdat");
        assert!(
            moov_at.is_some() && moov_at < mdat_at,
            "moov first: {types:?}"
        );
        assert!(!types.contains(&&b"moof"[..]), "not fragmented: {types:?}");

        // The AudioSpecificConfig must be there: without it ExoPlayer reads
        // the next descriptor as one and fails ("length=1; index=1").
        let (_, moov) = boxes(&m4a).into_iter().find(|(t, _)| t == b"moov").unwrap();
        let at = moov.windows(4).position(|w| w == b"esds").unwrap();
        let size = u32::from_be_bytes(moov[at - 4..at].try_into().unwrap()) as usize;
        let (tag, asc) = esds_decoder_specific_info(&moov[at + 4..at - 4 + size]);
        assert_eq!(tag, 0x05, "DecoderSpecificInfo");
        assert_eq!(asc[0] >> 3, 2, "AAC LC");
        assert_eq!(((asc[0] & 0x07) << 1) | (asc[1] >> 7), 4, "44.1 kHz");
        assert_eq!((asc[1] >> 3) & 0x0F, 1, "mono");

        // Repackaged, not re-encoded: the same AAC payload plus container.
        let ratio = m4a.len() as f64 / aac.len() as f64;
        assert!((0.9..1.2).contains(&ratio), "size ratio {ratio}");

        let out = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "format=duration"])
            .args(["-of", "csv=p=0"])
            .arg(&path)
            .output()
            .expect("the transcode tests need ffprobe on PATH");
        let duration: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        assert!((3.9..4.2).contains(&duration), "duration {duration}");
    }

    #[tokio::test]
    async fn remux_m4a_overwrites_what_a_failed_attempt_left() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("episode.m4a");
        std::fs::write(&path, b"leftover").unwrap();
        remux_m4a(chunked(&adts_tone(1)), &path).await.unwrap();
        assert_eq!(&std::fs::read(&path).unwrap()[4..8], b"ftyp");
    }

    fn cut_short(aac: &[u8]) -> impl Stream<Item = Result<Bytes, HlsError>> {
        futures::stream::iter(vec![
            Ok(Bytes::copy_from_slice(adts_prefix(aac, 40))),
            Err(HlsError::Upstream("segment gone".into())),
        ])
    }

    #[tokio::test]
    async fn transcode_mp3_ends_in_the_upstream_error() {
        let aac = adts_tone(2);
        let (out, err) = collect(transcode_mp3(cut_short(&aac)).await.unwrap()).await;
        assert!(matches!(err, Some(HlsError::Upstream(_))), "{err:?}");
        assert!(!out.is_empty(), "audio before the failure still streams");
    }

    #[tokio::test]
    async fn remux_m4a_fails_on_an_upstream_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("episode.m4a");
        let err = remux_m4a(cut_short(&adts_tone(2)), &path).await;
        assert!(matches!(err, Err(HlsError::Upstream(_))), "{err:?}");
    }

    fn garbage() -> impl Stream<Item = Result<Bytes, HlsError>> {
        futures::stream::iter(vec![Ok(Bytes::from_static(&[0x42; 4096]))])
    }

    #[tokio::test]
    async fn ffmpeg_failures_are_reported() {
        let (_, err) = collect(transcode_mp3(garbage()).await.unwrap()).await;
        assert!(matches!(err, Some(HlsError::Transcode(_))), "mp3: {err:?}");

        let dir = tempfile::tempdir().unwrap();
        let err = remux_m4a(garbage(), &dir.path().join("episode.m4a")).await;
        assert!(matches!(err, Err(HlsError::Transcode(_))), "m4a: {err:?}");
    }

    #[test]
    fn source_key_ignores_the_signature() {
        let signed = |sig: &str| {
            Url::parse(&format!(
                "https://media-cdn-episodes.podimo.com/a/a.m3u8?KeyName=k&Signature={sig}"
            ))
            .unwrap()
        };
        assert_eq!(source_key(&signed("one")), source_key(&signed("two")));
        let other = Url::parse("https://media-cdn-episodes.podimo.com/b/b.m3u8").unwrap();
        assert_ne!(source_key(&signed("one")), source_key(&other));
        assert!(source_key(&other).chars().all(|c| c.is_ascii_hexdigit()));
    }
}
