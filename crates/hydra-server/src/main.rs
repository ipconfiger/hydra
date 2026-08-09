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
//! 2. On a dedicated **background runtime** (so Pingora can own its own):
//!    open the SQLite pool and run migrations, load [`ConfigStore`], build the
//!    auth checker / usage sink / breaker / limiter, and spawn the long-lived
//!    background tasks (breaker probe, limiter GC). The runtime is **kept
//!    alive** for the process lifetime — the tasks need it.
//! 3. Resolve certs (if any) into the shared `HydraCertStore` (§12.1).
//! 4. Boot Pingora with an `http_proxy_service` — TLS when certs are present,
//!    plain TCP otherwise — plus the admin `ServeHttp` service on its own port.
//!
//! ## Why not `#[tokio::main]`
//!
//! Pingora's [`Server::run_forever`] is **blocking** and builds its own tokio
//! runtime internally. Calling it from inside `#[tokio::main]` (or any nested
//! `block_on`) panics with *"Cannot start a runtime from within a runtime"*.
//! The background runtime here is a **sibling**, not nested: we use it only for
//! the async bootstrap + the long-lived bg tasks, drop out of `block_on`,
//! keep the runtime alive via a binding, and let `run_forever` own the main
//! thread and its own runtime. This is the canonical Pingora binary layout
//! (see the integration tests in `tests/admin_api.rs` which use the same
//! `std::thread::spawn(run_forever)` shape to avoid the nesting).

use std::sync::Arc;

use hydra_core::breaker::BreakerConfig;
use hydra_server::admin::{AdminService, AdminState};
use hydra_server::crypto;
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

fn main() {
    // (1) Tracing.
    let _ = tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    info!("hydra gateway starting (W6: UI + ops hardening)");

    // (2) Background runtime: drives the async bootstrap AND hosts the
    //     long-lived tasks (breaker probe, limiter GC). Kept alive for the
    //     process lifetime via the `_bg_runtime` binding below — see the
    //     module docs for why we don't use #[tokio::main].
    let bg_runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            error!(error = %e, "failed to build background tokio runtime");
            std::process::exit(1);
        }
    };

    let boot = bg_runtime.block_on(bootstrap());
    let components = match boot {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "fatal startup error");
            std::process::exit(1);
        }
    };

    // (3) Run Pingora on the bare main thread. `run_forever` builds its own
    //     runtime; the bg_runtime above is a sibling (kept alive, not nested).
    //     `_bg_runtime` is never dropped because `run_forever` diverges.
    let _bg_runtime = bg_runtime;
    if let Err(e) = run_server(components) {
        error!(error = %e, "fatal pingora startup error");
        std::process::exit(1);
    }
}

/// All async + shared-component construction done on the background runtime
/// (so the resulting `Arc`s are usable both by Pingora's services and by the
/// bg tasks that share them).
async fn bootstrap() -> Result<BootstrapComponents, Box<dyn std::error::Error>> {
    // (2a) DB pool + migrations.
    let db_url = std::env::var("HYDRA_DB_URL").unwrap_or_else(|_| DEFAULT_DB_URL.to_string());
    let pool = db::init_pool(&db_url).await?;
    db::run_migrate(&pool).await?;
    info!(db_url = %db_url, "database pool ready");

    // (2b) Master key for provider-key encryption-at-rest (fail-closed: the
    //      process refuses to start without HYDRA_ENCRYPTION_KEY[_FILE]).
    let static_kp =
        crypto::StaticKeyProvider::from_env().map_err(|e| -> Box<dyn std::error::Error> {
            format!("master key load failed: {e}").into()
        })?;
    info!(
        "provider-key encryption enabled (master key version {})",
        static_kp.version()
    );
    let key_provider: Arc<dyn crypto::KeyProvider> = Arc::new(static_kp);

    // (2c) Config store (initial snapshot).
    let store = ConfigStore::load(pool.clone(), key_provider.clone()).await?;
    info!("config store loaded");

    // (2c) Auth checker.
    let auth_cache = AuthCache::new(
        AuthConfig::default().allow_ttl,
        AuthConfig::default().deny_ttl,
    );
    let auth_config = AuthConfig::default();
    let auth = Arc::new(HttpAuthChecker::new(auth_cache, auth_config)?);
    info!("auth checker initialised");

    // (2d) Usage sink.
    let sink_kind =
        std::env::var("HYDRA_USAGE_SINK").unwrap_or_else(|_| DEFAULT_USAGE_SINK.to_string());
    let ch_url = std::env::var("HYDRA_CLICKHOUSE_URL").ok();
    let sink = build_sink(&sink_kind, Some(pool.clone()), ch_url.as_deref())?;
    let sink: Arc<dyn hydra_server::sink::UsageSink> = Arc::from(sink);
    info!(kind = %sink_kind, "usage sink built");

    // (2e) Build shared app state.
    let proxy_cfg = ProxyConfig::default();
    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(
        proxy_cfg.breaker.threshold,
    )));
    let limiter = Arc::new(RateLimiter::new());
    let admission = hydra_server::proxy::admission::AdmissionControl::new();

    let state = Arc::new(AppState {
        store: store.clone(),
        auth: auth.clone(),
        breaker: breaker.clone(),
        limiter: limiter.clone(),
        admission: admission.clone(),
        sink,
        proxy: proxy_cfg.clone(),
    });

    // (2f) Background tasks (spawned onto this background runtime; they live as
    //      long as the runtime, which is kept alive in `main`).
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

    Ok(BootstrapComponents {
        pool,
        store,
        auth,
        breaker,
        key_provider,
        state,
    })
}

/// The shared components built by [`bootstrap`] and consumed by [`run_server`].
struct BootstrapComponents {
    pool: sqlx::SqlitePool,
    store: ConfigStore,
    auth: Arc<HttpAuthChecker>,
    breaker: Arc<CircuitBreaker>,
    key_provider: Arc<dyn crypto::KeyProvider>,
    state: Arc<AppState>,
}

/// Synchronous Pingora setup: build the proxy + admin services and call
/// [`Server::run_forever`]. Must run on a bare thread (no enclosing tokio
/// runtime) so Pingora can build its own.
fn run_server(c: BootstrapComponents) -> Result<(), Box<dyn std::error::Error>> {
    // (3a) Pingora server.
    let mut server =
        Server::new(Some(Opt::default())).map_err(|e| format!("pingora server init: {e:?}"))?;
    server.bootstrap();

    // Clone the admission controller out of AppState BEFORE c.state is moved
    // into HydraProxy below, so AdminState::new can share the same DashMap.
    let admission = c.state.admission.clone();
    let app = HydraProxy::new(c.state);

    let listen_addr = std::env::var("HYDRA_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.to_string());
    let mut proxy_service = pingora_proxy::http_proxy_service(&server.configuration, app);

    // (3b) Downstream TLS when any tenant has certs (design §12 / W4b); else
    //      plain TCP for the localhost/dev case. The cfg split keeps the binary
    //      buildable without a TLS backend (plain `proxy` feature). Under a TLS
    //      backend the resolved `HydraCertStore` is kept so the admin reload
    //      endpoint can re-resolve certs (W4b contract).
    #[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
    let (tls_enabled, cert_store) = {
        use hydra_server::tls::HydraCertStore;

        let snapshot = c.store.snapshot();
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

    // (3c) Admin service — a second Pingora `Service` (ServeHttp) on its own
    //      plain-TCP port (design §13.1). Same runtime, admin-token-gated.
    //      Also serves the embedded `/admin/*` UI (design §14) without the
    //      token gate so the browser can render the login prompt.
    let admin_token = AdminService::token_from_env();
    let admin_addr =
        std::env::var("HYDRA_ADMIN_ADDR").unwrap_or_else(|_| DEFAULT_ADMIN_LISTEN.to_string());

    // Cert-reload hook for the W4b contract: re-resolve certs from the latest
    // snapshot after every reload. Only meaningful under a TLS backend.
    #[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
    let cert_reloader: Option<Arc<dyn Fn() + Send + Sync>> = cert_store.as_ref().map(|cs| {
        let cs = cs.clone();
        let store = c.store.clone();
        Arc::new(move || {
            let snap = store.snapshot();
            cs.resolve_and_store(&snap.certs);
        }) as Arc<dyn Fn() + Send + Sync>
    });
    #[cfg(not(any(feature = "tls-boringssl", feature = "tls-openssl")))]
    let cert_reloader: Option<Arc<dyn Fn() + Send + Sync>> = None;

    let admin_state = Arc::new(AdminState::new(
        c.pool,
        c.store,
        c.auth,
        c.breaker,
        c.key_provider.clone(),
        admin_token.clone(),
        cert_reloader,
        admission.clone(),
    ));
    let admin_app = AdminService::new(admin_state);
    let mut admin_service =
        pingora_core::services::listening::Service::new("Hydra admin API".to_string(), admin_app);
    admin_service.add_tcp(&admin_addr);
    server.add_service(admin_service);
    if admin_token.is_some() {
        info!(admin = %admin_addr, "admin REST API + UI bound (admin token configured)");
    } else {
        error!(
            admin = %admin_addr,
            "admin REST API + UI bound but HYDRA_ADMIN_TOKEN is unset — all admin requests will be denied (§13.3)"
        );
    }

    server.run_forever();
}
