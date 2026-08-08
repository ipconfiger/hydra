//! Per-request context (`RequestContext`) shared across Pingora hooks.
//!
//! Holds everything `request_filter` decides (tenant, auth verdict, route
//! candidates, selected provider/key) plus the incremental state the body &
//! response filters mutate (`body_buffer`, `upstream_bytes_seen`, the usage
//! scanner). One `RequestContext` is created per request via
//! [`crate::proxy::HydraProxy::new_ctx`] and dropped at request end.

use std::time::Instant;

use bytes::Bytes;
use hydra_core::auth::AuthVerdict;
use hydra_core::model::{Candidate, RouteError, Tenant, Usage};
use hydra_core::sse::UsageScanner;

use crate::proxy::SelectedRoute;

/// Per-request state shared across the Pingora lifecycle hooks (design §6.2).
///
/// Fields are grouped by the phase that writes them:
/// - *request_filter*: `tenant`, `client_api_key`, `auth_verdict`, `model_key`,
///   `passthrough`, `candidates`, `route_error`, `trace_id`, `started_at`.
/// - *upstream_peer*: `selected`.
/// - *request_body_filter*: `body_buffer`, `body_too_large`, `first_chunk`,
///   `first_chunk_injected`, `accumulated_bytes`, `hard_capped`.
/// - *upstream_response_*: `upstream_bytes_seen`, `scanner`, `usage`,
///   `status_code`, `upstream_host`.
/// - *fail_to_connect / error_while_proxy*: `cursor`.
pub struct RequestContext {
    /// Request start time (for latency reporting in `logging`).
    pub started_at: Instant,
    /// Tenant resolved from the `Host` header (None ⇒ 404 short-circuit).
    pub tenant: Option<Tenant>,
    /// The raw client api-key parsed from `Authorization`/`x-api-key`.
    pub client_api_key: Option<String>,
    /// The external-auth verdict (carries the HTTP status to write back).
    pub auth_verdict: Option<AuthVerdict>,
    /// `model_key` extracted from the first request-body chunk via `memchr`
    /// (None ⇒ non-JSON / no-model ⇒ passthrough or reject).
    pub model_key: Option<String>,
    /// True ⇒ skip routing, connect directly to the tenant's first provider
    /// (design §6.3a: `GET /v1/models`, health checks, webhooks).
    pub passthrough: bool,
    /// Ordered candidate set from `router::resolve` + `swrr::order`.
    pub candidates: Vec<Candidate>,
    /// Index into `candidates` for the current attempt (failover advances it).
    pub cursor: usize,
    /// The currently-selected provider + its api-key (written by upstream_peer).
    pub selected: Option<SelectedRoute>,
    /// The stored first body chunk (consumed in `request_filter` for `memchr`
    /// model extraction; re-injected on the first `request_body_filter` call so
    /// it reaches the upstream intact — design §6.3 first-chunk re-forward).
    pub first_chunk: Option<Bytes>,
    /// Whether the stored first chunk has already been re-injected into the
    /// body stream (guards a one-shot prepend in `request_body_filter`).
    pub first_chunk_injected: bool,
    /// Accumulated body chunks for failover replay (`Bytes::clone` = O(1)
    /// refcount bump, zero memcpy). Capped by `max_request_body` (soft) — once
    /// over the cap `body_too_large` is set and accumulation stops (the body
    /// still forwards untouched); replay is then disabled (§8.5).
    pub body_buffer: Vec<Bytes>,
    /// Total body bytes seen so far (drives the soft/hard caps).
    pub accumulated_bytes: u64,
    /// Soft-cap breached: stop accumulating (replay disabled), body still flows.
    pub body_too_large: bool,
    /// Hard-cap breached: `request_filter` already wrote 413; body discarded.
    pub hard_capped: bool,
    /// Upstream response bytes seen so far (gates `error_while_proxy` retry —
    /// once > 0 we never retry to avoid duplicate streams, §8.2/§8.3).
    pub upstream_bytes_seen: u64,
    /// Status code received from the upstream (for the usage record).
    pub status_code: u16,
    /// Upstream host actually contacted (for the usage record).
    pub upstream_host: Option<String>,
    /// Incremental usage scanner (memchr over response chunks; §9.4).
    pub scanner: UsageScanner,
    /// Finalised usage (set in `logging` from `scanner.finalize()`).
    pub usage: Option<Usage>,
    /// Routing failure reason (written when `resolve` errors, surfaced as the
    /// HTTP error response in `fail_to_proxy`).
    pub route_error: Option<RouteError>,
    /// Per-request trace id (echoed back as `X-Hydra-Trace-Id`).
    pub trace_id: String,
}

impl RequestContext {
    /// Build a fresh context for an incoming request.
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            tenant: None,
            client_api_key: None,
            auth_verdict: None,
            model_key: None,
            passthrough: false,
            candidates: Vec::new(),
            cursor: 0,
            selected: None,
            first_chunk: None,
            first_chunk_injected: false,
            body_buffer: Vec::new(),
            accumulated_bytes: 0,
            body_too_large: false,
            hard_capped: false,
            upstream_bytes_seen: 0,
            status_code: 0,
            upstream_host: None,
            // Generic (OpenAI-compatible) schema by default; the shell does not
            // know the provider family until later, and `Generic` falls back to
            // the same `prompt/completion/total` field names.
            scanner: UsageScanner::new(hydra_core::model::ProviderKind::Generic),
            usage: None,
            route_error: None,
            trace_id: crate::proxy::new_trace_id(),
        }
    }

    /// The provider_id currently being attempted (None if no candidate).
    pub fn current_provider_id(&self) -> Option<&str> {
        self.candidates
            .get(self.cursor)
            .map(|c| c.provider_id.as_str())
    }
}

impl Default for RequestContext {
    fn default() -> Self {
        Self::new()
    }
}
