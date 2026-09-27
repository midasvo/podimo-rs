//! HLS → progressive audio for podcast episodes.
//!
//! Podimo serves episode audio as HLS: a master playlist with a few bitrate
//! variants, each a media playlist of ~10 s MPEG-TS segments carrying AAC.
//! Podcatchers want a single progressive file, so we walk the
//! highest-bandwidth variant, strip the TS/PES framing to get raw ADTS AAC,
//! and pipe that through `ffmpeg` into the feed's [`StreamFormat`]: MP3
//! (re-encoded) or M4A (the same AAC, repackaged). ffmpeg only ever sees
//! stdin/stdout; all network I/O stays in here.

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
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::Semaphore;
use tokio_util::io::ReaderStream;
use tokio_util::task::AbortOnDropHandle;

/// Content type of the legacy raw AAC stream (`/stream/<id>.aac`).
pub const AAC_CONTENT_TYPE: &str = "audio/aac";

/// Looked up on `PATH`; the Docker image installs Debian's ffmpeg.
const FFMPEG: &str = "ffmpeg";
/// Simultaneous MP3 encodes; further requests wait for a free slot. M4A only
/// repackages and doesn't take one.
const MAX_CONCURRENT_TRANSCODES: usize = 4;
/// How much of ffmpeg's stderr to keep for the error message.
const STDERR_TAIL_BYTES: usize = 2048;

static TRANSCODE_SLOTS: Lazy<Arc<Semaphore>> =
    Lazy::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_TRANSCODES)));

/// Only playlists on Podimo's own hosts are proxied, so `/stream` can't be
/// abused as an open relay.
const ALLOWED_HOST_SUFFIX: &str = ".podimo.com";

/// Segments fetched ahead of the one currently being written to the client.
const SEGMENT_PREFETCH: usize = 4;
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
}

/// What `/stream/<token>/<id>.<ext>` hands out. Both are always served; the
/// `STREAM_FORMAT` setting only picks which one the feed links to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamFormat {
    /// Re-encoded with LAME: plays everywhere, costs CPU.
    Mp3,
    /// Podimo's AAC untouched in a fragmented MP4: near-zero CPU, original
    /// quality, and unlike raw ADTS the container carries the duration.
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

    /// ffmpeg output options for ADTS AAC arriving on stdin.
    fn ffmpeg_output_args(self) -> &'static [&'static str] {
        match self {
            // LAME at -q 7 costs ~40% less CPU than its default for no audible
            // difference on speech. CBR keeps player duration estimates exact,
            // since there's no Xing header on a pipe.
            Self::Mp3 => &[
                "-c:a",
                "libmp3lame",
                "-b:a",
                "128k",
                "-compression_level",
                "7",
                "-f",
                "mp3",
            ],
            // Stream copy. Fragmented so it can be written to a pipe; ffprobe
            // still reads the exact duration from the fragments.
            Self::M4a => &[
                "-c:a",
                "copy",
                "-bsf:a",
                "aac_adtstoasc",
                "-movflags",
                "+empty_moov+default_base_moof",
                "-frag_duration",
                "10000000",
                "-f",
                "mp4",
            ],
        }
    }

    fn reencodes(self) -> bool {
        matches!(self, Self::Mp3)
    }
}

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
    if url.scheme() != "https" {
        return Err(HlsError::InvalidSource("must be https"));
    }
    let host = url.host_str().unwrap_or_default();
    if !host.ends_with(ALLOWED_HOST_SUFFIX) {
        return Err(HlsError::InvalidSource("host not allowed"));
    }
    if !is_hls_url(src) {
        return Err(HlsError::InvalidSource("not an .m3u8 playlist"));
    }
    Ok(url)
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
    best.map(|(_, uri)| join(base, uri)).transpose()
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
            segments.push(join(base, line)?);
        }
    }
    Ok(segments)
}

/// Stream the concatenated ADTS AAC of `segments`, fetching a few ahead.
/// An `Err` item aborts the response mid-body, so a client never mistakes a
/// truncated episode for a complete one.
pub fn aac_stream(
    client: Client,
    segments: Vec<Url>,
) -> impl Stream<Item = Result<Bytes, HlsError>> {
    let mut demuxer = TsDemuxer::default();
    futures::stream::iter(segments)
        .map(move |url| {
            let client = client.clone();
            async move { fetch_segment(&client, &url).await }
        })
        .buffered(SEGMENT_PREFETCH)
        .map(move |segment| {
            let segment = segment?;
            let mut out = Vec::with_capacity(segment.len());
            demuxer.push(&segment, &mut out)?;
            Ok(Bytes::from(out))
        })
}

/// Turn an ADTS AAC stream into `format` with ffmpeg, streaming the output
/// as it's produced. MP3 first waits for a free transcode slot.
///
/// Failures — upstream errors in `aac` as well as ffmpeg itself failing —
/// arrive as a trailing `Err` item, so the response is cut short instead of
/// ending in a truncated file that looks complete. Dropping the stream (the
/// client went away) kills ffmpeg and stops the upstream fetch.
pub async fn transcode<S>(
    aac: S,
    format: StreamFormat,
) -> Result<impl Stream<Item = Result<Bytes, HlsError>>, HlsError>
where
    S: Stream<Item = Result<Bytes, HlsError>> + Send + 'static,
{
    let permit = if format.reencodes() {
        let slots = TRANSCODE_SLOTS.clone();
        Some(
            slots
                .acquire_owned()
                .await
                .expect("transcode semaphore is never closed"),
        )
    } else {
        None
    };

    let mut child = Command::new(FFMPEG)
        .args([
            "-hide_banner",
            "-nostats",
            "-loglevel",
            "error",
            "-f",
            "aac",
            "-i",
            "pipe:0",
            "-map",
            "0:a:0",
        ])
        .args(format.ffmpeg_output_args())
        .arg("pipe:1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| HlsError::Transcode(format!("could not start {FFMPEG}: {err}")))?;
    let stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    let feeder = AbortOnDropHandle::new(tokio::spawn(feed_ffmpeg(aac, stdin)));
    // Drained concurrently: a chatty ffmpeg would otherwise block on a full
    // stderr pipe and stall the transcode.
    let stderr_tail = AbortOnDropHandle::new(tokio::spawn(read_tail(stderr)));

    let output = ReaderStream::with_capacity(stdout, 64 * 1024).map(|chunk| {
        chunk.map_err(|err| HlsError::Transcode(format!("reading ffmpeg output: {err}")))
    });

    // Polled once ffmpeg has closed stdout, i.e. when it's finished.
    let outcome = futures::stream::once(async move {
        let _permit = permit;
        let status = child
            .wait()
            .await
            .map_err(|err| HlsError::Transcode(format!("waiting for ffmpeg: {err}")))?;
        if !status.success() {
            let stderr = stderr_tail.await.unwrap_or_default();
            return Err(HlsError::Transcode(format!(
                "ffmpeg exited with {status}: {stderr}"
            )));
        }
        // A clean exit means ffmpeg saw EOF on stdin, so the feeder is done;
        // this surfaces an upstream error that cut the input short.
        feeder
            .await
            .map_err(|err| HlsError::Transcode(format!("feeding ffmpeg: {err}")))?
    })
    .filter_map(|outcome| async move { outcome.err().map(Err) });

    Ok(output.chain(outcome))
}

/// Log which ffmpeg will do the MP3 transcodes, or warn that there is none,
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
        .and_then(reqwest::Response::error_for_status)
        .map_err(|err| HlsError::Upstream(describe(err)))?;
    resp.text()
        .await
        .map_err(|err| HlsError::Upstream(describe(err)))
}

async fn fetch_segment(client: &Client, url: &Url) -> Result<Bytes, HlsError> {
    let mut last_err = None;
    for attempt in 0..SEGMENT_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1 << (attempt - 1))).await;
        }
        let result = client
            .get(url.clone())
            .timeout(FETCH_TIMEOUT)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status);
        let result = match result {
            Ok(resp) => resp.bytes().await,
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

fn join(base: &Url, uri: &str) -> Result<Url, HlsError> {
    base.join(uri)
        .map_err(|_| HlsError::Unsupported(format!("unresolvable playlist entry: {uri}")))
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

    #[tokio::test]
    async fn transcode_mp3_is_128k_cbr() {
        let stream = transcode(chunked(&adts_tone(4)), StreamFormat::Mp3)
            .await
            .unwrap();
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
    async fn transcode_m4a_keeps_the_aac_and_carries_the_duration() {
        let aac = adts_tone(4);
        let stream = transcode(chunked(&aac), StreamFormat::M4a).await.unwrap();
        let (m4a, err) = collect(stream).await;
        assert!(err.is_none(), "{err:?}");
        assert_eq!(&m4a[4..8], b"ftyp", "MP4 starts with its file type box");
        // Repackaged, not re-encoded: the same AAC payload plus container.
        let ratio = m4a.len() as f64 / aac.len() as f64;
        assert!((0.9..1.2).contains(&ratio), "size ratio {ratio}");

        // What raw ADTS couldn't give Audiobookshelf: a readable duration.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("episode.m4a");
        std::fs::write(&path, &m4a).unwrap();
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
    async fn transcode_ends_in_the_upstream_error() {
        let aac = adts_tone(2);
        for format in [StreamFormat::Mp3, StreamFormat::M4a] {
            let input = futures::stream::iter(vec![
                Ok(Bytes::copy_from_slice(adts_prefix(&aac, 40))),
                Err(HlsError::Upstream("segment gone".into())),
            ]);
            let (out, err) = collect(transcode(input, format).await.unwrap()).await;
            assert!(
                matches!(err, Some(HlsError::Upstream(_))),
                "{format:?}: {err:?}"
            );
            assert!(
                !out.is_empty(),
                "{format:?}: audio before the failure still streams"
            );
        }
    }

    #[tokio::test]
    async fn transcode_reports_ffmpeg_failure() {
        for format in [StreamFormat::Mp3, StreamFormat::M4a] {
            let input = futures::stream::iter(vec![Ok(Bytes::from_static(&[0x42; 4096]))]);
            let (_, err) = collect(transcode(input, format).await.unwrap()).await;
            assert!(
                matches!(err, Some(HlsError::Transcode(_))),
                "{format:?}: {err:?}"
            );
        }
    }
}
