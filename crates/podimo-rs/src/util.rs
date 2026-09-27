//! Pure helpers: random ids, header builder, query-arg + image-URL fixups,
//! and loose boolean parsing.

use std::collections::HashMap;

use once_cell::sync::Lazy;
use rand::distr::{Distribution, Uniform};
use rand::seq::IteratorRandom;
use regex::Regex;
use sha2::{Digest, Sha256};

static EMAIL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[^@\s]+@[^@\s]+\.[^@\s]+$").expect("static regex compiles"));

pub(crate) static PODCAST_ID_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[0-9a-fA-F\-]+$").expect("static regex compiles"));

/// Matches a canonical RFC-4122 UUID (8-4-4-4-12 hex) anywhere in a string.
/// Used to extract the podcast id from a pasted Podimo URL like
/// `https://open.podimo.com/podcast/de9b2081-9fc5-489f-b9d3-d744ed9cab20`.
static UUID_IN_URL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
        .expect("static regex compiles")
});

/// Returns the podcast id from raw user input. Accepts either a bare id
/// (anything `PODCAST_ID_RE` already matches) or a Podimo URL with a UUID
/// somewhere in its path. Returns `None` if neither applies.
pub(crate) fn extract_podcast_id(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if PODCAST_ID_RE.is_match(trimmed) {
        return Some(trimmed);
    }
    UUID_IN_URL_RE.find(trimmed).map(|m| m.as_str())
}

/// Distinguishes the two content kinds the proxy supports.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum PodimoKind {
    Podcast,
    Audiobook,
}

impl PodimoKind {
    /// URL path segment used when building the proxy-side feed link.
    pub(crate) fn route_segment(self) -> &'static str {
        match self {
            PodimoKind::Podcast => "feed",
            PodimoKind::Audiobook => "audiobook",
        }
    }
}

/// Parses an HTTP Basic header. Returns `(username_field, password)`. Shared
/// between `/feed/*` and `/audiobook/*` handlers — both accept the same
/// `email,region,locale:password` Basic-auth form.
pub(crate) fn parse_basic_auth(headers: &axum::http::HeaderMap) -> Option<(String, String)> {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine;
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = raw
        .strip_prefix("Basic ")
        .or_else(|| raw.strip_prefix("basic "))?;
    let decoded = BASE64.decode(token.trim()).ok()?;
    let s = std::str::from_utf8(&decoded).ok()?;
    let (user, pass) = s.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

/// `scheme://host[:port]` this request was addressed to, for absolute links
/// the same client can follow back. Behind a reverse proxy that's
/// `X-Forwarded-Proto` + `X-Forwarded-Host` (or the passed-through `Host`);
/// a direct request is plain HTTP, since we don't terminate TLS ourselves.
/// Falls back to `fallback` when there's no usable host header. Both headers
/// are client-controlled, but only shape the links in this client's own
/// response.
pub(crate) fn request_base_url(headers: &axum::http::HeaderMap, fallback: &str) -> String {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            // Chained proxies append ("a, b"); the first entry faces the client.
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let host = header("x-forwarded-host")
        .or_else(|| header("host"))
        .filter(|h| is_plausible_authority(h));
    let Some(host) = host else {
        return fallback.to_string();
    };
    let scheme = match header("x-forwarded-proto") {
        Some(proto) if proto.eq_ignore_ascii_case("https") => "https",
        _ => "http",
    };
    format!("{scheme}://{host}")
}

/// Host names, IPv4/IPv6 literals and ports only; anything else (quotes,
/// slashes, whitespace) is ignored so it can't reshape the links.
fn is_plausible_authority(host: &str) -> bool {
    host.len() <= 255
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._:[]".contains(&b))
}

/// Detects whether a pasted Podimo URL is an audiobook or a podcast, and
/// extracts the UUID. A bare UUID (no surrounding URL) defaults to podcast for
/// backwards compatibility with the old single-content-type form. Returns
/// `None` if no UUID can be found.
pub(crate) fn parse_podimo_input(input: &str) -> Option<(PodimoKind, &str)> {
    let id = extract_podcast_id(input)?;
    let lower = input.to_ascii_lowercase();
    let kind = if lower.contains("/audiobook/") || lower.contains("/audioboek/") {
        PodimoKind::Audiobook
    } else {
        PodimoKind::Podcast
    };
    Some((kind, id))
}

const HEX_CHARS: &[u8] = b"1234567890abcdef";

pub(crate) fn random_hex_id(length: usize) -> String {
    let mut rng = rand::rng();
    (0..length)
        .map(|_| {
            char::from(
                *HEX_CHARS
                    .iter()
                    .choose(&mut rng)
                    .expect("hex chars not empty"),
            )
        })
        .collect()
}

pub(crate) fn random_flyer_id() -> String {
    let mut rng = rand::rng();
    let dist =
        Uniform::try_from(1_000_000_000_000_u64..=9_999_999_999_999_u64).expect("range not empty");
    let a = dist.sample(&mut rng);
    let b = dist.sample(&mut rng);
    format!("{a}-{b}")
}

pub(crate) fn token_key(username: &str, password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(username.as_bytes());
    hasher.update(b"~");
    hasher.update(password.as_bytes());
    hex::encode(hasher.finalize())
}

pub(crate) fn is_correct_email(username: &str) -> bool {
    EMAIL_RE.is_match(username)
}

pub(crate) fn generate_headers(
    authorization: Option<&str>,
    locale: &str,
) -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert("user-os".into(), "android".into());
    h.insert(
        "user-agent".into(),
        "Podimo/2.45.1 build 566/Android 33".into(),
    );
    h.insert("user-version".into(), "2.45.1".into());
    h.insert("user-locale".into(), locale.into());
    h.insert("user-unique-id".into(), random_hex_id(16));
    if let Some(auth) = authorization {
        h.insert("authorization".into(), auth.into());
    }
    h
}

/// Splits an HTTP-Basic username overloaded with `,region,locale`.
/// When fewer or more than three parts are present, defaults to `("nl", "nl-NL")`.
pub(crate) fn split_username_region_locale(s: &str) -> (String, String, String) {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() == 3 {
        (parts[0].into(), parts[1].into(), parts[2].into())
    } else {
        (parts[0].into(), "nl".into(), "nl-NL".into())
    }
}

/// Returns the query-arg value for `name`, falling back to `amp;<name>` for
/// consumers that don't decode `&amp;` (e.g. Audiobookshelf scraping the HTML).
pub(crate) fn amp_arg<'a, F>(get: F, name: &str) -> Option<String>
where
    F: Fn(&str) -> Option<&'a str>,
{
    get(name)
        .or_else(|| get(&format!("amp;{name}")))
        .map(String::from)
}

/// Appends `#.jpg` to URLs that don't already end in `.jpg`/`.png`. Podimo image URLs
/// end in signed query strings; feedgen / Apple require a recognized extension.
/// Clients strip the fragment before issuing the GET, so the actual fetch is unaffected.
pub(crate) fn jpg_fragment(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".png") {
        url.to_string()
    } else {
        format!("{url}#.jpg")
    }
}

/// Case-insensitive positive coercion: `"true"`, `"1"`, `"t"`, `"y"`, `"yes"`.
pub(crate) fn parse_bool_loose(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "true" | "1" | "t" | "y" | "yes"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_validation_matches_python() {
        assert!(is_correct_email("a@b.com"));
        assert!(is_correct_email("user+tag@example.co.uk"));
        assert!(!is_correct_email("not-an-email"));
        assert!(!is_correct_email("user @example.com"));
        assert!(!is_correct_email("user@@example.com"));
        assert!(!is_correct_email("user@example"));
    }

    #[test]
    fn token_key_is_stable() {
        let k = token_key("a@b.com", "secret");
        assert_eq!(k.len(), 64);
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(token_key("a@b.com", "secret"), k);
    }

    #[test]
    fn split_three_parts() {
        assert_eq!(
            split_username_region_locale("a@b.com,nl,nl-NL"),
            ("a@b.com".into(), "nl".into(), "nl-NL".into())
        );
    }

    #[test]
    fn split_one_part_defaults() {
        assert_eq!(
            split_username_region_locale("a@b.com"),
            ("a@b.com".into(), "nl".into(), "nl-NL".into())
        );
    }

    #[test]
    fn split_four_parts_falls_back_to_defaults() {
        assert_eq!(
            split_username_region_locale("a@b.com,nl,nl-NL,extra"),
            ("a@b.com".into(), "nl".into(), "nl-NL".into())
        );
    }

    #[test]
    fn amp_arg_prefers_plain_then_amp_prefix() {
        let m: HashMap<&str, &str> = [("region", "nl"), ("amp;locale", "nl-NL")]
            .into_iter()
            .collect();
        assert_eq!(amp_arg(|k| m.get(k).copied(), "region"), Some("nl".into()));
        assert_eq!(
            amp_arg(|k| m.get(k).copied(), "locale"),
            Some("nl-NL".into())
        );
        assert_eq!(amp_arg(|k| m.get(k).copied(), "absent"), None);
    }

    #[test]
    fn amp_arg_plain_wins_over_amp_prefixed() {
        let m: HashMap<&str, &str> = [("region", "nl"), ("amp;region", "de")]
            .into_iter()
            .collect();
        assert_eq!(amp_arg(|k| m.get(k).copied(), "region"), Some("nl".into()));
    }

    fn headers(pairs: &[(&'static str, &str)]) -> axum::http::HeaderMap {
        pairs
            .iter()
            .map(|(k, v)| (axum::http::HeaderName::from_static(k), v.parse().unwrap()))
            .collect()
    }

    const FALLBACK: &str = "https://fallback.example";

    #[test]
    fn request_base_url_direct_request_is_plain_http_on_host() {
        let h = headers(&[("host", "podimo")]);
        assert_eq!(request_base_url(&h, FALLBACK), "http://podimo");
        let h = headers(&[("host", "192.168.1.5:12104")]);
        assert_eq!(request_base_url(&h, FALLBACK), "http://192.168.1.5:12104");
    }

    #[test]
    fn request_base_url_follows_reverse_proxy_headers() {
        let h = headers(&[
            ("host", "podimo.example.com"),
            ("x-forwarded-proto", "https"),
        ]);
        assert_eq!(request_base_url(&h, FALLBACK), "https://podimo.example.com");

        let h = headers(&[
            ("host", "podimo:12104"),
            ("x-forwarded-host", "podimo.example.com, internal"),
            ("x-forwarded-proto", "HTTPS, http"),
        ]);
        assert_eq!(request_base_url(&h, FALLBACK), "https://podimo.example.com");
    }

    #[test]
    fn request_base_url_falls_back_without_a_usable_host() {
        assert_eq!(request_base_url(&headers(&[]), FALLBACK), FALLBACK);
        let h = headers(&[("host", "evil.example/\"><x")]);
        assert_eq!(request_base_url(&h, FALLBACK), FALLBACK);
    }

    #[test]
    fn jpg_fragment_appends_when_missing() {
        assert_eq!(
            jpg_fragment("https://x/y?signed=abc"),
            "https://x/y?signed=abc#.jpg"
        );
        assert_eq!(jpg_fragment("https://x/cover.jpg"), "https://x/cover.jpg");
        assert_eq!(jpg_fragment("https://x/cover.PNG"), "https://x/cover.PNG");
    }

    #[test]
    fn podcast_id_regex_matches_hex_and_hyphens() {
        assert!(PODCAST_ID_RE.is_match("de9b2081-9fc5-489f-b9d3-d744ed9cab20"));
        assert!(PODCAST_ID_RE.is_match("1234567890"));
        assert!(!PODCAST_ID_RE.is_match("not-a-valid-id!"));
        assert!(!PODCAST_ID_RE.is_match(""));
    }

    #[test]
    fn extract_podcast_id_passes_through_bare_id() {
        assert_eq!(
            extract_podcast_id("de9b2081-9fc5-489f-b9d3-d744ed9cab20"),
            Some("de9b2081-9fc5-489f-b9d3-d744ed9cab20")
        );
        assert_eq!(extract_podcast_id("1234567890"), Some("1234567890"));
    }

    #[test]
    fn extract_podcast_id_pulls_uuid_from_open_url() {
        assert_eq!(
            extract_podcast_id(
                "https://open.podimo.com/podcast/de9b2081-9fc5-489f-b9d3-d744ed9cab20"
            ),
            Some("de9b2081-9fc5-489f-b9d3-d744ed9cab20")
        );
    }

    #[test]
    fn extract_podcast_id_pulls_uuid_from_shows_url() {
        assert_eq!(
            extract_podcast_id(
                "https://podimo.com/nl-nl/shows/de9b2081-9fc5-489f-b9d3-d744ed9cab20?ref=share"
            ),
            Some("de9b2081-9fc5-489f-b9d3-d744ed9cab20")
        );
    }

    #[test]
    fn extract_podcast_id_trims_whitespace() {
        assert_eq!(
            extract_podcast_id("   de9b2081-9fc5-489f-b9d3-d744ed9cab20\n  "),
            Some("de9b2081-9fc5-489f-b9d3-d744ed9cab20")
        );
    }

    #[test]
    fn extract_podcast_id_rejects_empty_and_garbage() {
        assert_eq!(extract_podcast_id(""), None);
        assert_eq!(extract_podcast_id("   "), None);
        assert_eq!(extract_podcast_id("not a url and not an id"), None);
    }

    #[test]
    fn parse_podimo_input_bare_uuid_defaults_to_podcast() {
        let (kind, id) =
            parse_podimo_input("de9b2081-9fc5-489f-b9d3-d744ed9cab20").expect("parses");
        assert_eq!(kind, PodimoKind::Podcast);
        assert_eq!(id, "de9b2081-9fc5-489f-b9d3-d744ed9cab20");
    }

    #[test]
    fn parse_podimo_input_audiobook_url_detected() {
        let (kind, id) = parse_podimo_input(
            "https://open.podimo.com/audiobook/fefa939e-c84d-4c16-8bbf-9575e1379d81",
        )
        .expect("parses");
        assert_eq!(kind, PodimoKind::Audiobook);
        assert_eq!(id, "fefa939e-c84d-4c16-8bbf-9575e1379d81");
    }

    #[test]
    fn parse_podimo_input_podcast_url_detected() {
        let (kind, id) = parse_podimo_input(
            "https://open.podimo.com/podcast/de9b2081-9fc5-489f-b9d3-d744ed9cab20",
        )
        .expect("parses");
        assert_eq!(kind, PodimoKind::Podcast);
        assert_eq!(id, "de9b2081-9fc5-489f-b9d3-d744ed9cab20");
    }

    #[test]
    fn parse_podimo_input_shows_url_treated_as_podcast() {
        let (kind, _id) = parse_podimo_input(
            "https://podimo.com/nl-nl/shows/de9b2081-9fc5-489f-b9d3-d744ed9cab20",
        )
        .expect("parses");
        assert_eq!(kind, PodimoKind::Podcast);
    }

    #[test]
    fn parse_podimo_input_dutch_audioboek_url_detected() {
        // Defensive: Podimo localises some paths; accept `/audioboek/` too.
        let (kind, _id) = parse_podimo_input(
            "https://podimo.com/nl-nl/audioboek/fefa939e-c84d-4c16-8bbf-9575e1379d81",
        )
        .expect("parses");
        assert_eq!(kind, PodimoKind::Audiobook);
    }

    #[test]
    fn parse_podimo_input_rejects_garbage() {
        assert!(parse_podimo_input("").is_none());
        assert!(parse_podimo_input("hello world").is_none());
    }

    #[test]
    fn route_segment_matches_handlers() {
        assert_eq!(PodimoKind::Podcast.route_segment(), "feed");
        assert_eq!(PodimoKind::Audiobook.route_segment(), "audiobook");
    }

    #[test]
    fn parse_basic_auth_decodes_user_and_password() {
        use base64::engine::general_purpose::STANDARD as BASE64;
        use base64::Engine;
        let mut h = axum::http::HeaderMap::new();
        let raw = BASE64.encode("a@b.com,nl,nl-NL:secret");
        h.insert(
            axum::http::header::AUTHORIZATION,
            format!("Basic {raw}").parse().unwrap(),
        );
        let (user, pass) = parse_basic_auth(&h).expect("decodes");
        assert_eq!(user, "a@b.com,nl,nl-NL");
        assert_eq!(pass, "secret");
    }

    #[test]
    fn parse_basic_auth_missing_header_returns_none() {
        let h = axum::http::HeaderMap::new();
        assert!(parse_basic_auth(&h).is_none());
    }

    #[test]
    fn random_hex_id_length_and_alphabet() {
        for n in [0, 1, 16, 64] {
            let s = random_hex_id(n);
            assert_eq!(s.len(), n, "random_hex_id({n}) wrong length");
            assert!(
                s.chars().all(|c| c.is_ascii_hexdigit()),
                "random_hex_id({n}) leaked non-hex: {s:?}"
            );
        }
    }

    #[test]
    fn token_key_distinct_for_different_inputs() {
        let base = token_key("user@example.com", "hunter2");
        assert_ne!(token_key("other@example.com", "hunter2"), base);
        assert_ne!(token_key("user@example.com", "different"), base);
    }

    #[test]
    fn random_flyer_id_format() {
        let s = random_flyer_id();
        let parts: Vec<&str> = s.split('-').collect();
        assert_eq!(parts.len(), 2);
        for p in parts {
            assert_eq!(p.len(), 13);
            assert!(p.chars().all(|c| c.is_ascii_digit()));
        }
    }
}
