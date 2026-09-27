//! End-to-end audiobook-flow tests with a mocked Podimo GraphQL upstream.

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

/// Mock dispatcher: routes by the GraphQL operation name in the request body.
/// Login flow + audiobook queries + an optional override audiobook payload.
async fn install_mock(server: &MockServer, audiobook: ResponseTemplate, audio: ResponseTemplate) {
    let audiobook = std::sync::Arc::new(std::sync::Mutex::new(audiobook));
    let audio = std::sync::Arc::new(std::sync::Mutex::new(audio));
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
            } else if query.contains("AudiobookResultsQuery") {
                audiobook.lock().unwrap().clone()
            } else if query.contains("ShortLivedAudiobookMediaUrlQuery") {
                audio.lock().unwrap().clone()
            } else {
                ResponseTemplate::new(500).set_body_string("unexpected graphql query")
            }
        })
        .mount(server)
        .await;
}

fn fake_audiobook_payload() -> Value {
    json!({
        "data": {
            "audiobookById": {
                "id": "fefa939e-c84d-4c16-8bbf-9575e1379d81",
                "title": "Test Audiobook",
                "authorNames": "Auteur A",
                "description": "Een test boek.",
                "duration": 36000,
                "publisherName": "Uitgeverij Test",
                "yearOfBookPublication": 2024,
                "authors": [{"name": "Auteur A"}],
                "narrators": [{"name": "Verteller B"}],
                "coverImage": {"url": "https://example.com/cover.jpg"},
                "language": {"isoLanguage": "nl"}
            }
        }
    })
}

fn fake_audio_url_payload() -> Value {
    json!({
        "data": {
            "audiobookAudioById": {
                "url": "https://example.com/audiobook.mp3"
            }
        }
    })
}

const AUDIOBOOK_ID: &str = "fefa939e-c84d-4c16-8bbf-9575e1379d81";

#[tokio::test]
async fn audiobook_happy_path_returns_rss() {
    let server = MockServer::start().await;
    install_mock(
        &server,
        ResponseTemplate::new(200).set_body_json(fake_audiobook_payload()),
        ResponseTemplate::new(200).set_body_json(fake_audio_url_payload()),
    )
    .await;

    let config = make_config(format!("{}/graphql", server.uri()));
    let state = AppState::new(config).await.unwrap();
    // Pre-populate the head cache so url_head_info short-circuits offline.
    state
        .caches
        .head
        .insert(
            format!("audiobook__{AUDIOBOOK_ID}"),
            HeadInfo {
                content_length: "123456".into(),
                content_type: "audio/mpeg".into(),
            },
        )
        .await;

    let (addr, handle) = boot_with_state(state).await;
    let resp = http_client()
        .get(format!("http://{addr}/audiobook/{AUDIOBOOK_ID}.xml"))
        .header("Authorization", basic_auth("a@b.com,nl,nl-NL", "pw"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-type").unwrap(), "text/xml");

    let body = resp.text().await.unwrap();
    assert!(body.contains("<rss"));
    assert!(body.contains("<title>Test Audiobook</title>"));
    // Single item only — audiobook = one RSS entry.
    assert_eq!(body.matches("<item>").count(), 1, "want 1 <item>: {body}");
    assert!(body.contains("https://example.com/audiobook.mp3"));
    assert!(body.contains("<itunes:duration>36000</itunes:duration>"));
    assert!(body.contains("Verteld door: Verteller B"));
    assert!(body.contains("Uitgever: Uitgeverij Test"));
    assert!(body.contains("https://example.com/cover.jpg"));
    // GUID = audiobook id, not the audio URL.
    assert!(body.contains(AUDIOBOOK_ID));
    handle.abort();
}

#[tokio::test]
async fn audiobook_not_found_returns_404() {
    let server = MockServer::start().await;
    install_mock(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "data": { "audiobookById": null }
        })),
        ResponseTemplate::new(200).set_body_json(fake_audio_url_payload()),
    )
    .await;

    let config = make_config(format!("{}/graphql", server.uri()));
    let state = AppState::new(config).await.unwrap();
    let (addr, handle) = boot_with_state(state).await;
    let resp = http_client()
        .get(format!("http://{addr}/audiobook/{AUDIOBOOK_ID}.xml"))
        .header("Authorization", basic_auth("a@b.com,nl,nl-NL", "pw"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    handle.abort();
}

#[tokio::test]
async fn audiobook_audio_query_failure_returns_500() {
    // Metadata succeeds but the short-lived audio URL query returns nothing
    // usable — the handler must surface that as 500, not a half-rendered RSS.
    let server = MockServer::start().await;
    install_mock(
        &server,
        ResponseTemplate::new(200).set_body_json(fake_audiobook_payload()),
        ResponseTemplate::new(200).set_body_json(json!({
            "data": { "audiobookAudioById": null }
        })),
    )
    .await;

    let config = make_config(format!("{}/graphql", server.uri()));
    let state = AppState::new(config).await.unwrap();
    let (addr, handle) = boot_with_state(state).await;
    let resp = http_client()
        .get(format!("http://{addr}/audiobook/{AUDIOBOOK_ID}.xml"))
        .header("Authorization", basic_auth("a@b.com,nl,nl-NL", "pw"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 500);
    handle.abort();
}
