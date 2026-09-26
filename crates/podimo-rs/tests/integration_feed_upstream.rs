//! End-to-end feed-flow tests with a mocked Podimo GraphQL upstream.
//!
//! Wiremock stands in for `https://podimo.com/graphql`; the head cache is
//! pre-populated so no episode-media HEAD probe goes out.

use std::net::SocketAddr;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use podimo_rs::cache::HeadInfo;
use podimo_rs::{app, config::Config, AppState};
use serde_json::{json, Value};
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
        store_tokens_on_disk: false,
        token_cache_time: 60,
        podcast_cache_time: 60,
        head_cache_time: 60,
        audiobook_audio_cache_time: 60,
        enable_library: false,
        library_dir: "./library".into(),
        public_feeds: false,
        graphql_url,
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
    let body = resp.text().await.unwrap();
    assert!(body.contains("<rss"));
    assert!(body.contains("<title>Test Show</title>"));
    assert!(body.contains("<title>Episode 1</title>"));
    assert!(body.contains("https://example.com/ep1.mp3"));
    handle.abort();
}

#[tokio::test]
async fn stream_links_follow_the_host_the_feed_was_requested_on() {
    let server = MockServer::start().await;
    let mut payload = fake_episodes_payload();
    let episode = &mut payload["data"]["episodes"][0];
    episode["audio"] = Value::Null;
    episode["streamMedia"] = json!({
        "url": "https://media-cdn-episodes.podimo.com/ep1/ep1.m3u8?u=x&Signature=s",
        "duration": 1234
    });
    install_graphql_mock(&server, ResponseTemplate::new(200).set_body_json(payload)).await;

    let config = make_config(format!("{}/graphql", server.uri()));
    let state = AppState::new(config).await.unwrap();
    let (addr, handle) = boot_with_state(state).await;

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
                // Hand back exactly the per-page-size the caller asked for. With
                // limit=20 the request is for 20 → 20 returned (short page → loop
                // exits naturally). Without a limit (or limit > 100) the per-page
                // size is 100 → 100 returned → pagination would continue.
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
    // Pre-populate head cache so HEAD probes short-circuit for all 20 episode ids.
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

    // limit=20 ≤ PAGE_MAX(100), so exactly one ChannelEpisodesQuery should fire.
    assert_eq!(
        ep_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "limit=20 must trigger exactly 1 ChannelEpisodesQuery, not full pagination"
    );

    let body = resp.text().await.unwrap();
    assert_eq!(body.matches("<item>").count(), 20, "want 20 items in feed");
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
