//! `hydra` — Pingora-based LLM gateway binary (design §6.1 / §15.1).
//!
//! Boots a [`pingora_core::server::Server`] hosting one `http_proxy_service`
//! running [`HydraProxy`]. W4a uses a plain `add_tcp` listener; downstream TLS
//! (multi-tenant SNI cert callback) is W4b (design §12).
//!
//! ## Startup sequence
//!
//! 1. Initialise tracing (`tracing_subscriber`).
//! 2. Open the SQLite pool and run migrations (`db::init_pool` + `run_migrate`).
//! 3. Load [`ConfigStore`] (builds the initial `ConfigData` snapshot).
//! 4. Build [`HttpAuthChecker`] (reqwest + cache).
//! 5. Build [`UsageSink`] via `build_sink`.
//! 6. Construct [`CircuitBreaker`], [`RateLimiter`], and [`AppState`].
//! 7. Spawn background tasks (breaker probe, limiter GC).
//! 8. Boot Pingora with an `http_proxy_service` on the configured address.

use std::sync::Arc;

use hydra_core::breaker::BreakerConfig;
use hydra_server::db;
use hydra_server::http::{AuthCache, AuthConfig, HttpAuthChecker};
use hydra_server::proxy::breaker_wrap::{spawn_probe_task, CircuitBreaker};
use hydra_server::proxy::config::ProxyConfig;
use hydra_server::proxy::limiter::{spawn_gc_task, RateLimiter};
use hydra_server::proxy::{AppState, HydraProxy};
use hydra_server::sink::build_sink;
use hydra_server::store::ConfigStore;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use tracing::{error, info};

const DEFAULT_DB_URL: &str = "sqlite:hydra.db?mode=rwc";
const DEFAULT_LISTEN: &str = "0.0.0.0:8080";
const DEFAULT_USAGE_SINK: &str = "sqlite";

#[tokio::main]
async fn main() {
    // (1) Tracing.
    let _ = tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    info!("hydra gateway starting (W4a: plain TCP listener)");

    if let Err(e) = run().await {
        error!(error = %e, "fatal startup error");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // (2) DB pool + migrations.
    let db_url = std::env::var("HYDRA_DB_URL").unwrap_or_else(|_| DEFAULT_DB_URL.to_string());
    let pool = db::init_pool(&db_url).await?;
    db::run_migrate(&pool).await?;
    info!(db_url = %db_url, "database pool ready");

    // (3) Config store (initial snapshot).
    let store = ConfigStore::load(pool.clone()).await?;
    info!("config store loaded");

    // (4) Auth checker.
    let auth_cache = AuthCache::new(
        AuthConfig::default().allow_ttl,
        AuthConfig::default().deny_ttl,
    );
    let auth_config = AuthConfig::default();
    let auth = Arc::new(HttpAuthChecker::new(auth_cache, auth_config)?);
    info!("auth checker initialised");

    // (5) Usage sink.
    let sink_kind =
        std::env::var("HYDRA_USAGE_SINK").unwrap_or_else(|_| DEFAULT_USAGE_SINK.to_string());
    let sink = build_sink(&sink_kind, Some(pool.clone()), None)?;
    let sink: Arc<dyn hydra_server::sink::UsageSink> = Arc::from(sink);
    info!(kind = %sink_kind, "usage sink built");

    // (6) Build shared app state.
    let proxy_cfg = ProxyConfig::default();
    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(
        proxy_cfg.breaker.threshold,
    )));
    let limiter = Arc::new(RateLimiter::new());

    let state = Arc::new(AppState {
        store: store.clone(),
        auth: auth.clone(),
        breaker: breaker.clone(),
        limiter: limiter.clone(),
        sink,
        proxy: proxy_cfg.clone(),
    });

    // (7) Background tasks.
    let snapshot_provider = {
        let store = store.clone();
        Arc::new(move || {
            let cfg = store.snapshot();
            cfg.providers
                .values()
                .map(|p| (p.id.clone(), p.endpoint.clone()))
                .collect::<Vec<_>>()
        })
    };
    spawn_probe_task(
        breaker.clone(),
        snapshot_provider,
        proxy_cfg.breaker.probe_interval,
    );
    spawn_gc_task(limiter.clone(), std::time::Duration::from_secs(30));

    // (8) Pingora server.
    let mut server =
        Server::new(Some(Opt::default())).map_err(|e| format!("pingora server init: {e:?}"))?;
    server.bootstrap();

    let app = HydraProxy::new(state);

    let listen_addr = std::env::var("HYDRA_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.to_string());
    let mut proxy_service = pingora_proxy::http_proxy_service(&server.configuration, app);
    proxy_service.add_tcp(&listen_addr);
    server.add_service(proxy_service);

    info!(listen = %listen_addr, "proxy listener bound");
    server.run_forever();
}
