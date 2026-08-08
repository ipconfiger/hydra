//! # hydra-server — I/O shell over [`hydra_core`].
//!
//! Thin adapter layer: Pingora proxy lifecycle, sqlx store (with `ArcSwap`
//! hot config), reqwest auth upstream, usage sinks, multi-tenant TLS.
//! All "internal logic" lives in the pure core; this crate only translates
//! between I/O (sessions / rows / responses) and core types.
//!
//! **Status:** Wave-1 foundation skeleton — modules are feature-gated and
//! intentionally empty. Waves 2–6 fill them in.
//!
//! ## Feature model
//!
//! The crate is split into composable Cargo features so each wave compiles
//! only the slice of the I/O shell it needs (see `Cargo.toml` `[features]`):
//!
//! | Feature        | Module(s)        | Wave  | Native dep        |
//! | -------------- | ---------------- | ----- | ----------------- |
//! | `runtime`      | `sink`, `admin`  | W3/W5 | tokio/dashmap/... |
//! | `db`           | `db`, `store`    | W2    | sqlx (sqlite)     |
//! | `http-client`  | `http`           | W3    | reqwest (rustls)  |
//! | `proxy`        | `proxy`, `tls`   | W4    | pingora/BoringSSL |
//! | `server`       | (umbrella)       | W4+   | all of the above  |
//! | `usage-clickhouse` | (within sink) | W3  | clickhouse (opt)  |
//!
//! With no features the crate is an empty lib; `db,http-client` builds sqlx +
//! reqwest/rustls **without** pingora/BoringSSL, letting W2/W3 run natively
//! on macOS.
#![forbid(unsafe_code)]

// --- W2: persistence & config store ---------------------------------------
/// sqlx pool, migrations, and the repo layer.
#[cfg(feature = "db")]
pub mod db;
/// `ConfigStore` — `ArcSwap<ConfigData>` hot-reload shell over the DB.
#[cfg(feature = "db")]
pub mod store;

// --- W3: external boundaries ----------------------------------------------
/// `HttpAuthChecker` (reqwest) + admin `ServeHttp` HTTP helpers.
#[cfg(feature = "http-client")]
pub mod http;
/// `UsageSink` trait + `SqliteSink` / `ClickHouseSink` adapters.
#[cfg(feature = "runtime")]
pub mod sink;

// --- W4: Pingora proxy shell ----------------------------------------------
/// `ProxyHttp` impl wiring core fns to Pingora hooks.
#[cfg(feature = "proxy")]
pub mod proxy;
/// `HydraCertStore` — multi-tenant dynamic SNI certificate callback (design
/// §12). Only compiled when a TLS backend (`tls-boringssl` / `tls-openssl`) is
/// enabled: it uses the pingora `x509`/`pkey`/`ssl`/`ext` types that exist only
/// under a real backend (plain `proxy` links the `noop_tls` stub instead).
#[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
pub mod tls;

// --- W5: admin service & observability ------------------------------------
/// `ServeHttp` admin REST API + self-hosted metrics.
#[cfg(feature = "runtime")]
pub mod admin;
