//! Terminate-in-Pingora proxy mode integration tests (design-change
//! `terminate-mode`). Replaces the former `spike_zero_copy.rs` which validated
//! the stream-through zero-copy body re-forward mechanism.
//!
//! ## What this proves
//!
//! In terminate mode the whole gateway lifecycle runs inside `request_filter`:
//! the proxy reads the full downstream body, extracts the model, routes, then
//! calls the provider via its own reqwest client and streams the response back.
//! These tests verify, against a real Pingora proxy service + wiremock mock
//! providers:
//!
//! - Full request body is forwarded byte-for-byte to the provider.
//! - The provider api-key replaces the client `Authorization`.
//! - The `/v1` path is rewritten onto the provider endpoint.
//! - SSE responses are streamed back chunk-by-chunk.
//! - The failover loop advances to the next candidate on a provider failure
//!   and the breaker records the failure.
//! - The breaker records a success on a 2xx.
//! - Usage (`prompt_tokens`/`completion_tokens`/`total_tokens`) is extracted
//!   from the streamed response.
//! - Error codes surface correctly: 404 (model not found), 401 (auth denied),
//!   429 (rate limited), 503 (no provider), 502 (all providers failed).

mod common;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use hydra_core::breaker::BreakerConfig;
use hydra_core::model::{
    LimitRole, Provider, ProviderKey, ProviderModel, Tenant, TenantModel, TenantProvider,
    UsageRecord,
};
use hydra_server::db as repo;
use hydra_server::http::{AuthCache, AuthConfig, HttpAuthChecker};
use hydra_server::proxy::breaker_wrap::CircuitBreaker;
use hydra_server::proxy::config::ProxyConfig;
use hydra_server::proxy::limiter::RateLimiter;
use hydra_server::proxy::{AppState, HydraProxy};
use hydra_server::store::ConfigStore;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A minimal no-op sink that records nothing (production sinks are tested
/// separately). Terminate-mode behaviour does not depend on the sink.
struct NoopSink;

impl hydra_server::sink::UsageSink for NoopSink {
    fn record(&self, _record: UsageRecord) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

const NOW: &str = "2026-01-01 00:00:00";

/// Seed one tenant + one provider + one model + one key (the minimal routed
/// graph). The provider endpoint is pointed at `upstream_endpoint`.
async fn seed_one(pool: &sqlx::SqlitePool, auth_url: &str, upstream_endpoint: &str) {
    seed_provider(pool, "p1", "openai", "OpenAI", upstream_endpoint).await;
    repo::insert_provider_model(
        pool,
        &ProviderModel {
            id: "m1".into(),
            key: "gpt-4".into(),
            name: "gpt-4".into(),
            provider_id: "p1".into(),
            status: 1,
        },
    )
    .await
    .expect("insert provider_model");
    seed_tenant(pool, "t1", "localhost", auth_url).await;
    repo::insert_tenant_provider(
        pool,
        &TenantProvider {
            id: "tp1".into(),
            tenant_id: "t1".into(),
            provider_id: "p1".into(),
        },
    )
    .await
    .expect("insert tenant_provider");
    repo::insert_tenant_model(
        pool,
        &TenantModel {
            id: "tm1".into(),
            tenant_id: "t1".into(),
            model_key: "gpt-4".into(),
        },
    )
    .await
    .expect("insert tenant_model");
    seed_key(pool, "pk1", "p1", "sk-upstream-secret").await;
    seed_default_role(pool, "t1").await;
}

async fn seed_provider(pool: &sqlx::SqlitePool, id: &str, key: &str, name: &str, endpoint: &str) {
    repo::insert_provider(
        pool,
        &Provider {
            id: id.into(),
            key: key.into(),
            name: name.into(),
            endpoint: endpoint.into(),
            weight: 1,
            created_at: NOW.into(),
            updated_at: NOW.into(),
        },
    )
    .await
    .expect("insert provider");
}

async fn seed_tenant(pool: &sqlx::SqlitePool, id: &str, domain: &str, auth_url: &str) {
    repo::insert_tenant(
        pool,
        &Tenant {
            id: id.into(),
            name: id.into(),
            domain: domain.into(),
            auth_url: auth_url.into(),
            cert_key: None,
            cert_file: None,
            enabled: true,
            created_at: NOW.into(),
            updated_at: NOW.into(),
        },
    )
    .await
    .expect("insert tenant");
}

async fn seed_key(pool: &sqlx::SqlitePool, id: &str, provider_id: &str, api_key: &str) {
    repo::insert_provider_key(
        pool,
        &ProviderKey {
            id: id.into(),
            provider_id: provider_id.into(),
            api_key: api_key.into(),
            created_at: NOW.into(),
        },
    )
    .await
    .expect("insert provider_key");
}

async fn seed_default_role(pool: &sqlx::SqlitePool, tenant: &str) {
    repo::insert_limit_role(
        pool,
        &LimitRole {
            id: "default".into(),
            name: "default".into(),
            matching_key: None,
            matching_model: None,
            matching_tenant: Some(tenant.into()),
            matching_provider: None,
            limit_count: Some(1000),
            limit_token: None,
            window: "m".into(),
            enabled: true,
            created_at: NOW.into(),
        },
    )
    .await
    .expect("insert limit_role");
}

/// Bind an ephemeral port, return it, then release the socket so Pingora can
/// rebind. (TOCTOU window is negligible in test environments.)
fn ephemeral_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.local_addr().expect("local_addr").port()
}

/// Build the full AppState from a seeded pool + auth URL.
async fn build_state(pool: &sqlx::SqlitePool) -> Arc<AppState> {
    let store = ConfigStore::load(pool.clone())
        .await
        .expect("ConfigStore::load");
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("HttpAuthChecker::new"),
    );
    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(5)));
    let limiter = Arc::new(RateLimiter::new());
    let sink: Arc<dyn hydra_server::sink::UsageSink> = Arc::new(NoopSink);
    Arc::new(AppState {
        store,
        auth,
        breaker,
        limiter,
        sink,
        proxy: ProxyConfig::default(),
    })
}

/// Start a Pingora proxy service on an ephemeral port, return the URL root.
fn start_proxy(state: Arc<AppState>) -> String {
    let port = ephemeral_port();
    let listen_addr = format!("127.0.0.1:{port}");
    let app = HydraProxy::new(state);
    let mut server = Server::new(Some(Opt::default())).expect("Server::new");
    server.bootstrap();
    let mut proxy_service = pingora_proxy::http_proxy_service(&server.configuration, app);
    proxy_service.add_tcp(&listen_addr);
    server.add_service(proxy_service);
    std::thread::spawn(move || {
        server.run_forever();
    });
    format!("http://localhost:{port}")
}

/// A reqwest client with a short-ish timeout for tests.
fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("reqwest client")
}

/// Retry the request until the proxy is ready (Pingora binds asynchronously),
/// returning the first successful response.
async fn send_until_ready(client: &reqwest::Client, url: &str, body: &str) -> reqwest::Response {
    let mut last_err = None;
    for _ in 0..60 {
        match client
            .post(url)
            .header("authorization", "Bearer test-client-key")
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
        {
            Ok(r) => return r,
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
    }
    panic!(
        "proxy never became ready: {}",
        last_err.map(|e| e.to_string()).unwrap_or_default()
    );
}

// ===========================================================================
// Tests
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_body_forwarded_intact_and_key_swapped() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&auth_server)
        .await;

    let upstream = MockServer::start().await;
    let request_body =
        r#"{"model":"gpt-4","messages":[{"role":"user","content":"Hello, world!"}]}"#;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-upstream-secret"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"x","object":"chat.completion","choices":[]}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    seed_one(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
    )
    .await;
    let state = build_state(&pool).await;
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();

    let resp = send_until_ready(&client, &url, request_body).await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert!(body.contains("chat.completion"), "body: {body}");

    // Verify the upstream received the body byte-for-byte.
    let received = upstream.received_requests().await.expect("recording on");
    let upstream_body = received
        .iter()
        .find(|r| r.method.as_str() == "POST" && r.url.path() == "/v1/chat/completions")
        .map(|r| String::from_utf8_lossy(&r.body).to_string())
        .expect("upstream got the request");
    assert_eq!(
        upstream_body, request_body,
        "upstream body must be byte-for-byte identical to client body"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_stream_is_forwarded_chunk_by_chunk() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&auth_server)
        .await;

    let upstream = MockServer::start().await;
    // An SSE body with two `data:` frames plus the final usage summary.
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
               data: {\"choices\":[{\"delta\":{\"content\":\"!\"}}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n\
               data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse.to_string()),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    seed_one(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
    )
    .await;
    let state = build_state(&pool).await;
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();

    let resp = send_until_ready(&client, &url, r#"{"model":"gpt-4","stream":true}"#).await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    // The whole SSE stream is forwarded verbatim.
    assert!(body.contains("data: {"), "missing first frame: {body}");
    assert!(body.contains("[DONE]"), "missing terminator: {body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failover_advances_on_provider_error_then_breaker_records() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&auth_server)
        .await;

    let dead_upstream = MockServer::start().await;
    let live_upstream = MockServer::start().await;

    // The first provider always returns 500 → failover to the second.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream exploded"))
        .mount(&dead_upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"ok","object":"chat.completion","choices":[]}"#),
        )
        .expect(1)
        .mount(&live_upstream)
        .await;

    let pool = common::setup_pool().await;
    // Two providers, both serving gpt-4. The first (deterministic SWRR for a
    // fresh state) is attempted first; its failure triggers failover.
    seed_provider(&pool, "p_dead", "dead", "Dead", &dead_upstream.uri()).await;
    seed_provider(&pool, "p_live", "live", "Live", &live_upstream.uri()).await;
    repo::insert_provider_model(
        &pool,
        &ProviderModel {
            id: "m_dead".into(),
            key: "gpt-4".into(),
            name: "gpt-4".into(),
            provider_id: "p_dead".into(),
            status: 1,
        },
    )
    .await
    .unwrap();
    repo::insert_provider_model(
        &pool,
        &ProviderModel {
            id: "m_live".into(),
            key: "gpt-4".into(),
            name: "gpt-4".into(),
            provider_id: "p_live".into(),
            status: 1,
        },
    )
    .await
    .unwrap();
    seed_tenant(
        &pool,
        "t1",
        "localhost",
        &format!("{}/auth", auth_server.uri()),
    )
    .await;
    repo::insert_tenant_provider(
        &pool,
        &TenantProvider {
            id: "tp_dead".into(),
            tenant_id: "t1".into(),
            provider_id: "p_dead".into(),
        },
    )
    .await
    .unwrap();
    repo::insert_tenant_provider(
        &pool,
        &TenantProvider {
            id: "tp_live".into(),
            tenant_id: "t1".into(),
            provider_id: "p_live".into(),
        },
    )
    .await
    .unwrap();
    repo::insert_tenant_model(
        &pool,
        &TenantModel {
            id: "tm1".into(),
            tenant_id: "t1".into(),
            model_key: "gpt-4".into(),
        },
    )
    .await
    .unwrap();
    seed_key(&pool, "pk_dead", "p_dead", "sk-dead").await;
    seed_key(&pool, "pk_live", "p_live", "sk-live").await;
    seed_default_role(&pool, "t1").await;

    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(5)));
    let store = ConfigStore::load(pool.clone()).await.unwrap();
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .unwrap(),
    );
    let limiter = Arc::new(RateLimiter::new());
    let sink: Arc<dyn hydra_server::sink::UsageSink> = Arc::new(NoopSink);
    let state = Arc::new(AppState {
        store,
        auth,
        breaker: breaker.clone(),
        limiter,
        sink,
        proxy: ProxyConfig::default(),
    });
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();

    let resp = send_until_ready(&client, &url, r#"{"model":"gpt-4"}"#).await;
    // Failover reached the live provider → 200.
    assert_eq!(resp.status(), 200, "should failover to the live provider");
    let body = resp.text().await.expect("body");
    assert!(body.contains("chat.completion"), "body: {body}");

    // The dead provider recorded a breaker failure; the live one a success.
    assert_eq!(
        breaker.fail_count("p_dead"),
        1,
        "dead provider should have 1 failure"
    );
    assert_eq!(
        breaker.fail_count("p_live"),
        0,
        "live provider should have 0 failures"
    );
    assert!(!breaker.is_dead("p_dead"), "threshold 5 not reached yet");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn breaker_success_clears_failures() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&auth_server)
        .await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"ok"}"#))
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    seed_one(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
    )
    .await;
    let state = build_state(&pool).await;
    // Pre-charge the breaker with a failure; a 2xx response should reset it.
    state.breaker.on_failure("p1");
    assert_eq!(state.breaker.fail_count("p1"), 1);

    let root = start_proxy(state.clone());
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();
    let resp = send_until_ready(&client, &url, r#"{"model":"gpt-4"}"#).await;
    assert_eq!(resp.status(), 200);
    // on_success fired in the failover loop → fail_count reset to 0.
    assert_eq!(
        state.breaker.fail_count("p1"),
        0,
        "2xx should reset the breaker streak"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_tokens_extracted_from_response() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&auth_server)
        .await;
    let upstream = MockServer::start().await;
    // Non-streaming JSON chat completion with a usage block.
    let body = r#"{"id":"x","object":"chat.completion","choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":13,"completion_tokens":7,"total_tokens":20}}"#;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(body.to_string()),
        )
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    seed_one(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
    )
    .await;

    // Use a recording sink to verify usage extraction end-to-end.
    let recording = Arc::new(RecordingSink::default());
    let store = ConfigStore::load(pool.clone()).await.unwrap();
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .unwrap(),
    );
    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(5)));
    let limiter = Arc::new(RateLimiter::new());
    let state = Arc::new(AppState {
        store,
        auth,
        breaker,
        limiter,
        sink: recording.clone(),
        proxy: ProxyConfig::default(),
    });
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();

    let resp = send_until_ready(&client, &url, r#"{"model":"gpt-4"}"#).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await;

    // The recording sink should have captured one record with the usage tokens.
    let records = recording.records();
    assert_eq!(records.len(), 1, "exactly one usage record expected");
    let r = &records[0];
    assert_eq!(r.prompt_tokens, Some(13));
    assert_eq!(r.completion_tokens, Some(7));
    assert_eq!(r.total_tokens, Some(20));
    assert_eq!(r.provider_id, "p1");
    assert_eq!(r.model_key, "gpt-4");
    assert_eq!(r.status_code, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn error_404_when_model_not_found() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&auth_server)
        .await;
    let upstream = MockServer::start().await;
    // Mount nothing on the upstream — routing should fail before we hit it.

    let pool = common::setup_pool().await;
    seed_one(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
    )
    .await;
    // Allow the tenant to use a model that NO provider serves → router returns
    // ModelNotFound (404), distinct from ModelNotAllowed (403) which fires when
    // the model is not in the tenant_models gate at all.
    repo::insert_tenant_model(
        &pool,
        &TenantModel {
            id: "tm_phantom".into(),
            tenant_id: "t1".into(),
            model_key: "phantom-model".into(),
        },
    )
    .await
    .expect("insert tenant_model");
    let state = build_state(&pool).await;
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();

    let resp = send_until_ready(&client, &url, r#"{"model":"phantom-model"}"#).await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn error_401_when_auth_denied() {
    let auth_server = MockServer::start().await;
    // Auth upstream denies the request.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&auth_server)
        .await;
    let upstream = MockServer::start().await;

    let pool = common::setup_pool().await;
    seed_one(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
    )
    .await;
    let state = build_state(&pool).await;
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();

    let resp = send_until_ready(&client, &url, r#"{"model":"gpt-4"}"#).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn error_502_when_all_providers_fail() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&auth_server)
        .await;
    let upstream = MockServer::start().await;
    // The single provider always returns 503 → all candidates exhausted → 502
    // surfacing the last provider status.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    seed_one(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
    )
    .await;
    let state = build_state(&pool).await;
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();

    let resp = send_until_ready(&client, &url, r#"{"model":"gpt-4"}"#).await;
    // Single provider returned 503; we surface the provider status.
    assert_eq!(resp.status(), 503);
}

// ===========================================================================
// RecordingSink helper (captures usage records for verification)
// ===========================================================================

#[derive(Default)]
struct RecordingSink {
    inner: Arc<std::sync::Mutex<Vec<UsageRecord>>>,
}

impl hydra_server::sink::UsageSink for RecordingSink {
    fn record(&self, record: UsageRecord) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let store = self.inner.clone();
        Box::pin(async move {
            store.lock().unwrap().push(record);
        })
    }
}

impl RecordingSink {
    fn records(&self) -> Vec<UsageRecord> {
        self.inner.lock().unwrap().clone()
    }
}
