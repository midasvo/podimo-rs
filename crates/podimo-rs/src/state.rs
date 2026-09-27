//! App-wide shared state. Wraps the config, caches, blocklist, HTTP scraper
//! client, and templates so handlers can take a single `State<AppState>`.

use std::sync::Arc;

use reqwest::Client;

use crate::blocklist::BlockList;
use crate::cache::Caches;
use crate::config::Config;
use crate::episode_files::EpisodeFiles;
use crate::library::Library;
use crate::templates::Templates;

#[derive(Clone)]
pub struct AppState {
    pub(crate) config: Arc<Config>,
    /// Public to let integration tests seed caches (e.g. pre-populate the
    /// HEAD cache so tests stay offline).
    pub caches: Caches,
    pub(crate) blocklist: Arc<BlockList>,
    pub(crate) scraper: Client,
    pub(crate) templates: Templates,
    /// Finished M4A episodes, served from disk (see `episode_files`).
    pub(crate) episode_files: EpisodeFiles,
    /// `Some` when `ENABLE_LIBRARY=true`; handlers under `/library/*` reject
    /// requests with 404 when this is `None`. Public so integration tests can
    /// seed entries directly without driving the HTTP `/library/add` flow.
    pub library: Option<Library>,
}

impl AppState {
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let caches = Caches::init(
            &config.cache_dir,
            config.store_tokens_on_disk,
            config.token_cache_time,
            config.podcast_cache_time,
            config.audiobook_audio_cache_time,
            config.head_cache_time,
        )
        .await;

        let blocklist = Arc::new(BlockList::load(&config.block_list_file));

        let mut builder = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .cookie_store(true)
            .user_agent("Podimo/2.45.1 build 566/Android 33");

        if let Some(proxy) = &config.http_proxy {
            match reqwest::Proxy::https(proxy) {
                Ok(p) => builder = builder.proxy(p),
                Err(err) => tracing::warn!(target: "podimo", "ignoring invalid HTTP_PROXY: {err}"),
            }
        }
        let scraper = builder.build()?;

        let library = if config.enable_library {
            if !config.local_credentials {
                tracing::warn!(
                    target: "podimo::library",
                    "ENABLE_LIBRARY=true requires LOCAL_CREDENTIALS=true — disabling library."
                );
                None
            } else {
                Some(Library::new(&config.library_dir).await?)
            }
        } else {
            None
        };

        let episode_files = EpisodeFiles::new()?;
        tracing::debug!(
            target: "podimo",
            "episode files in {}",
            episode_files.dir().display()
        );

        Ok(Self {
            config: Arc::new(config),
            caches,
            blocklist,
            scraper,
            templates: Templates::new(),
            episode_files,
            library,
        })
    }
}
