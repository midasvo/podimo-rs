//! Configuration: env + .env.

use std::env;
use std::sync::Arc;

use serde::Deserialize;

use crate::podimo::hls::StreamFormat;
use crate::util::parse_bool_loose;

pub type SharedConfig = Arc<Config>;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub hostname: String,
    pub bind_host: String,
    pub protocol: String,

    pub http_proxy: Option<String>,
    pub zenrows_api: Option<String>,
    pub scraper_api: Option<String>,

    pub cache_dir: String,
    pub block_list_file: String,

    pub debug: bool,

    pub local_credentials: bool,
    pub podimo_email: Option<String>,
    pub podimo_password: Option<String>,

    pub store_tokens_on_disk: bool,
    pub token_cache_time: u64,
    pub podcast_cache_time: u64,
    pub head_cache_time: u64,
    /// TTL for the short-lived audiobook audio URL (`ShortLivedAudiobookMediaUrlQuery`).
    /// The URL is signed and expires upstream; default keeps it brief so podcatchers
    /// always see a fresh link.
    pub audiobook_audio_cache_time: u64,

    /// Enables the audiobook library (`/library/*` endpoints). Requires
    /// `LOCAL_CREDENTIALS=true` because library entries are single-user.
    pub enable_library: bool,
    /// Where downloaded audiobooks are stored when the library is enabled.
    pub library_dir: String,

    pub public_feeds: bool,

    /// Format the feed links HLS episodes as: `mp3` (re-encoded, plays
    /// everywhere) or `m4a` (Podimo's AAC repackaged, near-zero CPU).
    /// `/stream` serves both either way; this only picks the enclosure.
    pub stream_format: StreamFormat,

    /// Podimo GraphQL endpoint. Overridable via `PODIMO_GRAPHQL_URL` so
    /// integration tests can point at a wiremock server. Defaults to the
    /// production URL — there is no operational reason to override in prod.
    pub graphql_url: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            hostname: env_or("PODIMO_HOSTNAME", "localhost:12104"),
            bind_host: env_or("PODIMO_BIND_HOST", "127.0.0.1:12104"),
            protocol: env_or("PODIMO_PROTOCOL", "http"),

            http_proxy: env_opt("HTTP_PROXY"),
            zenrows_api: env_opt("ZENROWS_API"),
            scraper_api: env_opt("SCRAPER_API"),

            cache_dir: std::path::PathBuf::from(env_or("CACHE_DIR", "./cache"))
                .canonicalize()
                .ok()
                .and_then(|p| p.to_str().map(String::from))
                .unwrap_or_else(|| env_or("CACHE_DIR", "./cache")),
            block_list_file: env_or("BLOCK_LIST_FILE", "./.block-list"),

            debug: env_bool("DEBUG", false),

            local_credentials: env_bool("LOCAL_CREDENTIALS", false),
            podimo_email: env_opt("PODIMO_EMAIL"),
            podimo_password: env_opt("PODIMO_PASSWORD"),

            store_tokens_on_disk: env_bool("STORE_TOKENS_ON_DISK", true),
            token_cache_time: env_u64("TOKEN_CACHE_TIME", 3600 * 24 * 5),
            podcast_cache_time: env_u64("PODCAST_CACHE_TIME", 21_600),
            head_cache_time: env_u64("HEAD_CACHE_TIME", 7 * 60 * 60 * 24),
            audiobook_audio_cache_time: env_u64("AUDIOBOOK_AUDIO_CACHE_TIME", 600),

            enable_library: env_bool("ENABLE_LIBRARY", false),
            library_dir: env_or("LIBRARY_DIR", "./library"),

            public_feeds: env_bool("PUBLIC_FEEDS", false),

            stream_format: env_stream_format()?,

            graphql_url: env_or("PODIMO_GRAPHQL_URL", "https://podimo.com/graphql"),
        })
    }

    pub fn into_shared(self) -> SharedConfig {
        Arc::new(self)
    }

    pub fn log_startup(&self) {
        if !self.debug {
            return;
        }
        tracing::info!(target: "podimo", "DEBUG: {}", self.debug);
        tracing::info!(target: "podimo", "LOCAL_CREDENTIALS: {} ({:?})", self.local_credentials, self.podimo_email);
        tracing::info!(target: "podimo", "PODIMO_HOSTNAME: {}", self.hostname);
        tracing::info!(target: "podimo", "PODIMO_BIND_HOST: {}", self.bind_host);
        tracing::info!(target: "podimo", "PODIMO_PROTOCOL: {}", self.protocol);
        tracing::info!(target: "podimo", "PUBLIC_FEEDS: {}", self.public_feeds);
        tracing::info!(target: "podimo", "STREAM_FORMAT: {}", self.stream_format.extension());
        tracing::info!(target: "podimo", "HTTP_PROXY: {:?}", self.http_proxy);
        tracing::info!(target: "podimo", "ZENROWS_API set: {}", self.zenrows_api.is_some());
        tracing::info!(target: "podimo", "SCRAPER_API set: {}", self.scraper_api.is_some());
        tracing::info!(target: "podimo", "CACHE_DIR: {}", self.cache_dir);
        tracing::info!(target: "podimo", "STORE_TOKENS_ON_DISK: {}", self.store_tokens_on_disk);
        tracing::info!(target: "podimo", "TOKEN_CACHE_TIME: {} sec", self.token_cache_time);
        tracing::info!(target: "podimo", "PODCAST_CACHE_TIME: {} sec", self.podcast_cache_time);
        tracing::info!(target: "podimo", "HEAD_CACHE_TIME: {} sec", self.head_cache_time);
        tracing::info!(target: "podimo", "AUDIOBOOK_AUDIO_CACHE_TIME: {} sec", self.audiobook_audio_cache_time);
        tracing::info!(target: "podimo", "ENABLE_LIBRARY: {}", self.enable_library);
        if self.enable_library {
            tracing::info!(target: "podimo", "LIBRARY_DIR: {}", self.library_dir);
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_opt(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.is_empty())
}

fn env_bool(key: &str, default: bool) -> bool {
    env::var(key)
        .map(|v| parse_bool_loose(&v))
        .unwrap_or(default)
}

/// Unlike the lenient helpers above, a typo here fails startup: silently
/// falling back would hide which format the feed actually serves.
fn env_stream_format() -> anyhow::Result<StreamFormat> {
    let value = env_or("STREAM_FORMAT", "mp3");
    StreamFormat::from_extension(&value.trim().to_ascii_lowercase())
        .ok_or_else(|| anyhow::anyhow!("STREAM_FORMAT must be `mp3` or `m4a`, got `{value}`"))
}

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

pub const REGIONS: &[(&str, &str)] = &[
    ("nl", "Nederland"),
    ("de", "Deutschland"),
    ("dk", "Danmark"),
    ("es", "España"),
    ("latam", "America latina"),
    ("en", "International"),
    ("mx", "Mexico"),
    ("no", "Norge"),
    ("fi", "Suomi"),
    ("uk", "United Kingdom"),
];

pub const LOCALES: &[&str] = &[
    "nl-NL", "de-DE", "da-DK", "es-ES", "en-US", "es-MX", "no-NO", "fi-FI", "en-GB",
];

pub fn is_known_region(code: &str) -> bool {
    REGIONS.iter().any(|(c, _)| *c == code)
}

pub fn is_known_locale(code: &str) -> bool {
    LOCALES.contains(&code)
}
