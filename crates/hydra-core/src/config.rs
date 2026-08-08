//! In-memory configuration model + load-time validation (pure).
//!
//! `ConfigData` is the hot-read aggregate built by the loader (W2) and held
//! behind an `ArcSwap` on the *server* side (`ConfigStore`). Here in core it is
//! just plain data: `Clone`-able, buildable by hand in tests (T1.2), indexable
//! by the pure `router::resolve`.
//!
//! ## Concurrency boundary
//!
//! Per `docs/waves/wave-1-pure-core.md` §3.1, the concurrency wrappers
//! (`ArcSwap`, `DashMap`, `Arc<CircuitBreaker>`) do **not** live in core. In
//! particular `certs` is a plain `HashMap<String, CertMeta>` here; the server
//! wraps the whole `ConfigData` (and, for independent cert hot-reload per
//! design §5.2/§12.1, the certs map specifically) in `ArcSwap` at the boundary.
//! This keeps `arc-swap` out of core's dependency firewall (dev-plan §2).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::model::{LimitRole, Provider, Tenant};

/// In-memory configuration snapshot. All indexes are built once at load time
/// and read lock-free thereafter (the server holds it inside `ArcSwap`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConfigData {
    /// `domain` (lowercase) → tenant (incl. the `localhost` special case).
    pub tenants_by_domain: HashMap<String, Tenant>,

    /// `model_key` → online providers serving it (`provider_id` + weight).
    /// Only `provider_model.status == 1` entries are included by the loader.
    pub models_by_key: HashMap<String, Vec<ModelProvider>>,

    /// `tenant_id` → set of allowed `provider_id`s.
    pub tenant_providers: HashMap<String, HashSet<String>>,

    /// `tenant_id` → set of allowed `model_key`s (the access gate, §7.1).
    pub tenant_models: HashMap<String, HashSet<String>>,

    /// `provider_id` → provider (incl. endpoint / weight).
    pub providers: HashMap<String, Provider>,

    /// `provider_id` → non-empty list of api-keys (runtime picks one at random).
    pub provider_keys: HashMap<String, Vec<String>>,

    /// Enabled limit roles (priority order decided by the loader).
    pub limit_roles: Vec<LimitRole>,

    /// `domain` → certificate metadata. Plain value here (see module docs);
    /// W1–W2 carries `CertMeta`, W4 resolves to a parsed `ResolvedCert` on the
    /// server side while this map remains the single source of truth.
    pub certs: HashMap<String, CertMeta>,
}

/// One row of `models_by_key`: which provider serves a model and at what weight.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProvider {
    pub provider_id: String,
    pub weight: i32,
}

/// W1–W2 certificate placeholder (paths only). W4 resolves these into a
/// parsed certificate; until then the path fields stand in for validation
/// (e.g. T9.7 "missing cert path" → fatal issue).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertMeta {
    pub domain: String,
    pub cert_file: Option<String>,
    pub cert_key: Option<String>,
}
