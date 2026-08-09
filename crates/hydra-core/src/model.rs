//! Domain entities — pure data structures (no logic).
//!
//! These mirror the SQLite schema in `docs/design.md` §4.1. Timestamps are
//! stored as ISO-8601 `String` (the core has no `chrono` dependency by design;
//! the server translates to/from `DateTime` at the I/O boundary). `enabled`
//! and `status` use idiomatic Rust types (`bool`/`i32`); the DB-layer
//! mapping (INTEGER 0/1) lives in `hydra-server`.
//!
//! All entities derive `Clone, Debug, PartialEq, Eq, Serialize, Deserialize`
//! so they round-trip through JSON symmetrically (covered by T1.1).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Provider family
// ---------------------------------------------------------------------------

/// An upstream LLM provider. `weight = 0` means soft-disabled (excluded from
/// candidates, see design §7.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    /// Stable provider keyword (globally unique).
    pub key: String,
    pub name: String,
    /// Backend base URL, e.g. `https://api.openai.com`.
    pub endpoint: String,
    /// Effective weight for SWRR. `0` = soft-disabled.
    pub weight: i32,
    pub created_at: String,
    pub updated_at: String,
}

/// A model served by a provider. `status`: `1` online / `0` manually offline /
/// `-1` probe-offline (design §4.2). Only `status == 1` enters the candidate
/// set built into `ConfigData::models_by_key`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModel {
    pub id: String,
    /// Model keyword — the routing key (matched against the request's `model`).
    pub key: String,
    pub name: String,
    pub provider_id: String,
    pub status: i32,
}

/// A real api-key for a provider (stored plaintext by necessity; see §16.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderKey {
    pub id: String,
    pub provider_id: String,
    pub api_key: String,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// Tenant family
// ---------------------------------------------------------------------------

/// A tenant, bound to a domain. `auth_url` is mandatory (NOT NULL): a tenant
/// without it is always rejected (design §11.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tenant {
    pub id: String,
    pub name: String,
    /// Domain (lowercased) that maps to this tenant. `localhost` is allowed.
    pub domain: String,
    /// External auth endpoint — required. Empty/missing ⇒ all requests 401.
    pub auth_url: String,
    pub cert_key: Option<String>,
    pub cert_file: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// Grants a tenant access to a provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantProvider {
    pub id: String,
    pub tenant_id: String,
    pub provider_id: String,
}

/// Access gate: a tenant may only use models listed here (design §7.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantModel {
    pub id: String,
    pub tenant_id: String,
    pub model_key: String,
}

// ---------------------------------------------------------------------------
// Limiting
// ---------------------------------------------------------------------------

/// A rate-limit role. Any `matching_*` of `None` means "match all" on that
/// dimension (design §10.1). `window` ∈ {"m","h","d"}.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitRole {
    pub id: String,
    pub name: String,
    pub matching_key: Option<String>,
    pub matching_model: Option<String>,
    pub matching_tenant: Option<String>,
    pub matching_provider: Option<String>,
    pub limit_count: Option<i64>,
    pub limit_token: Option<i64>,
    pub window: String,
    pub enabled: bool,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// A single weighted candidate produced by `router::resolve`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub provider_id: String,
    /// Resolved host:port + scheme, parsed from `Provider::endpoint`.
    pub endpoint: String,
    pub weight: i32,
}

/// Why routing failed. Maps to HTTP statuses by the proxy shell
/// (design §7.3): `ModelNotAllowed`→403, `ModelNotFound`→404,
/// `TenantForbidden`→403, `NoAvailableProvider`/`NoAvailableKey`→503.
///
/// Defined here (and re-exported from `router`) so the Router lane and the
/// shell share one canonical type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouteError {
    /// `model_key` not in the tenant's `tenant_models` gate.
    ModelNotAllowed,
    /// `model_key` unknown to the system (no online provider serves it).
    ModelNotFound,
    /// Tenant disabled / has no providers configured.
    TenantForbidden,
    /// Intersection of model-providers and tenant-providers empty, or all
    /// filtered out (dead / soft-disabled).
    NoAvailableProvider,
    /// All surviving candidates lack an api-key.
    NoAvailableKey,
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

/// Provider family, driving usage-schema normalisation (design §9.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
    /// Fallback for unknown providers (generic JSON `usage` object).
    Generic,
}

/// Normalised token usage. All fields optional: some providers omit them
/// (e.g. OpenAI without `stream_options.include_usage`).
///
/// `cached_tokens` captures prompt-cache hits: OpenAI exposes it as
/// `usage.prompt_tokens_details.cached_tokens`, Anthropic as
/// `usage.cache_read_input_tokens`. `None` when the provider does not report
/// it (so the dimension is simply absent, not zero).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    /// Prompt-cache hit token count (OpenAI `cached_tokens` /
    /// Anthropic `cache_read_input_tokens`). `None` when unreported.
    pub cached_tokens: Option<u64>,
}

/// A persisted usage record (design §9.1). `created_at` is ISO-8601 text
/// (no `chrono` in core). The api-key is always the masked form (§9.5).
///
/// Latency dimensions:
/// - `latency_ms` — end-to-end wall clock (request start → response complete).
/// - `forward_latency_ms` — Hydra's own overhead: request start → just before
///   the upstream `send` (auth + routing + body read). `None` when no upstream
///   attempt was made.
/// - `ttft_ms` — Time To First Token: request start → first response chunk
///   received from the provider. `None` for non-streamed / errored requests
///   that never produced a chunk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub tenant_id: String,
    pub provider_id: String,
    pub model_key: String,
    pub client_api_key_masked: Option<String>,
    pub status_code: u16,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    /// Prompt-cache hit tokens (mirrors [`Usage::cached_tokens`]).
    pub cached_tokens: Option<u64>,
    pub latency_ms: u64,
    /// Hydra overhead: request start → just before upstream send.
    pub forward_latency_ms: Option<u64>,
    /// Time To First Token: request start → first response chunk.
    pub ttft_ms: Option<u64>,
    pub upstream_host: Option<String>,
    pub error: Option<String>,
    pub trace_id: String,
    pub created_at: String,
}
