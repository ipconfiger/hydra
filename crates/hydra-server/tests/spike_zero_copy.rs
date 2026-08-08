//! **SPIKE** — validate the zero-copy body re-forward mechanism (design §6.3
//! hypothesis (b)).
//!
//! ## What this proves
//!
//! When `request_filter` consumes the first body chunk via
//! `read_request_body()`, Pingora's auto-forward still delivers the *remaining*
//! chunks, and the consumed first chunk is re-injected on the first
//! `request_body_filter` call by prepending it. The upstream receives the
//! complete, byte-identical body.
//!
//! ## How
//!
//! 1. Start **wiremock** as the upstream LLM provider (records the exact body).
//! 2. Start **wiremock** as the auth upstream (returns 200).
//! 3. Seed an in-memory SQLite with a tenant / provider / model pointing at
//!    both.
//! 4. Build a full `HydraProxy` with real `ConfigStore`, `HttpAuthChecker`,
//!    `CircuitBreaker`, `RateLimiter`.
//! 5. Start a real Pingora proxy service on an ephemeral port.
//! 6. Send a real HTTP POST with a JSON body via `reqwest`.
//! 7. Assert the upstream received the body byte-for-byte identical.

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
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A minimal no-op sink for the spike (production sinks are tested separately).
struct NoopSink;

impl hydra_server::sink::UsageSink for NoopSink {
    fn record(&self, _record: UsageRecord) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

const NOW: &str = "2026-01-01 00:00:00";

/// Seed the test DB with one tenant, one provider, one model, one key.
async fn seed(pool: &sqlx::SqlitePool, auth_url: &str, upstream_endpoint: &str) {
    repo::insert_provider(
        pool,
        &Provider {
            id: "p1".into(),
            key: "openai".into(),
            name: "OpenAI".into(),
            endpoint: upstream_endpoint.into(),
            weight: 1,
            created_at: NOW.into(),
            updated_at: NOW.into(),
        },
    )
    .await
    .expect("insert provider");

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

    repo::insert_tenant(
        pool,
        &Tenant {
            id: "t1".into(),
            name: "Test".into(),
            domain: "localhost".into(),
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

    repo::insert_provider_key(
        pool,
        &ProviderKey {
            id: "pk1".into(),
            provider_id: "p1".into(),
            api_key: "sk-test-upstream-key".into(),
            created_at: NOW.into(),
        },
    )
    .await
    .expect("insert provider_key");

    // Insert a default limit role (required by the config loader).
    repo::insert_limit_role(
        pool,
        &LimitRole {
            id: "default".into(),
            name: "default".into(),
            matching_key: None,
            matching_model: None,
            matching_tenant: Some("t1".into()),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spike_zero_copy_body_forwarded_intact() {
    // -----------------------------------------------------------------------
    // (1) Start wiremock upstreams.
    // -----------------------------------------------------------------------
    let auth_server = MockServer::start().await;
    let upstream_server = MockServer::start().await;

    // Auth: always 200 (allowed).
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&auth_server)
        .await;

    // The request body the client will send.
    let request_body =
        r#"{"model":"gpt-4","messages":[{"role":"user","content":"Hello, world!"}]}"#;

    // Upstream: return a minimal chat completion response.
    // We verify the received body manually after the request completes.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                r#"{"id":"chatcmpl-test","object":"chat.completion","choices":[]"#,
            ),
        )
        .expect(1)
        .mount(&upstream_server)
        .await;

    // -----------------------------------------------------------------------
    // (2) Seed config + build app state.
    // -----------------------------------------------------------------------
    let pool = common::setup_pool().await;
    seed(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream_server.uri(),
    )
    .await;

    let store = ConfigStore::load(pool).await.expect("ConfigStore::load");

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

    let state = Arc::new(AppState {
        store,
        auth,
        breaker,
        limiter,
        sink,
        proxy: ProxyConfig::default(),
    });

    // -----------------------------------------------------------------------
    // (3) Start Pingora proxy on an ephemeral port.
    // -----------------------------------------------------------------------
    let port = ephemeral_port();
    let listen_addr = format!("127.0.0.1:{port}");

    let app = HydraProxy::new(state);

    let mut server = Server::new(Some(Opt::default())).expect("Server::new");
    server.bootstrap();
    let mut proxy_service = pingora_proxy::http_proxy_service(&server.configuration, app);
    proxy_service.add_tcp(&listen_addr);
    server.add_service(proxy_service);

    // Run the server in a background thread (run_forever blocks).
    let _server_handle = std::thread::spawn(move || {
        server.run_forever();
    });

    // -----------------------------------------------------------------------
    // (4) Send the request through the proxy.
    // -----------------------------------------------------------------------
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest client");

    let proxy_url = format!("http://localhost:{port}/v1/chat/completions");

    // Retry connection until the proxy is ready (Pingora binds asynchronously
    // after the thread starts; allow up to 10 s).
    let resp = {
        let mut last_err = None;
        let mut ok_resp = None;
        for _ in 0..50 {
            match client
                .post(&proxy_url)
                .header("authorization", "Bearer test-client-key")
                .header("content-type", "application/json")
                .body(request_body)
                .send()
                .await
            {
                Ok(r) => {
                    ok_resp = Some(r);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
        ok_resp.unwrap_or_else(|| {
            panic!(
                "proxy never became ready: {}",
                last_err.map(|e| e.to_string()).unwrap_or_default()
            )
        })
    };

    assert_eq!(
        resp.status(),
        200,
        "proxy should return 200 from the upstream"
    );

    let resp_body = resp.text().await.expect("read response body");
    assert!(
        resp_body.contains("chat.completion"),
        "response body should be the upstream chat completion: {resp_body}"
    );

    // Inspect what the upstream actually received.
    let received = upstream_server
        .received_requests()
        .await
        .expect("recording should be enabled");
    assert!(
        !received.is_empty(),
        "upstream must have received at least one request"
    );

    // Find the POST /v1/chat/completions request body.
    let upstream_body = received
        .iter()
        .find(|r| r.method.as_str() == "POST" && r.url.path() == "/v1/chat/completions")
        .map(|r| String::from_utf8_lossy(&r.body).to_string())
        .expect("upstream must have received the chat request");

    assert_eq!(
        upstream_body, request_body,
        "SPIKE RESULT: upstream body must be byte-for-byte identical to client body"
    );

    tracing::info!("SPIKE PASSED: body forwarded byte-for-byte through Pingora proxy");
}
