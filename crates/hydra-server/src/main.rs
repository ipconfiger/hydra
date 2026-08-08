//! `hydra` — Pingora-based LLM gateway binary (design §6.1 / §15.1).
//!
//! Boots a [`pingora_core::server::Server`] hosting one `http_proxy_service`
//! running [`HydraProxy`]. The listener is downstream TLS (per-tenant SNI cert
//! callback, design §12 / W4b) whenever any tenant has certs configured; a
//! plain `add_tcp` listener is used for the localhost/dev case (no certs).
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
//! 8. Resolve certs (if any) into the shared `HydraCertStore` (§12.1).
//! 9. Boot Pingora with an `http_proxy_service` — TLS when certs are present,
//!    plain TCP otherwise.

use std::sync::Arc;

use hydra_core::breaker::BreakerConfig;
use hydra_server::admin::{AdminService, AdminState};
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
const DEFAULT_ADMIN_LISTEN: &str = "127.0.0.1:8081";
const DEFAULT_USAGE_SINK: &str = "sqlite";

#[tokio::main]
async fn main() {
    // (1) Tracing.
    let _ = tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    info!("hydra gateway starting (W4b: downstream TLS listener)");

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

    // (8a) Downstream TLS when any tenant has certs (design §12 / W4b); else
    //      plain TCP for the localhost/dev case. The cfg split keeps the binary
    //      buildable without a TLS backend (plain `proxy` feature). Under a TLS
    //      backend the resolved `HydraCertStore` is kept so the admin reload
    //      endpoint can re-resolve certs (W4b contract).
    #[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
    let (tls_enabled, cert_store) = {
        use hydra_server::tls::HydraCertStore;

        let snapshot = store.snapshot();
        if snapshot.certs.is_empty() {
            proxy_service.add_tcp(&listen_addr);
            (false, None::<HydraCertStore>)
        } else {
            // Resolve CertMeta → parsed certs into the shared ArcSwap (§12.1
            // single source). The box inside TlsSettings shares this same
            // ArcSwap, so hot-reload only needs another `resolve_and_store`
            // after `reload_all`.
            let cert_store = HydraCertStore::new(None);
            cert_store.resolve_and_store(&snapshot.certs);
            match cert_store.build_tls_settings() {
                Ok(settings) => {
                    proxy_service.add_tls_with_settings(&listen_addr, None, settings);
                    (true, Some(cert_store))
                }
                Err(e) => {
                    error!(error = %e, "failed to build TLS settings; falling back to plain TCP");
                    proxy_service.add_tcp(&listen_addr);
                    (false, None)
                }
            }
        }
    };
    #[cfg(not(any(feature = "tls-boringssl", feature = "tls-openssl")))]
    let tls_enabled = {
        proxy_service.add_tcp(&listen_addr);
        false
    };

    server.add_service(proxy_service);

    if tls_enabled {
        info!(listen = %listen_addr, "proxy TLS listener bound (per-tenant SNI cert callback)");
    } else {
        info!(listen = %listen_addr, "proxy plain-TCP listener bound (no tenant certs configured)");
    }

    // (9) Admin service — a second Pingora `Service` (ServeHttp) on its own
    //     plain-TCP port (design §13.1). Same runtime, admin-token-gated.
    let admin_token = AdminService::token_from_env();
    let admin_addr =
        std::env::var("HYDRA_ADMIN_ADDR").unwrap_or_else(|_| DEFAULT_ADMIN_LISTEN.to_string());

    // Cert-reload hook for the W4b contract: re-resolve certs from the latest
    // snapshot after every reload. Only meaningful under a TLS backend.
    #[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
    let cert_reloader: Option<Arc<dyn Fn() + Send + Sync>> = cert_store.as_ref().map(|cs| {
        let cs = cs.clone();
        let store = store.clone();
        Arc::new(move || {
            let snap = store.snapshot();
            cs.resolve_and_store(&snap.certs);
        }) as Arc<dyn Fn() + Send + Sync>
    });
    #[cfg(not(any(feature = "tls-boringssl", feature = "tls-openssl")))]
    let cert_reloader: Option<Arc<dyn Fn() + Send + Sync>> = None;

    let admin_state = Arc::new(AdminState::new(
        pool.clone(),
        store.clone(),
        auth.clone(),
        breaker.clone(),
        admin_token.clone(),
        cert_reloader,
    ));
    let admin_app = AdminService::new(admin_state);
    let mut admin_service =
        pingora_core::services::listening::Service::new("Hydra admin API".to_string(), admin_app);
    admin_service.add_tcp(&admin_addr);
    server.add_service(admin_service);
    if admin_token.is_some() {
        info!(admin = %admin_addr, "admin REST API bound (admin token configured)");
    } else {
        error!(
            admin = %admin_addr,
            "admin REST API bound but HYDRA_ADMIN_TOKEN is unset — all admin requests will be denied (§13.3)"
        );
    }

    server.run_forever();
}
