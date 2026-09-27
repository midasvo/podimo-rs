//! RSS 2.0 rendering with iTunes extensions.

use std::collections::BTreeMap;

use futures::future::join_all;
use reqwest::Client;
use rss::extension::itunes::{
    ITunesChannelExtensionBuilder, ITunesItemExtensionBuilder, ITunesOwnerBuilder,
};
use rss::{ChannelBuilder, EnclosureBuilder, ItemBuilder};
use serde_json::Value;

use crate::cache::{HeadInfo, TtlCache};
use crate::podimo::head::url_head_info;
use crate::podimo::hls::{is_hls_url, stream_enclosure_url, StreamFormat};
use crate::util::jpg_fragment;

const ITUNES_NS: &str = "http://www.itunes.com/dtds/podcast-1.0.dtd";
const CONCURRENT_HEAD_PROBES: usize = 10;

/// Where enclosures of HLS episodes point: our `/stream` route on
/// `base_url` (normally the address the feed was requested on), in `format`.
#[derive(Debug, Clone, Copy)]
pub struct StreamLinks<'a> {
    pub base_url: &'a str,
    pub format: StreamFormat,
}

#[allow(clippy::too_many_arguments)]
pub async fn podcasts_to_rss(
    payload: &Value,
    podcast_id: &str,
    locale: &str,
    public_feeds: bool,
    limit: Option<usize>,
    stream_links: StreamLinks<'_>,
    scraper: &Client,
    head_cache: &TtlCache<HeadInfo>,
) -> anyhow::Result<String> {
    let podcast = payload.get("podcast");
    let all_episodes: &[Value] = payload
        .get("episodes")
        .and_then(|e| e.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    // Episodes arrive PUBLISHED_DESCENDING; trimming the head keeps the N newest.
    let episodes: &[Value] = match limit {
        Some(n) => &all_episodes[..n.min(all_episodes.len())],
        None => all_episodes,
    };
    let last_episode = episodes.first();

    let title = first_non_null_string(&[
        podcast.and_then(|p| p.get("title")),
        last_episode.and_then(|e| e.get("podcastName")),
    ])
    .unwrap_or_else(|| "Podimo".to_string());

    let description = first_non_null_string(&[podcast.and_then(|p| p.get("description"))])
        .unwrap_or_else(|| title.clone());

    let image = first_non_null_string(&[
        podcast
            .and_then(|p| p.get("images"))
            .and_then(|i| i.get("coverImageUrl")),
        last_episode.and_then(|e| e.get("imageUrl")),
    ])
    .map(|s| jpg_fragment(&s));

    let language = first_non_null_string(&[podcast.and_then(|p| p.get("language"))])
        .unwrap_or_else(|| locale.to_string());

    let author = first_non_null_string(&[
        podcast.and_then(|p| p.get("authorName")),
        last_episode.and_then(|e| e.get("artist")),
    ])
    .unwrap_or_default();

    let link = format!("https://podimo.com/shows/{podcast_id}");

    // Up to CONCURRENT_HEAD_PROBES probes in flight per batch; ordering preserved.
    let mut items: Vec<rss::Item> = Vec::with_capacity(episodes.len());
    for chunk in episodes.chunks(CONCURRENT_HEAD_PROBES) {
        let batch = join_all(
            chunk
                .iter()
                .map(|ep| build_item(scraper, head_cache, ep, locale, stream_links)),
        )
        .await;
        for res in batch {
            match res {
                Ok(Some(it)) => items.push(it),
                Ok(None) => {}
                Err(err) => tracing::warn!(target: "podimo", "feed entry skipped: {err}"),
            }
        }
    }

    let itunes_owner = ITunesOwnerBuilder::default()
        .name(Some(author.clone()))
        .build();
    let mut itunes = ITunesChannelExtensionBuilder::default();
    itunes
        .author(Some(author))
        .image(image.clone())
        .owner(Some(itunes_owner));
    if !public_feeds {
        itunes.block(Some("Yes".to_string()));
    }
    let itunes = itunes.build();

    let mut channel = ChannelBuilder::default();
    channel
        .title(title.clone())
        .description(description)
        .link(link)
        .language(Some(language))
        .itunes_ext(Some(itunes))
        .items(items);
    let namespaces: BTreeMap<String, String> = [("itunes".to_string(), ITUNES_NS.to_string())]
        .into_iter()
        .collect();
    channel.namespaces(namespaces);

    if let Some(image_url) = image {
        let image_obj = rss::ImageBuilder::default()
            .url(image_url)
            .title(title.clone())
            .link(format!("https://podimo.com/shows/{podcast_id}"))
            .build();
        channel.image(Some(image_obj));
    }

    let channel = channel.build();
    Ok(channel.to_string())
}

/// Render a single-item RSS feed for one audiobook. Layout mirrors the podcast
/// renderer (iTunes extensions, namespaces, channel image) so existing
/// podcatchers handle it identically — the book itself is the lone episode.
///
/// `payload` is the GraphQL `data` block containing `audiobookById` (as returned
/// by `PodimoClient::get_audiobook`). `audio_url` is the freshly-minted signed
/// URL from `get_audiobook_audio_url`; we trust the caller to pass a fresh one.
pub async fn audiobook_to_rss(
    payload: &Value,
    audio_url: &str,
    audiobook_id: &str,
    locale: &str,
    public_feeds: bool,
    scraper: &Client,
    head_cache: &TtlCache<HeadInfo>,
) -> anyhow::Result<String> {
    let book = payload.get("audiobookById");

    let title = first_non_null_string(&[book.and_then(|b| b.get("title"))])
        .unwrap_or_else(|| "Podimo Audiobook".to_string());

    let description =
        first_non_null_string(&[book.and_then(|b| b.get("description"))]).unwrap_or_default();

    let image = first_non_null_string(&[book
        .and_then(|b| b.get("coverImage"))
        .and_then(|c| c.get("url"))])
    .map(|s| jpg_fragment(&s));

    let language = first_non_null_string(&[book
        .and_then(|b| b.get("language"))
        .and_then(|l| l.get("isoLanguage"))])
    .unwrap_or_else(|| locale.to_string());

    // Prefer the joined `authorNames` string; fall back to authors[].name.
    let author = first_non_null_string(&[book.and_then(|b| b.get("authorNames"))])
        .or_else(|| collect_name_array(book.and_then(|b| b.get("authors"))))
        .unwrap_or_default();

    let narrators_str =
        collect_name_array(book.and_then(|b| b.get("narrators"))).unwrap_or_default();

    let publisher = first_non_null_string(&[book.and_then(|b| b.get("publisherName"))]);
    let year = book
        .and_then(|b| b.get("yearOfBookPublication"))
        .and_then(|v| v.as_i64());
    let duration = book
        .and_then(|b| b.get("duration"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let link = format!("https://open.podimo.com/audiobook/{audiobook_id}");

    // Enclosure: HEAD-probe the (presumably freshly-minted) audio URL for size
    // and mime. Same `url_head_info` path as podcasts — the head cache key
    // namespace uses the audiobook id so we don't collide with episode HEADs.
    let head_key = format!("audiobook__{audiobook_id}");
    let head =
        crate::podimo::head::url_head_info(scraper, head_cache, &head_key, audio_url, locale)
            .await
            .map_err(|err| {
                anyhow::anyhow!("HEAD probe failed for audiobook {audiobook_id}: {err}")
            })?;

    let enclosure = EnclosureBuilder::default()
        .url(audio_url.to_string())
        .length(head.content_length)
        .mime_type(head.content_type)
        .build();

    let mut item_description = description.clone();
    append_section(&mut item_description, "Verteld door: ", &narrators_str);
    append_section(
        &mut item_description,
        "Uitgever: ",
        publisher.as_deref().unwrap_or(""),
    );

    let pub_date = year.map(|y| format!("Thu, 01 Jan {y} 00:00:00 +0000"));

    let mut itunes_item = ITunesItemExtensionBuilder::default();
    if duration > 0 {
        itunes_item.duration(Some(duration.to_string()));
    }
    if let Some(img) = image.clone() {
        itunes_item.image(Some(img));
    }

    let item = ItemBuilder::default()
        .guid(Some(
            rss::GuidBuilder::default()
                .value(audiobook_id.to_string())
                .permalink(false)
                .build(),
        ))
        .title(Some(title.clone()))
        .description(Some(item_description))
        .pub_date(pub_date)
        .enclosure(Some(enclosure))
        .itunes_ext(Some(itunes_item.build()))
        .build();

    let itunes_owner = ITunesOwnerBuilder::default()
        .name(Some(author.clone()))
        .build();
    let mut itunes = ITunesChannelExtensionBuilder::default();
    itunes
        .author(Some(author.clone()))
        .image(image.clone())
        .owner(Some(itunes_owner));
    if !public_feeds {
        itunes.block(Some("Yes".to_string()));
    }
    let itunes = itunes.build();

    let mut channel = ChannelBuilder::default();
    let channel_description = if description.is_empty() {
        title.clone()
    } else {
        description.clone()
    };
    channel
        .title(title.clone())
        .description(channel_description)
        .link(link.clone())
        .language(Some(language))
        .itunes_ext(Some(itunes))
        .items(vec![item]);
    let namespaces: BTreeMap<String, String> = [("itunes".to_string(), ITUNES_NS.to_string())]
        .into_iter()
        .collect();
    channel.namespaces(namespaces);

    if let Some(image_url) = image {
        let image_obj = rss::ImageBuilder::default()
            .url(image_url)
            .title(title.clone())
            .link(link)
            .build();
        channel.image(Some(image_obj));
    }

    let channel = channel.build();
    Ok(channel.to_string())
}

/// Appends `"{label}{value}"` to `buf`, separated from any existing content by
/// a blank line. No-op when `value` is empty so callers can pass optional
/// metadata without conditionals at the call site.
fn append_section(buf: &mut String, label: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    if !buf.is_empty() {
        buf.push_str("\n\n");
    }
    buf.push_str(label);
    buf.push_str(value);
}

fn collect_name_array(v: Option<&Value>) -> Option<String> {
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

async fn build_item(
    scraper: &Client,
    head_cache: &TtlCache<HeadInfo>,
    episode: &Value,
    locale: &str,
    stream_links: StreamLinks<'_>,
) -> anyhow::Result<Option<rss::Item>> {
    let id = episode.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    let title = episode
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = episode
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let pub_date = episode
        .get("publishDatetime")
        .and_then(|v| v.as_str())
        .or_else(|| episode.get("datetime").and_then(|v| v.as_str()))
        .map(|s| s.to_string());

    let (audio_url, duration) = extract_audio_url(episode);
    let Some(audio_url) = audio_url else {
        return Ok(None);
    };

    let enclosure = if is_hls_url(&audio_url) {
        // HLS playlists are useless to podcatchers; point at our own
        // MP3/M4A stream instead. Its size isn't known up front, and 0 is the
        // conventional "unknown" enclosure length.
        EnclosureBuilder::default()
            .url(stream_enclosure_url(
                stream_links.base_url,
                id,
                &audio_url,
                stream_links.format,
            ))
            .length("0".to_string())
            .mime_type(stream_links.format.content_type().to_string())
            .build()
    } else {
        // url_head_info already bounds total time via RETRIES * TIMEOUT_PER_TRY +
        // backoff; no outer timeout needed.
        let head = url_head_info(scraper, head_cache, id, &audio_url, locale)
            .await
            .map_err(|err| anyhow::anyhow!("HEAD probe failed for episode {id}: {err}"))?;
        EnclosureBuilder::default()
            .url(audio_url.clone())
            .length(head.content_length)
            .mime_type(head.content_type)
            .build()
    };

    let image_url = episode
        .get("imageUrl")
        .and_then(|v| v.as_str())
        .map(jpg_fragment);

    let mut itunes_item = ITunesItemExtensionBuilder::default();
    if duration > 0 {
        itunes_item.duration(Some(duration.to_string()));
    }
    if let Some(img) = image_url {
        itunes_item.image(Some(img));
    }

    let item = ItemBuilder::default()
        .guid(Some(
            rss::GuidBuilder::default()
                .value(id.to_string())
                .permalink(false)
                .build(),
        ))
        .title(Some(title))
        .description(Some(description))
        .pub_date(pub_date)
        .enclosure(Some(enclosure))
        .itunes_ext(Some(itunes_item.build()))
        .build();

    Ok(Some(item))
}

/// Returns `(Option<url>, duration_seconds)` from an episode object. The URL
/// may be an HLS playlist; `build_item` routes those through `/stream`.
fn extract_audio_url(episode: &Value) -> (Option<String>, i64) {
    let audio = episode.get("audio");
    let url = audio
        .and_then(|a| a.get("url"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let duration = audio
        .and_then(|a| a.get("duration"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    if let Some(url) = url {
        return (Some(url), duration);
    }

    let stream = episode.get("streamMedia");
    if let Some(stream) = stream {
        let url = stream
            .get("url")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let duration = stream.get("duration").and_then(|v| v.as_i64()).unwrap_or(0);
        return (url, duration);
    }
    (None, 0)
}

fn first_non_null_string(candidates: &[Option<&Value>]) -> Option<String> {
    for v in candidates.iter().flatten() {
        if let Some(s) = v.as_str() {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_audio_url_prefers_audio_block() {
        let ep = json!({
            "audio": {"url": "https://a/file.mp3", "duration": 42},
            "streamMedia": {"url": "https://s/main.m3u8", "duration": 1},
        });
        assert_eq!(
            extract_audio_url(&ep),
            (Some("https://a/file.mp3".into()), 42)
        );
    }

    #[test]
    fn extract_audio_url_falls_back_to_stream_media_when_audio_empty() {
        let ep = json!({
            "audio": {"url": "", "duration": 0},
            "streamMedia": {"url": "https://s/file.mp3", "duration": 99},
        });
        assert_eq!(
            extract_audio_url(&ep),
            (Some("https://s/file.mp3".into()), 99)
        );
    }

    #[test]
    fn extract_audio_url_passes_hls_through() {
        let hls = "https://media-cdn-episodes.podimo.com/ep/ep.m3u8?sig=x";
        let ep = json!({
            "audio": null,
            "streamMedia": {"url": hls, "duration": 60},
        });
        assert_eq!(extract_audio_url(&ep), (Some(hls.into()), 60));
    }

    #[test]
    fn extract_audio_url_returns_none_when_no_audio() {
        let ep = json!({"audio": null, "streamMedia": null});
        assert_eq!(extract_audio_url(&ep), (None, 0));
    }

    #[test]
    fn first_non_null_string_skips_empty_and_null() {
        let a = Value::Null;
        let b = json!("");
        let c = json!("found");
        assert_eq!(
            first_non_null_string(&[Some(&a), Some(&b), Some(&c)]),
            Some("found".into())
        );
        assert_eq!(first_non_null_string(&[Some(&a), Some(&b)]), None);
        assert_eq!(first_non_null_string(&[None, None]), None);
    }
}
