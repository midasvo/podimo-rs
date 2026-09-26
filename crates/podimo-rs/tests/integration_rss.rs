//! Structural snapshot tests for `podimo::rss::podcasts_to_rss`.
//!
//! Assertions are substring-based so they don't couple to attribute ordering,
//! whitespace, or generator metadata. The HEAD probe is pre-empted by
//! pre-populating the head cache, so the test stays offline.

use std::time::Duration;

use podimo_rs::cache::{HeadInfo, TtlCache};
use podimo_rs::podimo::rss::{audiobook_to_rss, podcasts_to_rss};
use reqwest::Client;
use serde_json::json;

fn fixed_payload() -> serde_json::Value {
    json!({
        "podcast": {
            "title": "Test Show",
            "description": "A deterministic test show",
            "webAddress": serde_json::Value::Null,
            "authorName": "Author",
            "language": "nl",
            "images": { "coverImageUrl": "https://example.com/cover.jpg" }
        },
        "episodes": [
            {
                "id": "ep1",
                "title": "Episode 1",
                "description": "First episode body",
                "publishDatetime": "2024-01-01T12:00:00Z",
                "datetime": "2024-01-01T12:00:00Z",
                "imageUrl": "https://example.com/ep1.jpg",
                "audio": { "url": "https://example.com/ep1.mp3", "duration": 1234 },
                "streamMedia": serde_json::Value::Null,
                "artist": "Author",
                "podcastName": "Test Show"
            },
            {
                "id": "ep2",
                "title": "Episode 2",
                "description": "Second episode body",
                "publishDatetime": "2024-01-02T12:00:00Z",
                "datetime": "2024-01-02T12:00:00Z",
                "imageUrl": "https://example.com/ep2.jpg",
                "audio": { "url": "https://example.com/ep2.mp3", "duration": 5678 },
                "streamMedia": serde_json::Value::Null,
                "artist": "Author",
                "podcastName": "Test Show"
            }
        ]
    })
}

async fn stub_head_cache_for(
    episode_ids: &[&str],
    length: &str,
    ctype: &str,
) -> TtlCache<HeadInfo> {
    let cache: TtlCache<HeadInfo> = TtlCache::new("head_test", None, Duration::from_secs(60)).await;
    for id in episode_ids {
        cache
            .insert(
                (*id).to_string(),
                HeadInfo {
                    content_length: length.into(),
                    content_type: ctype.into(),
                },
            )
            .await;
    }
    cache
}

#[tokio::test]
async fn podcasts_to_rss_renders_expected_structure() {
    let head_cache = stub_head_cache_for(&["ep1", "ep2"], "12345", "audio/mpeg").await;
    let scraper = Client::new();

    let rss = podcasts_to_rss(
        &fixed_payload(),
        "podcast-uuid",
        "nl-NL",
        false,
        None,
        "http://proxy.test",
        &scraper,
        &head_cache,
    )
    .await
    .expect("render");

    // Top-level RSS skeleton.
    assert!(rss.contains("<rss"));
    assert!(rss.contains("<channel>"));
    assert!(rss.contains("</channel>"));
    assert!(rss.contains("</rss>"));

    // Channel metadata.
    assert!(rss.contains("<title>Test Show</title>"));
    assert!(rss.contains("<description>A deterministic test show</description>"));
    assert!(rss.contains("<language>nl</language>"));
    assert!(rss.contains("<itunes:author>Author</itunes:author>"));
    assert!(rss.contains("https://podimo.com/shows/podcast-uuid"));

    // Two items.
    assert_eq!(rss.matches("<item>").count(), 2, "want 2 <item>: {rss}");
    assert_eq!(rss.matches("</item>").count(), 2);
    assert!(rss.contains("<title>Episode 1</title>"));
    assert!(rss.contains("<title>Episode 2</title>"));

    // GUIDs come through unchanged.
    assert!(rss.contains("<guid"));
    assert!(rss.contains("ep1"));
    assert!(rss.contains("ep2"));

    // itunes:duration carried through for each episode.
    assert!(rss.contains("<itunes:duration>1234</itunes:duration>"));
    assert!(rss.contains("<itunes:duration>5678</itunes:duration>"));

    // Enclosures rendered for both episodes with the head-info metadata.
    assert!(rss.contains("https://example.com/ep1.mp3"));
    assert!(rss.contains("https://example.com/ep2.mp3"));
    assert!(rss.contains("audio/mpeg"));
    assert!(rss.contains("12345"));
}

#[tokio::test]
async fn podcasts_to_rss_appends_jpg_fragment_to_extensionless_image_urls() {
    let head_cache = stub_head_cache_for(&["ep1", "ep2"], "0", "audio/mpeg").await;
    let scraper = Client::new();

    let mut payload = fixed_payload();
    payload["podcast"]["images"]["coverImageUrl"] =
        json!("https://images.podimo.com/cover?sig=abcdef");
    payload["episodes"][0]["imageUrl"] = json!("https://images.podimo.com/ep1?sig=xyz");
    payload["episodes"][1]["imageUrl"] = json!("https://images.podimo.com/ep2?sig=qrs");

    let rss = podcasts_to_rss(
        &payload,
        "podcast-uuid",
        "nl-NL",
        false,
        None,
        "http://proxy.test",
        &scraper,
        &head_cache,
    )
    .await
    .expect("render");

    assert!(
        rss.contains("https://images.podimo.com/cover?sig=abcdef#.jpg"),
        "channel cover should have #.jpg appended: {rss}"
    );
    assert!(
        rss.contains("https://images.podimo.com/ep1?sig=xyz#.jpg"),
        "ep1 image should have #.jpg appended: {rss}"
    );
    assert!(
        rss.contains("https://images.podimo.com/ep2?sig=qrs#.jpg"),
        "ep2 image should have #.jpg appended: {rss}"
    );
    // Sanity: never append a second fragment to an already-extensioned URL.
    assert!(!rss.contains("ep1.jpg#.jpg"));
}

#[tokio::test]
async fn podcasts_to_rss_preserves_existing_jpg_extension() {
    let head_cache = stub_head_cache_for(&["ep1", "ep2"], "0", "audio/mpeg").await;
    let scraper = Client::new();

    let rss = podcasts_to_rss(
        &fixed_payload(),
        "podcast-uuid",
        "nl-NL",
        false,
        None,
        "http://proxy.test",
        &scraper,
        &head_cache,
    )
    .await
    .expect("render");

    assert!(rss.contains("https://example.com/cover.jpg"));
    assert!(!rss.contains("https://example.com/cover.jpg#.jpg"));
    assert!(rss.contains("https://example.com/ep1.jpg"));
    assert!(!rss.contains("https://example.com/ep1.jpg#.jpg"));
}

#[tokio::test]
async fn podcasts_to_rss_limits_to_n_newest_episodes() {
    let head_cache = stub_head_cache_for(&["ep1", "ep2"], "0", "audio/mpeg").await;
    let scraper = Client::new();

    let rss = podcasts_to_rss(
        &fixed_payload(),
        "podcast-uuid",
        "nl-NL",
        false,
        Some(1),
        "http://proxy.test",
        &scraper,
        &head_cache,
    )
    .await
    .expect("render");

    // Episodes arrive PUBLISHED_DESCENDING; limit=1 keeps the head of the slice.
    assert_eq!(rss.matches("<item>").count(), 1, "want 1 <item>: {rss}");
    assert!(rss.contains("<title>Episode 1</title>"));
    assert!(!rss.contains("<title>Episode 2</title>"));
}

#[tokio::test]
async fn podcasts_to_rss_limit_larger_than_episode_count_is_noop() {
    let head_cache = stub_head_cache_for(&["ep1", "ep2"], "0", "audio/mpeg").await;
    let scraper = Client::new();

    let rss = podcasts_to_rss(
        &fixed_payload(),
        "podcast-uuid",
        "nl-NL",
        false,
        Some(999),
        "http://proxy.test",
        &scraper,
        &head_cache,
    )
    .await
    .expect("render");

    assert_eq!(rss.matches("<item>").count(), 2);
}

#[tokio::test]
async fn podcasts_to_rss_sets_itunes_block_when_public_feeds_disabled() {
    let head_cache = stub_head_cache_for(&["ep1", "ep2"], "0", "audio/mpeg").await;
    let scraper = Client::new();

    let rss_blocked = podcasts_to_rss(
        &fixed_payload(),
        "podcast-uuid",
        "nl-NL",
        /*public_feeds=*/ false,
        None,
        "http://proxy.test",
        &scraper,
        &head_cache,
    )
    .await
    .expect("render");
    assert!(
        rss_blocked.contains("itunes:block"),
        "PUBLIC_FEEDS=false must emit itunes:block: {rss_blocked}"
    );

    let rss_public = podcasts_to_rss(
        &fixed_payload(),
        "podcast-uuid",
        "nl-NL",
        /*public_feeds=*/ true,
        None,
        "http://proxy.test",
        &scraper,
        &head_cache,
    )
    .await
    .expect("render");
    assert!(
        !rss_public.contains("itunes:block"),
        "PUBLIC_FEEDS=true must NOT emit itunes:block: {rss_public}"
    );
}

fn fixed_audiobook_payload() -> serde_json::Value {
    json!({
        "audiobookById": {
            "id": "abuid",
            "title": "The Test Book",
            "authorNames": "Auteur A",
            "description": "Lorem ipsum.",
            "duration": 7200,
            "publisherName": "Testers Publishing",
            "yearOfBookPublication": 2024,
            "authors": [{"name": "Auteur A"}],
            "narrators": [{"name": "Verteller B"}, {"name": "Verteller C"}],
            "coverImage": {"url": "https://example.com/cover.jpg"},
            "language": {"isoLanguage": "nl"}
        }
    })
}

#[tokio::test]
async fn audiobook_to_rss_renders_single_item_with_metadata() {
    let head_cache = stub_head_cache_for(&["audiobook__abuid"], "99999", "audio/mpeg").await;
    let scraper = Client::new();

    let rss = audiobook_to_rss(
        &fixed_audiobook_payload(),
        "https://example.com/audiobook.mp3",
        "abuid",
        "nl-NL",
        false,
        &scraper,
        &head_cache,
    )
    .await
    .expect("render");

    assert!(rss.contains("<rss"));
    assert_eq!(rss.matches("<item>").count(), 1, "audiobook = single item");
    assert!(rss.contains("<title>The Test Book</title>"));
    assert!(rss.contains("<itunes:author>Auteur A</itunes:author>"));
    assert!(rss.contains("<itunes:duration>7200</itunes:duration>"));
    assert!(rss.contains("https://example.com/audiobook.mp3"));
    assert!(rss.contains("99999"));
    assert!(rss.contains("audio/mpeg"));
    // Narrators + publisher merged into the item description.
    assert!(rss.contains("Verteller B, Verteller C"));
    assert!(rss.contains("Testers Publishing"));
    // GUID is the audiobook id, not the (rotating) audio URL.
    assert!(rss.contains("abuid"));
    // pubDate derived from yearOfBookPublication.
    assert!(rss.contains("2024"));
    // Link points at the Podimo share page.
    assert!(rss.contains("https://open.podimo.com/audiobook/abuid"));
}

#[tokio::test]
async fn audiobook_to_rss_uses_authors_array_when_author_names_absent() {
    let head_cache = stub_head_cache_for(&["audiobook__abuid"], "0", "audio/mpeg").await;
    let scraper = Client::new();

    let mut payload = fixed_audiobook_payload();
    payload["audiobookById"]["authorNames"] = serde_json::Value::Null;

    let rss = audiobook_to_rss(
        &payload,
        "https://example.com/audiobook.mp3",
        "abuid",
        "nl-NL",
        false,
        &scraper,
        &head_cache,
    )
    .await
    .expect("render");

    assert!(rss.contains("<itunes:author>Auteur A</itunes:author>"));
}

#[tokio::test]
async fn audiobook_to_rss_sets_itunes_block_when_public_feeds_disabled() {
    let head_cache = stub_head_cache_for(&["audiobook__abuid"], "0", "audio/mpeg").await;
    let scraper = Client::new();

    let rss_blocked = audiobook_to_rss(
        &fixed_audiobook_payload(),
        "https://example.com/audiobook.mp3",
        "abuid",
        "nl-NL",
        /*public_feeds=*/ false,
        &scraper,
        &head_cache,
    )
    .await
    .expect("render");
    assert!(rss_blocked.contains("itunes:block"));

    let rss_public = audiobook_to_rss(
        &fixed_audiobook_payload(),
        "https://example.com/audiobook.mp3",
        "abuid",
        "nl-NL",
        /*public_feeds=*/ true,
        &scraper,
        &head_cache,
    )
    .await
    .expect("render");
    assert!(!rss_public.contains("itunes:block"));
}

#[tokio::test]
async fn podcasts_to_rss_routes_hls_episodes_through_stream_proxy() {
    // Empty head cache: an HLS episode must not trigger a HEAD probe, so this
    // stays offline even though nothing is stubbed.
    let head_cache: TtlCache<HeadInfo> =
        TtlCache::new("head_hls_test", None, Duration::from_secs(60)).await;
    let scraper = Client::new();

    let mut payload = fixed_payload();
    let episodes = payload["episodes"].as_array_mut().unwrap();
    episodes.truncate(1);
    episodes[0]["audio"] = serde_json::Value::Null;
    episodes[0]["streamMedia"] = json!({
        "url": "https://media-cdn-episodes.podimo.com/ep1/ep1.m3u8?u=x&KeyName=k&Signature=s",
        "duration": 1234
    });

    let rss = podcasts_to_rss(
        &payload,
        "podcast-uuid",
        "nl-NL",
        false,
        None,
        "http://proxy.test",
        &scraper,
        &head_cache,
    )
    .await
    .expect("render");

    assert!(
        rss.contains(
            "url=\"http://proxy.test/stream/ep1.aac?src=https%3A%2F%2Fmedia-cdn-episodes.podimo.com%2Fep1%2Fep1.m3u8%3Fu%3Dx%26KeyName%3Dk%26Signature%3Ds\""
        ),
        "enclosure should point at the stream proxy: {rss}"
    );
    assert!(rss.contains("type=\"audio/aac\""), "{rss}");
    assert!(!rss.contains("audio/x-mpegurl"), "{rss}");
    assert!(rss.contains("<itunes:duration>1234</itunes:duration>"));
}
