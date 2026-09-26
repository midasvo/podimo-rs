//! HLS → ADTS AAC remux for podcast episodes.
//!
//! Podimo serves episode audio as HLS: a master playlist with a few bitrate
//! variants, each a media playlist of ~10 s MPEG-TS segments carrying AAC.
//! Podcatchers want a single progressive file, so `/stream/<id>.aac` walks the
//! highest-bandwidth variant, strips the TS/PES framing and streams the raw
//! ADTS frames back-to-back. No re-encoding, so no ffmpeg and no quality loss.

use std::time::Duration;

use axum::body::Bytes;
use futures::{Stream, StreamExt};
use reqwest::{Client, Url};
use thiserror::Error;

/// Content type of the remuxed stream.
pub const STREAM_CONTENT_TYPE: &str = "audio/aac";

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
}

/// True when `url` points at an HLS playlist rather than a progressive file.
pub fn is_hls_url(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.ends_with(".m3u8")
}

/// The proxy URL a feed enclosure should point at for an HLS episode.
pub fn stream_enclosure_url(base_url: &str, episode_id: &str, hls_url: &str) -> String {
    format!(
        "{base_url}/stream/{}.aac?src={}",
        urlencoding::encode(episode_id),
        urlencoding::encode(hls_url)
    )
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
                if best.map_or(true, |(b, _)| bandwidth > b) {
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
        for packet in data.chunks_exact(TS_PACKET_LEN) {
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
    section.get(8..)?.chunks_exact(4).find_map(|entry| {
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
                p.extend(std::iter::repeat(0xFF).take(af_len - 1));
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
    fn stream_enclosure_url_encodes_source() {
        let url = stream_enclosure_url("http://host:1", "ep-1", "https://x/a.m3u8?a=1&b=2");
        assert_eq!(
            url,
            "http://host:1/stream/ep-1.aac?src=https%3A%2F%2Fx%2Fa.m3u8%3Fa%3D1%26b%3D2"
        );
    }
}
