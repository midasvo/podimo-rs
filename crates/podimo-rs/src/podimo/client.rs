//! GraphQL client for Podimo.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde::Serialize;
use serde_json::{json, Value};
use thiserror::Error;

use crate::cache::TtlCache;
use crate::config::Config;
use crate::util::{generate_headers, is_correct_email, random_flyer_id, token_key};

#[derive(Debug, Error)]
pub enum ClientError {
    /// Bad credentials. Maps to 401.
    #[error("invalid credentials: {0}")]
    InvalidCredentials(String),

    /// Network / upstream failure. Maps to 503.
    #[error("upstream unavailable: {0}")]
    Upstream(String),

    /// GraphQL returned errors. Inspect the message for "not found" routing.
    #[error("graphql error: {0}")]
    GraphQl(String),
}

impl ClientError {
    pub fn is_not_found(&self) -> bool {
        matches!(self, ClientError::GraphQl(msg) if msg.to_lowercase().contains("not found"))
    }
}

#[derive(Debug, Clone)]
pub struct PodimoClient {
    pub username: String,
    pub password: String,
    pub region: String,
    pub locale: String,
    pub key: String,
    pub token: Option<String>,
    preauth_token: Option<String>,
    prereg_id: Option<String>,
}

impl PodimoClient {
    pub fn new(
        username: &str,
        password: &str,
        region: &str,
        locale: &str,
    ) -> Result<Self, ClientError> {
        if username.is_empty() || password.is_empty() {
            return Err(ClientError::InvalidCredentials(
                "empty username or password".into(),
            ));
        }
        if username.len() > 256 || password.len() > 256 {
            return Err(ClientError::InvalidCredentials(
                "username or password are too long".into(),
            ));
        }
        if !is_correct_email(username) {
            return Err(ClientError::InvalidCredentials(
                "email is not in the correct format".into(),
            ));
        }

        Ok(Self {
            username: username.to_string(),
            password: password.to_string(),
            region: region.to_string(),
            locale: locale.to_string(),
            key: token_key(username, password),
            token: None,
            preauth_token: None,
            prereg_id: None,
        })
    }

    /// Three-step login dance: pre-register → onboarding → authorize. Returns
    /// the bearer token to use on subsequent requests.
    pub async fn login(
        &mut self,
        scraper: &Client,
        config: &Config,
    ) -> Result<String, ClientError> {
        self.get_preregister_token(scraper, config).await?;
        self.get_onboarding_id(scraper, config).await?;

        let preauth = self.preauth_token.as_deref().ok_or_else(|| {
            ClientError::Upstream("preauth token missing after pre-register".into())
        })?;

        let headers = generate_headers(Some(preauth), &self.locale);
        let query = r#"
            query AuthorizationAuthorize($email: String!, $password: String!, $locale: String!, $preregisterId: String) {
                tokenWithCredentials(
                    email: $email
                    password: $password
                    locale: $locale
                    preregisterId: $preregisterId
                ) {
                    token
                }
            }
        "#;
        let variables = json!({
            "email": self.username,
            "password": self.password,
            "locale": self.locale,
            "preregisterId": self.prereg_id,
        });
        let result = post_graphql(scraper, config, &headers, query, &variables).await?;
        let token = get_str(&result, &["tokenWithCredentials", "token"])
            .ok_or_else(|| ClientError::InvalidCredentials("no token in response".into()))?
            .to_string();
        self.token = Some(token.clone());
        Ok(token)
    }

    async fn get_preregister_token(
        &mut self,
        scraper: &Client,
        config: &Config,
    ) -> Result<(), ClientError> {
        let headers = generate_headers(None, &self.locale);
        let query = r#"
            query AuthorizationPreregisterUser($locale: String!, $referenceUser: String, $countryCode: String, $appsFlyerId: String) {
                tokenWithPreregisterUser(
                    locale: $locale
                    referenceUser: $referenceUser
                    countryCode: $countryCode
                    source: MOBILE
                    appsFlyerId: $appsFlyerId
                    currentCountry: $countryCode
                ) {
                    token
                }
            }
        "#;
        let variables = json!({
            "locale": self.locale,
            "countryCode": self.region,
            "appsFlyerId": random_flyer_id(),
        });
        let result = post_graphql(scraper, config, &headers, query, &variables).await?;
        let token = get_str(&result, &["tokenWithPreregisterUser", "token"])
            .ok_or_else(|| ClientError::Upstream("no tokenWithPreregisterUser".into()))?
            .to_string();
        self.preauth_token = Some(token);
        Ok(())
    }

    async fn get_onboarding_id(
        &mut self,
        scraper: &Client,
        config: &Config,
    ) -> Result<(), ClientError> {
        let preauth = self.preauth_token.as_deref().ok_or_else(|| {
            ClientError::Upstream("preauth token missing before onboarding".into())
        })?;
        let headers = generate_headers(Some(preauth), &self.locale);
        let query = r#"
            query OnboardingQuery {
                userOnboardingFlow {
                    id
                }
            }
        "#;
        let variables = json!({ "locale": self.locale, "countryCode": self.region, "appsFlyerId": random_flyer_id() });
        let result = post_graphql(scraper, config, &headers, query, &variables).await?;
        let id = get_str(&result, &["userOnboardingFlow", "id"])
            .ok_or_else(|| ClientError::Upstream("no userOnboardingFlow.id".into()))?
            .to_string();
        self.prereg_id = Some(id);
        Ok(())
    }

    /// Page through `podcastEpisodes`, newest first, and return the payload.
    /// Always fetches whole pages of 100. With `limit` (the feed's `?limit=`)
    /// paging stops once there are enough episodes: for a 500-episode show,
    /// `limit=Some(20)` is one GraphQL call instead of six. Without one, it
    /// pages until the show runs out. The caller trims to `limit`.
    ///
    /// Cached per account, since the payload holds signed media URLs issued
    /// to this login. Limits in the same hundred share an entry (`p1` for
    /// 1..=100, `p2` for 101..=200, …), and a listing that reached the end of
    /// the show is stored as `all` and serves any limit. So an account has at
    /// most one entry per page of a show, whatever limits are requested.
    pub async fn get_podcasts(
        &self,
        scraper: &Client,
        config: &Config,
        podcast_id: &str,
        limit: Option<usize>,
        podcast_cache: &TtlCache<Arc<Value>>,
    ) -> Result<Arc<Value>, ClientError> {
        const PAGE_MAX: usize = 100;
        let all_key = format!("{}__{podcast_id}__all", self.key);
        if let Some(cached) = podcast_cache.get(&all_key).await {
            return Ok(cached);
        }
        let pages_wanted = limit.map(|n| n.div_ceil(PAGE_MAX));
        let pages_key = pages_wanted.map(|p| format!("{}__{podcast_id}__p{p}", self.key));
        if let Some(key) = &pages_key {
            if let Some(cached) = podcast_cache.get(key).await {
                return Ok(cached);
            }
        }

        let token = self
            .token
            .as_deref()
            .ok_or_else(|| ClientError::InvalidCredentials("login not yet completed".into()))?;
        let headers = generate_headers(Some(token), &self.locale);

        let query = r#"
            query ChannelEpisodesQuery($podcastId: String!, $limit: Int!, $offset: Int!, $sorting: PodcastEpisodeSorting) {
                episodes: podcastEpisodes(
                    podcastId: $podcastId
                    converted: true
                    published: true
                    limit: $limit
                    offset: $offset
                    sorting: $sorting
                ) {
                    ...EpisodeBase
                }
                podcast: podcastById(podcastId: $podcastId) {
                    title
                    description
                    webAddress
                    authorName
                    language
                    images {
                        coverImageUrl
                    }
                }
            }

            fragment EpisodeBase on PodcastEpisode {
                id
                artist
                podcastName
                imageUrl
                description
                datetime
                publishDatetime
                title
                audio {
                    url
                    duration
                }
                streamMedia {
                    duration
                    url
                }
            }
        "#;

        let mut offset = 0;
        let mut pages = 0;
        let mut full: Option<Value> = None;

        // True when the show ran out, false when `pages_wanted` stopped it.
        let exhausted = loop {
            let variables = json!({
                "podcastId": podcast_id,
                "limit": PAGE_MAX,
                "offset": offset,
                "sorting": "PUBLISHED_DESCENDING",
            });
            let result = post_graphql(scraper, config, &headers, query, &variables).await?;
            let page_episodes_len = result
                .get("episodes")
                .and_then(|e| e.as_array())
                .map_or(0, Vec::len);

            match full.as_mut() {
                None => {
                    full = Some(result);
                }
                Some(existing) => {
                    if let (Some(existing_eps), Some(new_eps)) = (
                        existing.get_mut("episodes").and_then(|e| e.as_array_mut()),
                        result.get("episodes").and_then(|e| e.as_array()),
                    ) {
                        existing_eps.extend_from_slice(new_eps);
                    }
                }
            }

            pages += 1;
            if page_episodes_len < PAGE_MAX {
                break true;
            }
            if pages_wanted.is_some_and(|wanted| pages >= wanted) {
                break false;
            }
            offset += page_episodes_len;
        };

        let result = full.ok_or_else(|| ClientError::Upstream("no episodes returned".into()))?;
        let arc = Arc::new(result);
        let cache_key = match pages_key {
            Some(key) if !exhausted => key,
            _ => all_key,
        };
        podcast_cache.insert(cache_key, Arc::clone(&arc)).await;
        Ok(arc)
    }

    /// Fetch the metadata for a single audiobook via `audiobookById`. Cached as
    /// `Arc<Value>` per account and `audiobook_id`. The schema mirrors what
    /// `audiobook-dl` uses, including `authors`, `narrators`, and
    /// `coverImage.url`.
    pub async fn get_audiobook(
        &self,
        scraper: &Client,
        config: &Config,
        audiobook_id: &str,
        meta_cache: &TtlCache<Arc<Value>>,
    ) -> Result<Arc<Value>, ClientError> {
        let cache_key = format!("{}__{audiobook_id}", self.key);
        if let Some(cached) = meta_cache.get(&cache_key).await {
            return Ok(cached);
        }

        let token = self
            .token
            .as_deref()
            .ok_or_else(|| ClientError::InvalidCredentials("login not yet completed".into()))?;
        let headers = generate_headers(Some(token), &self.locale);

        // The full upstream query has many fragments + a side-channel
        // `audiobookExternalPurchaseLink`. We only need the fields used in RSS
        // rendering, so the query below is a trimmed-down equivalent.
        let query = r#"
            query AudiobookResultsQuery($id: String!) {
                audiobookById(id: $id) {
                    id
                    title
                    authorNames
                    description
                    duration
                    publisherName
                    yearOfBookPublication
                    authors { name }
                    narrators { name }
                    coverImage { url }
                    language { isoLanguage }
                }
            }
        "#;
        let variables = json!({ "id": audiobook_id });
        let data = post_graphql(scraper, config, &headers, query, &variables).await?;
        // GraphQL can return `data.audiobookById: null` for a non-existent id
        // without surfacing it as an `errors[]` block. Treat that as not-found.
        if data
            .get("audiobookById")
            .map(|v| v.is_null())
            .unwrap_or(true)
        {
            return Err(ClientError::GraphQl("audiobook not found".into()));
        }

        let arc = Arc::new(data);
        meta_cache.insert(cache_key, Arc::clone(&arc)).await;
        Ok(arc)
    }

    /// Fetch the short-lived signed audio URL for an audiobook. Cached per
    /// account, since the URL is issued to this login, and with a short TTL
    /// since the URL itself expires.
    pub async fn get_audiobook_audio_url(
        &self,
        scraper: &Client,
        config: &Config,
        audiobook_id: &str,
        audio_cache: &TtlCache<String>,
    ) -> Result<String, ClientError> {
        let cache_key = format!("{}__{audiobook_id}", self.key);
        if let Some(cached) = audio_cache.get(&cache_key).await {
            return Ok(cached);
        }

        let token = self
            .token
            .as_deref()
            .ok_or_else(|| ClientError::InvalidCredentials("login not yet completed".into()))?;
        let headers = generate_headers(Some(token), &self.locale);

        let query = r#"
            query ShortLivedAudiobookMediaUrlQuery($id: String!) {
                audiobookAudioById(audiobookId: $id) {
                    url
                }
            }
        "#;
        let variables = json!({ "id": audiobook_id });
        let data = post_graphql(scraper, config, &headers, query, &variables).await?;

        let url = get_str(&data, &["audiobookAudioById", "url"])
            .ok_or_else(|| ClientError::GraphQl("no audiobook audio url in response".into()))?
            .to_string();
        audio_cache.insert(cache_key, url.clone()).await;
        Ok(url)
    }
}

fn get_str<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for &k in path {
        cur = cur.get(k)?;
    }
    cur.as_str()
}

/// Decides which URL/client to use for the Cloudflare bypass and posts a
/// GraphQL query. Mirrors `PodimoClient.post` in Python.
async fn post_graphql(
    scraper: &Client,
    config: &Config,
    headers: &std::collections::HashMap<String, String>,
    query: &str,
    variables: &Value,
) -> Result<Value, ClientError> {
    #[derive(Serialize)]
    struct Body<'a> {
        query: &'a str,
        variables: &'a Value,
    }

    let body = Body { query, variables };

    let url = graphql_request_url(config);
    let response = build_request(scraper, &url, headers, &body).await?;

    if !response.status().is_success() {
        // Don't leak proxy API keys in the URL — log the configured destination
        // host, never the formatted SCRAPER_API/ZENROWS_API URL that includes the key.
        return Err(ClientError::Upstream(format!(
            "Podimo returned status {} (target host: {})",
            response.status(),
            host_only(&url),
        )));
    }

    let body: Value = response.json().await.map_err(|e| {
        ClientError::Upstream(format!("invalid JSON: {}", describe_reqwest_error(e)))
    })?;

    if let Some(errors) = body.get("errors").and_then(|e| e.as_array()) {
        let msg = errors
            .iter()
            .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(ClientError::GraphQl(msg));
    }

    body.get("data")
        .cloned()
        .ok_or_else(|| ClientError::GraphQl("no data field in response".into()))
}

/// Where GraphQL requests go: through ScraperAPI or ZenRows when one is
/// configured (in that order), else straight to `graphql_url`. Both proxies
/// only pass our headers on (the auth tokens among them) when asked to.
/// The proxy URLs carry the API key, so they must never end up in errors.
fn graphql_request_url(config: &Config) -> String {
    let target = urlencoding::encode(&config.graphql_url);
    if let Some(api_key) = &config.scraper_api {
        format!(
            "https://api.scraperapi.com?api_key={}&url={target}&keep_headers=true",
            urlencoding::encode(api_key),
        )
    } else if let Some(api_key) = &config.zenrows_api {
        format!(
            "https://api.zenrows.com/v1/?apikey={}&url={target}&custom_headers=true",
            urlencoding::encode(api_key),
        )
    } else {
        config.graphql_url.clone()
    }
}

fn host_only(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    after_scheme
        .split(['/', '?'])
        .next()
        .unwrap_or("")
        .to_string()
}

/// `reqwest::Error` text without the request URL, which carries the
/// SCRAPER_API / ZENROWS_API key. Names the host instead.
fn describe_reqwest_error(err: reqwest::Error) -> String {
    let host = err.url().and_then(|u| u.host_str()).map(str::to_owned);
    let msg = err.without_url().to_string();
    match host {
        Some(host) => format!("{msg} (target host: {host})"),
        None => msg,
    }
}

async fn build_request<B: Serialize>(
    scraper: &Client,
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    body: &B,
) -> Result<reqwest::Response, ClientError> {
    let mut req = scraper
        .post(url)
        .timeout(Duration::from_secs(30))
        .json(body);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    req.send()
        .await
        .map_err(|e| ClientError::Upstream(describe_reqwest_error(e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::podimo::hls::StreamFormat;

    fn config(scraper_api: Option<&str>, zenrows_api: Option<&str>) -> Config {
        Config {
            hostname: "localhost:12104".into(),
            bind_host: "127.0.0.1:12104".into(),
            protocol: "http".into(),
            http_proxy: None,
            zenrows_api: zenrows_api.map(String::from),
            scraper_api: scraper_api.map(String::from),
            cache_dir: "./cache".into(),
            block_list_file: "/dev/null".into(),
            debug: false,
            local_credentials: false,
            podimo_email: None,
            podimo_password: None,
            store_tokens_on_disk: false,
            token_cache_time: 60,
            podcast_cache_time: 60,
            head_cache_time: 60,
            audiobook_audio_cache_time: 60,
            enable_library: false,
            library_dir: "./library".into(),
            public_feeds: false,
            stream_format: StreamFormat::M4a,
            stream_links_from_request: true,
            graphql_url: "https://podimo.com/graphql".into(),
        }
    }

    const ENCODED_GRAPHQL_URL: &str = "url=https%3A%2F%2Fpodimo.com%2Fgraphql";

    #[test]
    fn zenrows_is_asked_to_pass_our_headers_on() {
        let url = graphql_request_url(&config(None, Some("ZR KEY")));
        assert!(url.starts_with("https://api.zenrows.com/v1/?"), "{url}");
        assert!(url.contains("apikey=ZR%20KEY"), "{url}");
        assert!(url.contains(ENCODED_GRAPHQL_URL), "{url}");
        assert!(url.contains("&custom_headers=true"), "{url}");
    }

    #[test]
    fn scraperapi_comes_first_and_keeps_our_headers() {
        let url = graphql_request_url(&config(Some("SA"), Some("ZR")));
        assert!(
            url.starts_with("https://api.scraperapi.com?api_key=SA&"),
            "{url}"
        );
        assert!(url.contains(ENCODED_GRAPHQL_URL), "{url}");
        assert!(url.contains("&keep_headers=true"), "{url}");
    }

    #[test]
    fn without_a_proxy_key_requests_go_straight_to_podimo() {
        assert_eq!(
            graphql_request_url(&config(None, None)),
            "https://podimo.com/graphql"
        );
    }

    #[tokio::test]
    async fn reqwest_errors_do_not_leak_the_url() {
        // Port 1 on localhost refuses the connection.
        let err = reqwest::Client::new()
            .post("http://127.0.0.1:1/?api_key=SECRET")
            .send()
            .await
            .unwrap_err();
        let msg = describe_reqwest_error(err);
        assert!(!msg.contains("SECRET"), "{msg}");
        assert!(msg.contains("127.0.0.1"), "{msg}");
    }
}
