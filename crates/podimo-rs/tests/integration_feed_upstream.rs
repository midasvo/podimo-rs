//! End-to-end feed-flow tests with a mocked Podimo GraphQL upstream.
//!
//! Wiremock stands in for `https://podimo.com/graphql`; the head cache is
//! pre-populated so no episode-media HEAD probe goes out.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use podimo_rs::cache::HeadInfo;
use podimo_rs::{app, config::Config, AppState};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_config(graphql_url: String) -> Config {
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let path = cache_dir.path().to_string_lossy().to_string();
    std::mem::forget(cache_dir);
    Config {
        hostname: "localhost:12104".into(),
        bind_host: "127.0.0.1:12104".into(),
        protocol: "http".into(),
        http_proxy: None,
        zenrows_api: None,
        scraper_api: None,
        cache_dir: path,
        block_list_file: "/dev/null".into(),
        debug: false,
        local_credentials: false,
        podimo_email: None,
        podimo_password: None,
        podimo_region: "nl".into(),
        podimo_locale: "nl-NL".into(),
        store_tokens_on_disk: false,
        token_cache_time: 60,
        podcast_cache_time: 60,
        head_cache_time: 60,
        audiobook_audio_cache_time: 60,
        enable_library: false,
        library_dir: "./library".into(),
        public_feeds: false,
        graphql_url,
        stream_format: podimo_rs::podimo::hls::StreamFormat::Mp3,
        stream_links_from_request: true,
    }
}

async fn boot_with_state(state: AppState) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state).await.unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (addr, handle)
}

fn basic_auth(username: &str, password: &str) -> String {
    let raw = format!("{username}:{password}");
    format!("Basic {}", BASE64.encode(raw))
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

/// Single dispatching mock for `POST /graphql`. The body's `query` field
/// determines which Podimo operation is being invoked; the supplied
/// `episodes_response` is returned for the channelEpisodes query.
async fn install_graphql_mock(server: &MockServer, episodes_response: ResponseTemplate) {
    let episodes = std::sync::Arc::new(std::sync::Mutex::new(Some(episodes_response)));
    let episodes = std::sync::Arc::clone(&episodes);
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let query = body.get("query").and_then(|q| q.as_str()).unwrap_or("");
            if query.contains("AuthorizationPreregisterUser") {
                ResponseTemplate::new(200).set_body_json(json!({
                    "data": { "tokenWithPreregisterUser": { "token": "preauth-token" } }
                }))
            } else if query.contains("OnboardingQuery") {
                ResponseTemplate::new(200).set_body_json(json!({
                    "data": { "userOnboardingFlow": { "id": "onboarding-id" } }
                }))
            } else if query.contains("AuthorizationAuthorize") {
                ResponseTemplate::new(200).set_body_json(json!({
                    "data": { "tokenWithCredentials": { "token": "user-token" } }
                }))
            } else if query.contains("ChannelEpisodesQuery") {
                // The handler may call this multiple times for pagination. We
                // hand back the canned response each time so subsequent calls
                // (offset=100) see the same episode count and the loop exits.
                match episodes.lock().unwrap().clone() {
                    Some(tmpl) => tmpl,
                    None => ResponseTemplate::new(500),
                }
            } else {
                ResponseTemplate::new(500).set_body_string("unexpected graphql query")
            }
        })
        .mount(server)
        .await;
}

fn fake_episodes_payload() -> Value {
    json!({
        "data": {
            "podcast": {
                "title": "Test Show",
                "description": "Hello world",
                "webAddress": null,
                "authorName": "Author",
                "language": "nl",
                "images": { "coverImageUrl": "https://example.com/cover.jpg" }
            },
            "episodes": [
                {
                    "id": "ep1",
                    "title": "Episode 1",
                    "description": "First episode",
                    "publishDatetime": "2024-01-01T12:00:00Z",
                    "datetime": "2024-01-01T12:00:00Z",
                    "imageUrl": "https://example.com/ep1.jpg",
                    "audio": { "url": "https://example.com/ep1.mp3", "duration": 1234 },
                    "streamMedia": null,
                    "artist": "Author",
                    "podcastName": "Test Show"
                }
            ]
        }
    })
}

const PODCAST_ID: &str = "de9b2081-9fc5-489f-b9d3-d744ed9cab20";

#[tokio::test]
async fn happy_path_returns_rss() {
    let server = MockServer::start().await;
    install_graphql_mock(
        &server,
        ResponseTemplate::new(200).set_body_json(fake_episodes_payload()),
    )
    .await;

    let config = make_config(format!("{}/graphql", server.uri()));
    let state = AppState::new(config).await.unwrap();
    // Pre-populate the head cache so url_head_info short-circuits.
    state
        .caches
        .head
        .insert(
            "ep1".to_string(),
            HeadInfo {
                content_length: "9876".into(),
                content_type: "audio/mpeg".into(),
            },
        )
        .await;

    let (addr, handle) = boot_with_state(state).await;
    let resp = http_client()
        .get(format!("http://{addr}/feed/{PODCAST_ID}.xml"))
        .header("Authorization", basic_auth("a@b.com,nl,nl-NL", "pw"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "expected 200, got {}", resp.status());
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/xml",
        "feed must be served as text/xml"
    );
    assert_eq!(resp.headers().get("cache-control").unwrap(), "max-age=900");
    let body = resp.text().await.unwrap();
    assert!(body.contains("<rss"));
    assert!(body.contains("<title>Test Show</title>"));
    assert!(body.contains("<title>Episode 1</title>"));
    assert!(body.contains("https://example.com/ep1.mp3"));
    handle.abort();
}

/// Mocks a show whose one episode is HLS, so its enclosure is a `/stream`
/// link, and boots the app with `config` pointed at the mock.
async fn boot_hls_show(
    server: &MockServer,
    config: impl FnOnce(&mut Config),
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let mut payload = fake_episodes_payload();
    let episode = &mut payload["data"]["episodes"][0];
    episode["audio"] = Value::Null;
    episode["streamMedia"] = json!({
        "url": "https://media-cdn-episodes.podimo.com/ep1/ep1.m3u8?u=x&Signature=s",
        "duration": 1234
    });
    install_graphql_mock(server, ResponseTemplate::new(200).set_body_json(payload)).await;

    let mut cfg = make_config(format!("{}/graphql", server.uri()));
    config(&mut cfg);
    boot_with_state(AppState::new(cfg).await.unwrap()).await
}

#[tokio::test]
async fn stream_links_follow_the_host_the_feed_was_requested_on() {
    let server = MockServer::start().await;
    let (addr, handle) = boot_hls_show(&server, |c| {
        c.protocol = "https".into();
        c.hostname = "podimo.example.com".into();
    })
    .await;

    let feed = |extra: &'static [(&'static str, &'static str)]| {
        let mut req = http_client()
            .get(format!("http://{addr}/feed/{PODCAST_ID}.xml"))
            .header("Authorization", basic_auth("a@b.com,nl,nl-NL", "pw"));
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        async move { req.send().await.unwrap().text().await.unwrap() }
    };

    // In-cluster client talking to the service directly.
    let body = feed(&[("Host", "podimo")]).await;
    assert!(body.contains("url=\"http://podimo/stream/"), "{body}");

    // Same feed through a TLS-terminating reverse proxy.
    let body = feed(&[
        ("Host", "podimo.example.com"),
        ("X-Forwarded-Proto", "https"),
    ])
    .await;
    assert!(
        body.contains("url=\"https://podimo.example.com/stream/"),
        "{body}"
    );

    // A proxy that passes `Host` on without `X-Forwarded-Proto`: the
    // configured public host gets the configured scheme.
    let body = feed(&[("Host", "podimo.example.com")]).await;
    assert!(
        body.contains("url=\"https://podimo.example.com/stream/"),
        "{body}"
    );
    handle.abort();
}

#[tokio::test]
async fn stream_links_can_always_use_the_configured_address() {
    // For proxies that rewrite `Host`: the test request's own `Host` is
    // `127.0.0.1:<port>`, which no podcatcher outside could reach.
    let server = MockServer::start().await;
    let (addr, handle) = boot_hls_show(&server, |c| {
        c.stream_links_from_request = false;
        c.protocol = "https".into();
        c.hostname = "podimo.example.com".into();
    })
    .await;
    let body = http_client()
        .get(format!("http://{addr}/feed/{PODCAST_ID}.xml"))
        .header("Authorization", basic_auth("a@b.com,nl,nl-NL", "pw"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        body.contains("url=\"https://podimo.example.com/stream/"),
        "{body}"
    );
    handle.abort();
}

#[tokio::test]
async fn podcast_not_found_returns_404() {
    let server = MockServer::start().await;
    install_graphql_mock(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "errors": [{ "message": "Podcast not found" }]
        })),
    )
    .await;
    let config = make_config(format!("{}/graphql", server.uri()));
    let state = AppState::new(config).await.unwrap();
    let (addr, handle) = boot_with_state(state).await;
    let resp = http_client()
        .get(format!("http://{addr}/feed/{PODCAST_ID}.xml"))
        .header("Authorization", basic_auth("a@b.com,nl,nl-NL", "pw"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    handle.abort();
}

#[tokio::test]
async fn other_upstream_error_returns_500() {
    let server = MockServer::start().await;
    install_graphql_mock(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "errors": [{ "message": "upstream down" }]
        })),
    )
    .await;
    let config = make_config(format!("{}/graphql", server.uri()));
    let state = AppState::new(config).await.unwrap();
    let (addr, handle) = boot_with_state(state).await;
    let resp = http_client()
        .get(format!("http://{addr}/feed/{PODCAST_ID}.xml"))
        .header("Authorization", basic_auth("a@b.com,nl,nl-NL", "pw"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 500);
    handle.abort();
}

#[tokio::test]
async fn limit_query_param_avoids_full_pagination() {
    // Build an episodes mock that returns a full page (100 eps) every time. Without
    // a limit the handler would paginate forever — with `?limit=20` it must stop
    // after the first page. We assert by counting ChannelEpisodesQuery calls.
    let server = MockServer::start().await;
    let ep_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ep_calls_for_mock = std::sync::Arc::clone(&ep_calls);

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let query = body.get("query").and_then(|q| q.as_str()).unwrap_or("");
            if query.contains("AuthorizationPreregisterUser") {
                ResponseTemplate::new(200).set_body_json(json!({
                    "data": { "tokenWithPreregisterUser": { "token": "preauth-token" } }
                }))
            } else if query.contains("OnboardingQuery") {
                ResponseTemplate::new(200).set_body_json(json!({
                    "data": { "userOnboardingFlow": { "id": "onboarding-id" } }
                }))
            } else if query.contains("AuthorizationAuthorize") {
                ResponseTemplate::new(200).set_body_json(json!({
                    "data": { "tokenWithCredentials": { "token": "user-token" } }
                }))
            } else if query.contains("ChannelEpisodesQuery") {
                ep_calls_for_mock.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Hand back exactly the page size the caller asked for. That is
                // always a full page of 100, so the show never runs out and only
                // the limit can stop the paging: limit=20 needs one page.
                let vars = body.get("variables").cloned().unwrap_or(Value::Null);
                let want = vars
                    .get("limit")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(100)
                    .max(0) as usize;
                let mut eps = Vec::with_capacity(want);
                for i in 0..want {
                    eps.push(json!({
                        "id": format!("ep{i}"),
                        "title": format!("Episode {i}"),
                        "description": "",
                        "publishDatetime": "2024-01-01T12:00:00Z",
                        "datetime": "2024-01-01T12:00:00Z",
                        "imageUrl": "https://example.com/ep.jpg",
                        "audio": { "url": format!("https://example.com/ep{i}.mp3"), "duration": 1 },
                        "streamMedia": null,
                        "artist": "Author",
                        "podcastName": "Test Show"
                    }));
                }
                ResponseTemplate::new(200).set_body_json(json!({
                    "data": {
                        "podcast": {
                            "title": "Test Show",
                            "description": "Hello world",
                            "webAddress": null,
                            "authorName": "Author",
                            "language": "nl",
                            "images": { "coverImageUrl": "https://example.com/cover.jpg" }
                        },
                        "episodes": eps,
                    }
                }))
            } else {
                ResponseTemplate::new(500).set_body_string("unexpected graphql query")
            }
        })
        .mount(&server)
        .await;

    let config = make_config(format!("{}/graphql", server.uri()));
    let state = AppState::new(config).await.unwrap();
    // Pre-populate head cache so HEAD probes short-circuit for the 20 episodes
    // the feed keeps of the 100 fetched.
    for i in 0..20 {
        state
            .caches
            .head
            .insert(
                format!("ep{i}"),
                HeadInfo {
                    content_length: "1".into(),
                    content_type: "audio/mpeg".into(),
                },
            )
            .await;
    }

    let (addr, handle) = boot_with_state(state).await;
    let resp = http_client()
        .get(format!("http://{addr}/feed/{PODCAST_ID}.xml?limit=20"))
        .header("Authorization", basic_auth("a@b.com,nl,nl-NL", "pw"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // limit=20 fits in one page of 100, so exactly one ChannelEpisodesQuery
    // should fire.
    assert_eq!(
        ep_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "limit=20 must trigger exactly 1 ChannelEpisodesQuery, not full pagination"
    );

    let body = resp.text().await.unwrap();
    assert_eq!(body.matches("<item>").count(), 20, "want 20 items in feed");
    handle.abort();
}

/// Answers the three login queries, with a token per email address:
/// `token-<email>`. `None` for any other query.
fn login_response(body: &Value) -> Option<ResponseTemplate> {
    let query = body["query"].as_str().unwrap_or("");
    let data = if query.contains("AuthorizationPreregisterUser") {
        json!({ "tokenWithPreregisterUser": { "token": "preauth-token" } })
    } else if query.contains("OnboardingQuery") {
        json!({ "userOnboardingFlow": { "id": "onboarding-id" } })
    } else if query.contains("AuthorizationAuthorize") {
        let email = body["variables"]["email"].as_str().unwrap_or("");
        json!({ "tokenWithCredentials": { "token": format!("token-{email}") } })
    } else {
        return None;
    };
    Some(ResponseTemplate::new(200).set_body_json(json!({ "data": data })))
}

/// A show of `total` episodes, paged like Podimo does: each
/// `ChannelEpisodesQuery` returns up to `limit` episodes from `offset`.
/// Returns the number of those calls so far. The episodes are HLS, so the
/// feed renders without HEAD probes.
async fn install_show_mock(server: &MockServer, total: usize) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            if let Some(resp) = login_response(&body) {
                return resp;
            }
            counter.fetch_add(1, Ordering::SeqCst);
            let offset = body["variables"]["offset"].as_u64().unwrap_or(0) as usize;
            let limit = body["variables"]["limit"].as_u64().unwrap_or(0) as usize;
            let episodes: Vec<Value> = (offset..total.min(offset + limit))
                .map(|i| {
                    json!({
                        "id": format!("ep{i}"),
                        "title": format!("Episode {i}"),
                        "publishDatetime": "2024-01-01T12:00:00Z",
                        "audio": null,
                        "streamMedia": {
                            "url": format!("https://media-cdn-episodes.podimo.com/ep{i}/ep{i}.m3u8"),
                            "duration": 1
                        }
                    })
                })
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({
                "data": { "podcast": { "title": "Test Show" }, "episodes": episodes }
            }))
        })
        .mount(server)
        .await;
    calls
}

/// Number of `<item>`s in the feed for `query` (e.g. `?limit=20`).
async fn feed_items(addr: SocketAddr, query: &str) -> usize {
    let resp = http_client()
        .get(format!("http://{addr}/feed/{PODCAST_ID}.xml{query}"))
        .header("Authorization", basic_auth("a@b.com,nl,nl-NL", "pw"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{query}");
    resp.text().await.unwrap().matches("<item>").count()
}

#[tokio::test]
async fn accounts_do_not_share_cached_payloads() {
    // Podimo signs the media URLs in a listing for the account that asked. A
    // second account must get its own listing, not the cached one.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(|req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            if let Some(resp) = login_response(&body) {
                return resp;
            }
            let who = req
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let mut payload = fake_episodes_payload();
            payload["data"]["episodes"][0]["audio"]["url"] =
                json!(format!("https://example.com/ep1.mp3?who={who}"));
            ResponseTemplate::new(200).set_body_json(payload)
        })
        .mount(&server)
        .await;

    let config = make_config(format!("{}/graphql", server.uri()));
    let state = AppState::new(config).await.unwrap();
    state
        .caches
        .head
        .insert(
            "ep1".to_string(),
            HeadInfo {
                content_length: "9876".into(),
                content_type: "audio/mpeg".into(),
            },
        )
        .await;
    let (addr, handle) = boot_with_state(state).await;

    for email in ["a@b.com", "c@d.com"] {
        let body = http_client()
            .get(format!("http://{addr}/feed/{PODCAST_ID}.xml"))
            .header(
                "Authorization",
                basic_auth(&format!("{email},nl,nl-NL"), "pw"),
            )
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            body.contains(&format!("who=token-{email}")),
            "{email}: {body}"
        );
        assert_eq!(body.matches("who=").count(), 1, "{email}: {body}");
    }
    handle.abort();
}

#[tokio::test]
async fn limits_in_the_same_hundred_share_a_cache_entry() {
    let server = MockServer::start().await;
    let calls = install_show_mock(&server, 1000).await;
    let state = AppState::new(make_config(format!("{}/graphql", server.uri())))
        .await
        .unwrap();
    let (addr, handle) = boot_with_state(state).await;

    assert_eq!(feed_items(addr, "?limit=20").await, 20);
    assert_eq!(feed_items(addr, "?limit=50").await, 50);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "both fit the first page");

    assert_eq!(feed_items(addr, "?limit=150").await, 150);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "limit=150 fetches two pages"
    );
    handle.abort();
}

#[tokio::test]
async fn a_listing_that_reached_the_end_serves_any_limit() {
    // 30 episodes: the first page comes back short, so the show is complete.
    let server = MockServer::start().await;
    let calls = install_show_mock(&server, 30).await;
    let state = AppState::new(make_config(format!("{}/graphql", server.uri())))
        .await
        .unwrap();
    let (addr, handle) = boot_with_state(state).await;

    assert_eq!(feed_items(addr, "?limit=500").await, 30);
    assert_eq!(feed_items(addr, "?limit=501").await, 30);
    assert_eq!(feed_items(addr, "").await, 30);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    handle.abort();
}

#[tokio::test]
async fn upstream_5xx_during_auth_returns_503() {
    // When the upstream returns 5xx (e.g. Cloudflare block), the handler must
    // map that to 503 so clients distinguish it from a bad-credentials 401.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(503).set_body_string("Cloudflare blocked"))
        .mount(&server)
        .await;

    let config = make_config(format!("{}/graphql", server.uri()));
    let state = AppState::new(config).await.unwrap();
    let (addr, handle) = boot_with_state(state).await;
    let resp = http_client()
        .get(format!("http://{addr}/feed/{PODCAST_ID}.xml"))
        .header("Authorization", basic_auth("a@b.com,nl,nl-NL", "pw"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Upstream"), "body: {body}");
    handle.abort();
}

#[tokio::test]
async fn wrong_password_returns_401() {
    // Podimo answers a wrong email or password with a GraphQL error on the
    // login query. That must ask for new credentials, not say "retry later".
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(|req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let query = body["query"].as_str().unwrap_or("");
            if query.contains("AuthorizationAuthorize") {
                return ResponseTemplate::new(200).set_body_json(json!({
                    "errors": [{ "message": "Invalid credentials" }],
                    "data": null
                }));
            }
            login_response(&body).unwrap_or_else(|| ResponseTemplate::new(500))
        })
        .mount(&server)
        .await;
    let state = AppState::new(make_config(format!("{}/graphql", server.uri())))
        .await
        .unwrap();
    let (addr, handle) = boot_with_state(state).await;

    for route in ["feed", "audiobook"] {
        let resp = http_client()
            .get(format!("http://{addr}/{route}/{PODCAST_ID}.xml"))
            .header("Authorization", basic_auth("a@b.com,nl,nl-NL", "wrong"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "{route}");
        assert!(resp.headers().contains_key("www-authenticate"), "{route}");
    }
    handle.abort();
}

#[tokio::test]
async fn a_cached_token_upstream_rejects_is_dropped() {
    // Only `fresh-token` works upstream; the cache starts out holding one
    // Podimo no longer accepts.
    let server = MockServer::start().await;
    let logins = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&logins);
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let query = body["query"].as_str().unwrap_or("");
            if query.contains("AuthorizationAuthorize") {
                counter.fetch_add(1, Ordering::SeqCst);
                return ResponseTemplate::new(200).set_body_json(json!({
                    "data": { "tokenWithCredentials": { "token": "fresh-token" } }
                }));
            }
            if let Some(resp) = login_response(&body) {
                return resp;
            }
            let token = req
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok());
            if token == Some("fresh-token") {
                ResponseTemplate::new(200).set_body_json(fake_episodes_payload())
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "errors": [{ "message": "Unauthorized" }]
                }))
            }
        })
        .mount(&server)
        .await;

    let state = AppState::new(make_config(format!("{}/graphql", server.uri())))
        .await
        .unwrap();
    // What `util::token_key` computes for a@b.com / pw.
    let key = hex::encode(Sha256::digest(b"a@b.com~pw"));
    state
        .caches
        .tokens
        .insert(key.clone(), "stale-token".into())
        .await;
    state
        .caches
        .head
        .insert(
            "ep1".to_string(),
            HeadInfo {
                content_length: "9876".into(),
                content_type: "audio/mpeg".into(),
            },
        )
        .await;
    let (addr, handle) = boot_with_state(state.clone()).await;
    let feed = || {
        http_client()
            .get(format!("http://{addr}/feed/{PODCAST_ID}.xml"))
            .header("Authorization", basic_auth("a@b.com,nl,nl-NL", "pw"))
            .send()
    };

    assert_eq!(feed().await.unwrap().status(), 500, "the stale token fails");
    assert_eq!(state.caches.tokens.get(&key).await, None, "and is dropped");
    assert_eq!(
        feed().await.unwrap().status(),
        200,
        "so the next one logs in"
    );
    assert_eq!(logins.load(Ordering::SeqCst), 1);
    handle.abort();
}

#[tokio::test]
async fn local_credentials_log_in_with_the_configured_region_and_locale() {
    let server = MockServer::start().await;
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            if body["query"]
                .as_str()
                .unwrap_or("")
                .contains("AuthorizationPreregisterUser")
            {
                let locale = req.headers.get("user-locale").and_then(|v| v.to_str().ok());
                record.lock().unwrap().push(format!(
                    "{} {}",
                    body["variables"]["countryCode"].as_str().unwrap_or("?"),
                    locale.unwrap_or("?"),
                ));
            }
            login_response(&body).unwrap_or_else(|| {
                ResponseTemplate::new(200).set_body_json(fake_episodes_payload())
            })
        })
        .mount(&server)
        .await;

    let mut config = make_config(format!("{}/graphql", server.uri()));
    config.local_credentials = true;
    config.podimo_email = Some("a@b.com".into());
    config.podimo_password = Some("pw".into());
    config.podimo_region = "de".into();
    config.podimo_locale = "de-DE".into();
    let state = AppState::new(config).await.unwrap();
    state
        .caches
        .head
        .insert(
            "ep1".to_string(),
            HeadInfo {
                content_length: "9876".into(),
                content_type: "audio/mpeg".into(),
            },
        )
        .await;
    let (addr, handle) = boot_with_state(state).await;

    // No `?region=` or `?locale=`: the configured ones apply.
    let resp = http_client()
        .get(format!("http://{addr}/feed/{PODCAST_ID}.xml"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(*seen.lock().unwrap(), ["de de-DE"]);
    handle.abort();
}
