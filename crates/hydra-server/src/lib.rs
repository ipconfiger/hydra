//! # hydra-server — I/O shell over [`hydra_core`].
//!
//! Thin adapter layer: Pingora proxy lifecycle, sqlx store (with `ArcSwap`
//! hot config), reqwest auth upstream, usage sinks, multi-tenant TLS.
//! All "internal logic" lives in the pure core; this crate only translates
//! between I/O (sessions / rows / responses) and core types.
//!
//! **Status:** Wave-1 foundation skeleton — empty by design. Waves 2–6 fill in
//! the modules below; the `server` feature (gating pingora/sqlx/reqwest) and
//! the binary target are enabled in W4.
//!
//! Planned modules (per design §3):
//! - `proxy` — `ProxyHttp` impl wiring core fns to Pingora hooks
//! - `store` — sqlx repo + `ConfigStore` (`ArcSwap<ConfigData>`) + loader
//! - `http` — `HttpAuthChecker` (reqwest) + admin `ServeHttp`
//! - `sink` — `SqliteSink` / `ClickHouseSink`
//! - `tls` — `certificate_callback` over the core `certs` map
