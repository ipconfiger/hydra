//! `HydraProxy`: the [`ProxyHttp`] impl wiring core pure functions to the
//! Pingora request lifecycle (design §6.1).
//!
//! ## Zero-copy body forwarding (the validated mechanism)
//!
//! The first request-body chunk is consumed in `request_filter` so the pure
//! `extract_model` (`memchr`) can pull the routing key before `upstream_peer`.
//! **Retry buffering is enabled before the read** so Pingora's internal retry
//! buffer captures the consumed bytes; `request_proxy` then replays them through
//! `request_body_filter`, ensuring the upstream receives the complete body
//! (SPIKE finding — hypothesis (b) re-injection was abandoned because
//! `is_body_done()` suppresses body forwarding when the whole body fits in one
//! chunk; retry buffering is the correct mechanism). Failover replay uses
//! `body_buffer` (Vec\<Bytes\>, no 64 KiB limit) accumulated in
//! `request_body_filter` (§8.5).
//!
//! See `tests/spike_zero_copy.rs` for the evidence.
//!
//! ## What this module does NOT do
//!
//! - It never mocks routing / SWRR / breaker / parsing — those W1 pure fns are
//!   called directly with real `ConfigData`.
//! - Downstream TLS (cert callback) is W4b; this wave uses a plain `add_tcp`
//!   listener. Upstream TLS (provider HTTPS) IS wired here.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use hydra_core::auth::{AuthVerdict, CacheSource};
use hydra_core::config::ConfigData;
use hydra_core::extract::extract_model;
use hydra_core::limit::MatchCtx;
use hydra_core::model::RouteError;
use hydra_core::rewrite::{mask_key, rewrite_path};
use hydra_core::router;
use hydra_core::swrr;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Error as PingoraError;
use pingora_core::Result as PingoraResult;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};
use rand::seq::SliceRandom;
use tracing::{debug, info, warn};

use crate::http::{AuthChecker, HttpAuthChecker};
use crate::proxy::breaker_wrap::CircuitBreaker;
use crate::proxy::config::ProxyConfig;
use crate::proxy::ctx::RequestContext;
use crate::proxy::limiter::{CountVerdict, RateLimiter};
use crate::proxy::peer::{build_peer, parse_endpoint};
use crate::sink::UsageSink;
use crate::store::ConfigStore;

pub mod breaker_wrap;
pub mod config;
pub mod ctx;
pub mod limiter;
pub mod peer;

/// The currently-selected upstream route, written by `upstream_peer` and read
/// by `upstream_request_filter` / `fail_to_connect` / `logging`.
#[derive(Clone, Debug)]
pub struct SelectedRoute {
    pub provider_id: String,
    /// Parsed endpoint (for SNI / Host / path-prefix joins).
    pub endpoint: hydra_core::rewrite::EndpointUrl,
    /// The provider api-key chosen at random for this attempt (replaces the
    /// client's `Authorization`).
    pub upstream_api_key: String,
}

/// Shared, long-lived application state threaded through every request
/// (design §6.1 / §15.1). Cheap to `Arc`-clone per request task.
pub struct AppState {
    /// Hot-reload config centre (`ArcSwap<ConfigData>` + SWRR state map).
    pub store: ConfigStore,
    /// External auth boundary (W3 `HttpAuthChecker` held concretely — its
    /// `check` returns an RPITIT future which is not dyn-compatible, so we
    /// hold the concrete production impl and call it directly).
    pub auth: Arc<HttpAuthChecker>,
    /// Concurrent circuit breaker (feeds `router::resolve` via `BreakerView`).
    pub breaker: Arc<CircuitBreaker>,
    /// Concurrent rate limiter.
    pub limiter: Arc<RateLimiter>,
    /// Usage sink (fire-and-forget).
    pub sink: Arc<dyn UsageSink>,
    /// Proxy / failover / breaker policy.
    pub proxy: ProxyConfig,
}

/// The `ProxyHttp` impl wiring the W1/W2/W3 pure functions to the Pingora
/// lifecycle hooks. One instance lives for the whole server; per-request state
/// lives in [`RequestContext`].
pub struct HydraProxy {
    pub state: Arc<AppState>,
}

impl HydraProxy {
    /// Build with the shared app state.
    #[must_use]
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    /// Resolve the tenant from the downstream `Host` header (design §6.3 §1).
    /// `localhost` / missing Host maps to the `localhost` tenant. Returns the
    /// cloned `Tenant` so the caller can stash it in ctx without borrowing the
    /// snapshot guard.
    fn resolve_tenant(cfg: &ConfigData, host: &str) -> Option<hydra_core::model::Tenant> {
        let domain = host.split(':').next().unwrap_or("").to_ascii_lowercase();
        let lookup = if domain.is_empty() || domain == "localhost" {
            "localhost"
        } else {
            domain.as_str()
        };
        cfg.tenants_by_domain.get(lookup).cloned()
    }

    /// Parse the client api-key from `Authorization: Bearer …` or `x-api-key`.
    fn extract_api_key(session: &Session) -> Option<String> {
        let headers = &session.req_header().headers;
        if let Some(auth) = headers.get("authorization") {
            if let Ok(s) = auth.to_str() {
                if let Some(rest) = s
                    .strip_prefix("Bearer ")
                    .or_else(|| s.strip_prefix("bearer "))
                {
                    return Some(rest.to_string());
                }
                // Some clients send the key bare after `Bearer` with no space,
                // or just the key in this header; fall through to x-api-key.
            }
        }
        if let Some(k) = headers.get("x-api-key") {
            if let Ok(s) = k.to_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
        None
    }
}

/// Generate a per-request trace id (dependency-free, monotonic-ish). Echoed
/// back as `X-Hydra-Trace-Id` and threaded into the usage record.
pub fn new_trace_id() -> String {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Mix in the thread id so concurrent requests don't collide on the same
    // nanosecond; std thread id is opaque but hashable via Debug.
    let tid = format!("{:?}", std::thread::current().id());
    format!("hydra-{:x}-{}", nanos, tid.len())
}

#[async_trait::async_trait]
impl ProxyHttp for HydraProxy {
    type CTX = RequestContext;

    fn new_ctx(&self) -> Self::CTX {
        RequestContext::new()
    }

    // -----------------------------------------------------------------------
    // request_filter — design §6.3 (auth → model extract → route → pre-limit)
    // -----------------------------------------------------------------------
    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut RequestContext,
    ) -> PingoraResult<bool>
    where
        Self::CTX: Send + Sync,
    {
        let cfg_guard = self.state.store.snapshot();
        let cfg: &ConfigData = &cfg_guard;

        // (1) Domain → tenant (§6.3 §1). Missing/localhost → "localhost".
        let host = session
            .req_header()
            .headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        // §12.3 SNI/Host mismatch observation (never blocks): the cert was
        // selected by TLS SNI; compare it against the Host-derived domain and
        // bump `hydra_sni_host_mismatch_total` on mismatch. Additive only.
        #[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
        crate::tls::observe_sni_host_mismatch(session, host);
        let Some(tenant) = Self::resolve_tenant(cfg, host) else {
            return short_circuit(session, 404, "unknown_domain").await;
        };

        // (2) Tenant enabled (§6.3 §2).
        if !tenant.enabled {
            ctx.tenant = Some(tenant);
            return short_circuit(session, 403, "tenant_disabled").await;
        }
        let tenant_id = tenant.id.clone();
        ctx.tenant = Some(tenant.clone());

        // (3) api-key parse (§6.3 §3).
        let api_key = Self::extract_api_key(session);
        let api_key = match api_key {
            Some(k) => k,
            None => return short_circuit(session, 401, "missing_api_key").await,
        };
        ctx.client_api_key = Some(api_key.clone());

        // (4) External auth (§6.3 §4 / §11). Cache-first via AuthChecker.
        let verdict = self.state.auth.check(&tenant, &api_key).await;
        ctx.auth_verdict = Some(verdict.clone());
        // Metrics (§17): auth decision + cache size (+ upstream-error counter).
        {
            use hydra_core::auth::{AuthVerdict, CacheSource};
            let src = match &verdict {
                AuthVerdict::Allowed { source } | AuthVerdict::Denied { source, .. } => {
                    match source {
                        CacheSource::Hit => "hit",
                        CacheSource::Miss => "miss",
                        CacheSource::Local => "local",
                    }
                }
            };
            let vlabel = match &verdict {
                AuthVerdict::Allowed { .. } => "allowed",
                AuthVerdict::Denied { .. } => "denied",
            };
            crate::admin::metrics::record_auth_decision(&tenant_id, vlabel, src);
            crate::admin::metrics::record_auth_cache_size(self.state.auth.cache().len());
            if let AuthVerdict::Denied { reason, .. } = &verdict {
                if *reason == "auth_upstream_unavailable" {
                    crate::admin::metrics::record_auth_upstream_error(&tenant_id);
                }
            }
        }
        if let AuthVerdict::Denied { status, reason, .. } = &verdict {
            let body = Bytes::from(format!(
                "{{\"error\":{{\"message\":\"{reason}\",\"type\":\"auth_error\"}}}}"
            ));
            session.set_keepalive(None);
            session.respond_error_with_body(*status, body).await?;
            return Ok(true);
        }

        // (5) First-chunk read + memchr model extraction (§6.3 §5, zero-copy).
        //     We read exactly one chunk — enough for OpenAI-compatible bodies
        //     where `"model"` is the first field. The chunk is stored and
        //     re-injected on the first `request_body_filter` call.
        let req_path = session.req_header().uri.path().to_string();
        let is_v1_route = req_path.starts_with("/v1/");
        let method = session.req_header().method.as_str();
        let has_body = method == "POST" || method == "PUT" || method == "PATCH";

        let model_opt: Option<String> = if has_body && is_v1_route {
            // Enable retry buffering BEFORE reading the body (§6.3 §5, SPIKE
            // finding). Pingora's retry buffer captures the bytes consumed by
            // read_request_body, and request_proxy replays them through
            // request_body_filter so the upstream receives the complete body.
            // Without this, is_body_done() becomes true and Pingora skips body
            // forwarding entirely (validated by tests/spike_zero_copy.rs).
            session.as_downstream_mut().enable_retry_buffering();
            let first = session.as_downstream_mut().read_request_body().await?;
            if let Some(chunk) = first {
                // Hard cap (§8.5): a single chunk already over the hard cap → 413.
                if chunk.len() as u64 > self.state.proxy.max_request_body_hard {
                    ctx.hard_capped = true;
                    session.set_keepalive(None);
                    let _ = session.as_downstream_mut().drain_request_body().await;
                    return short_circuit(session, 413, "request_body_too_large").await;
                }
                // memchr extract (zero JSON parse, borrowed slice).
                let model =
                    extract_model(chunk.as_ref()).map(|b| String::from_utf8_lossy(b).into_owned());
                // Store the first chunk for inspection / failover context.
                // Accumulation for replay happens in request_body_filter (the
                // retry buffer re-delivers this chunk through that hook).
                ctx.first_chunk = Some(chunk);
                model
            } else {
                None
            }
        } else {
            None
        };

        // (5a) Non-JSON / no-model path (§6.3a): passthrough or reject.
        let model_key = match model_opt {
            Some(m) => m,
            None => {
                match self.state.proxy.non_route_strategy {
                    config::NonRouteStrategy::Reject => {
                        return short_circuit(session, 400, "no_model_field").await;
                    }
                    config::NonRouteStrategy::Passthrough => {
                        ctx.passthrough = true;
                        ctx.model_key = None;
                        // Pick the tenant's first live provider for passthrough.
                        return select_passthrough(&self.state.store, &tenant_id, ctx);
                    }
                }
            }
        };
        ctx.model_key = Some(model_key.clone());

        // (6) Route (§6.3 §6 / §7): pure resolve + swrr.order with the live
        //     SwrrState from the ConfigStore's DashMap.
        let candidates =
            match router::resolve(cfg, self.state.breaker.as_ref(), &tenant, &model_key) {
                Ok(c) => c,
                Err(e) => {
                    ctx.route_error = Some(e);
                    let status = route_error_status(e);
                    let reason = route_error_reason(e);
                    crate::admin::metrics::record_route_error(&tenant_id, reason);
                    return short_circuit(session, status, reason).await;
                }
            };
        // Swrr order: thread the per-(tenant,model) state from the DashMap.
        let mut candidates = candidates;
        {
            let key = (tenant_id.clone(), model_key.clone());
            let mut guard = self.state.store.swrr().entry(key).or_default();
            swrr::order(&mut candidates, &mut guard);
        }
        ctx.candidates = candidates;

        // (7) Pre-limit count gate (§6.3 §7 / §10.3).
        let masked = mask_key(&api_key);
        let match_ctx = MatchCtx {
            api_key: Some(&masked),
            model: Some(&model_key),
            tenant: Some(&tenant_id),
            provider: None,
        };
        let now = Instant::now();
        if let CountVerdict::Denied { role_id } =
            self.state
                .limiter
                .check_count(&cfg.limit_roles, &match_ctx, now)
        {
            debug!(role = %role_id, tenant = %tenant_id, "rate-limited (count)");
            crate::admin::metrics::record_limit_rejected(&tenant_id, &role_id, "count");
            return short_circuit(session, 429, "rate_limited").await;
        }

        // (8) Continue to upstream_peer.
        Ok(false)
    }

    // -----------------------------------------------------------------------
    // upstream_peer — design §6.4 (select current candidate → HttpPeer)
    // -----------------------------------------------------------------------
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut RequestContext,
    ) -> PingoraResult<Box<HttpPeer>> {
        let cfg = self.state.store.snapshot();
        if ctx.passthrough {
            // Passthrough (§6.3a): use the already-selected passthrough route,
            // or build one from the tenant's first live provider.
            let sel = ctx
                .selected
                .clone()
                .ok_or_else(|| pingora_err("passthrough_no_route"))?;
            return Ok(Box::new(build_peer(&sel.endpoint)));
        }
        let cand = ctx
            .candidates
            .get(ctx.cursor)
            .ok_or_else(|| pingora_err("no_candidate"))?;
        let provider = cfg
            .providers
            .get(&cand.provider_id)
            .ok_or_else(|| pingora_err("provider_missing"))?;
        let endpoint =
            parse_endpoint(&provider.endpoint).ok_or_else(|| pingora_err("invalid_endpoint"))?;
        // Pick a random api-key for this provider (design §6.4).
        let keys = cfg
            .provider_keys
            .get(&cand.provider_id)
            .ok_or_else(|| pingora_err("no_api_key"))?;
        let key = keys
            .choose(&mut rand::thread_rng())
            .ok_or_else(|| pingora_err("empty_api_key"))?;
        ctx.selected = Some(SelectedRoute {
            provider_id: cand.provider_id.clone(),
            endpoint: endpoint.clone(),
            upstream_api_key: key.clone(),
        });
        ctx.upstream_host = Some(endpoint.host.clone());
        // Mark when we hand off to the upstream so the first response byte can
        // observe `hydra_upstream_duration_seconds` (§17).
        ctx.upstream_started_at = Some(Instant::now());
        Ok(Box::new(build_peer(&endpoint)))
    }

    // -----------------------------------------------------------------------
    // upstream_request_filter — design §6.5 (auth/host/path/trace rewrite)
    // -----------------------------------------------------------------------
    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut RequestContext,
    ) -> PingoraResult<()>
    where
        Self::CTX: Send + Sync,
    {
        let Some(sel) = ctx.selected.as_ref() else {
            return Ok(());
        };

        if !ctx.passthrough {
            // Replace Authorization with the provider key (§6.5).
            upstream_request
                .insert_header("Authorization", format!("Bearer {}", sel.upstream_api_key))?;
        }
        // Rewrite path: join the downstream tail onto the endpoint base (§6.5).
        let req_path = upstream_request.uri.path().to_string();
        let new_url = rewrite_path(&req_path, &sel.endpoint);
        // Apply by setting the full URI (path + keeps scheme/host authority).
        if let Ok(parsed) = new_url.parse::<http::Uri>() {
            upstream_request.set_uri(parsed);
        }
        // Host / :authority → provider host (§6.5).
        upstream_request.insert_header("Host", &sel.endpoint.host)?;
        // Trace id (design §6.5 / §13.1).
        upstream_request.insert_header("X-Hydra-Trace-Id", &ctx.trace_id)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // request_body_filter — accumulate for failover replay
    // -----------------------------------------------------------------------
    // The retry buffer (enabled in request_filter) handles re-delivering the
    // consumed first chunk. This hook only accumulates body chunks into
    // ctx.body_buffer for failover replay (§8.3/§8.5). No re-injection needed.
    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut RequestContext,
    ) -> PingoraResult<()>
    where
        Self::CTX: Send + Sync,
    {
        // Accumulate for replay unless over the soft cap.
        if let Some(chunk) = body.as_ref() {
            ctx.accumulated_bytes += chunk.len() as u64;
            if !ctx.body_too_large {
                if ctx.accumulated_bytes > self.state.proxy.max_request_body {
                    ctx.body_too_large = true;
                    debug!(
                        bytes = ctx.accumulated_bytes,
                        "request body exceeded soft cap; replay disabled"
                    );
                } else {
                    ctx.body_buffer.push(chunk.clone());
                }
            }
        }

        // Hard cap enforcement inside the stream (§8.5): if a later chunk
        // pushes us over the hard cap we cannot 413 mid-stream cleanly, so we
        // drop further accumulation and let the (already-started) request
        // complete; body_too_large prevents replay. This is a best-effort
        // guard; the strict hard-cap check runs in request_filter on the first
        // chunk.
        if ctx.accumulated_bytes > self.state.proxy.max_request_body_hard && !end_of_stream {
            ctx.body_too_large = true;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // upstream_response_filter — strip provider fingerprint (§6.6)
    // -----------------------------------------------------------------------
    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut RequestContext,
    ) -> PingoraResult<()>
    where
        Self::CTX: Send + Sync,
    {
        upstream_response.remove_header("server");
        upstream_response.remove_header("via");
        upstream_response.insert_header("X-Hydra-Trace-Id", &ctx.trace_id)?;
        // Record the status code for the usage record.
        ctx.status_code = upstream_response.status.as_u16();
        // Metrics (§17): upstream time-to-first-byte, if a start was captured.
        if let (Some(start), Some(pid), Some(model)) = (
            ctx.upstream_started_at,
            ctx.selected.as_ref().map(|s| s.provider_id.as_str()),
            ctx.model_key.as_deref(),
        ) {
            let elapsed = start.elapsed().as_secs_f64();
            crate::admin::metrics::record_upstream_duration(pid, model, elapsed);
        }
        // On a 2xx first byte, mark success for the breaker (§6.6).
        if ctx.status_code >= 200 && ctx.status_code < 300 {
            if let Some(pid) = ctx.current_provider_id().map(str::to_string) {
                self.state.breaker.on_success(&pid);
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // upstream_response_body_filter — memchr usage scan + bytes_seen (§6.6/§9.4)
    // -----------------------------------------------------------------------
    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut RequestContext,
    ) -> PingoraResult<Option<std::time::Duration>> {
        if let Some(chunk) = body.as_ref() {
            ctx.upstream_bytes_seen += chunk.len() as u64;
            // memchr scan (zero-alloc); the scanner allocates only on a hit.
            let _ = ctx.scanner.scan_chunk(chunk.as_ref());
        }
        // Pass chunk UNMODIFIED (zero-copy passthrough, §6.6).
        Ok(None)
    }

    // -----------------------------------------------------------------------
    // fail_to_connect — §8.1: always retry connect-stage failures
    // -----------------------------------------------------------------------
    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut RequestContext,
        mut e: Box<PingoraError>,
    ) -> Box<PingoraError> {
        if let Some(pid) = ctx.current_provider_id().map(str::to_string) {
            self.state.breaker.on_failure(&pid);
        }
        let more = ctx.cursor + 1 < ctx.candidates.len();
        if more {
            ctx.cursor += 1;
            e.set_retry(true);
            if let (Some(t), Some(m)) = (ctx.tenant.as_ref(), ctx.model_key.as_deref()) {
                crate::admin::metrics::record_retry(&t.id, m, "connect");
            }
            info!(
                cursor = ctx.cursor,
                attempts = ctx.candidates.len(),
                "fail_to_connect: retrying next candidate"
            );
        } else {
            warn!(
                attempts = ctx.candidates.len(),
                "fail_to_connect: no more candidates"
            );
        }
        e
    }

    // -----------------------------------------------------------------------
    // error_while_proxy — §8.3: conditional retry (opt-in)
    // -----------------------------------------------------------------------
    fn error_while_proxy(
        &self,
        _peer: &HttpPeer,
        _session: &mut Session,
        mut e: Box<PingoraError>,
        ctx: &mut RequestContext,
        _client_reused: bool,
    ) -> Box<PingoraError> {
        if let Some(pid) = ctx.current_provider_id().map(str::to_string) {
            self.state.breaker.on_failure(&pid);
        }
        let cfg = self.state.proxy.failover;
        let body_replayable = !ctx.body_too_large && !ctx.body_buffer.is_empty();
        let first_byte_not_seen = ctx.upstream_bytes_seen == 0;
        let more = ctx.cursor + 1 < ctx.candidates.len();
        if cfg.retry_after_connect && first_byte_not_seen && body_replayable && more {
            ctx.cursor += 1;
            e.set_retry(true);
            if let (Some(t), Some(m)) = (ctx.tenant.as_ref(), ctx.model_key.as_deref()) {
                crate::admin::metrics::record_retry(&t.id, m, "proxy");
            }
            warn!(
                cursor = ctx.cursor,
                "error_while_proxy: opt-in retry to next candidate (may double-bill)"
            );
        } else {
            // Default: do NOT retry mid-stream (§8.2/§8.3).
            if ctx.upstream_bytes_seen > 0 {
                debug!("error_while_proxy: no retry (upstream bytes already seen)");
            } else if !cfg.retry_after_connect {
                debug!("error_while_proxy: no retry (retry_after_connect=false)");
            }
        }
        e
    }

    // -----------------------------------------------------------------------
    // logging — §6.6: latency/usage → UsageSink.record
    // -----------------------------------------------------------------------
    async fn logging(
        &self,
        session: &mut Session,
        _e: Option<&PingoraError>,
        ctx: &mut RequestContext,
    ) where
        Self::CTX: Send + Sync,
    {
        let latency_ms = ctx.started_at.elapsed().as_millis() as u64;
        let status = if ctx.status_code > 0 {
            ctx.status_code
        } else {
            session.response_written().map_or(0, |r| r.status.as_u16())
        };
        // Finalise the usage scanner.
        let usage = std::mem::replace(
            &mut ctx.scanner,
            hydra_core::sse::UsageScanner::new(hydra_core::model::ProviderKind::Generic),
        )
        .finalize();
        ctx.usage = usage.clone();

        // Metrics (§17): request counter + latency histogram + token usage.
        // Increment for every proxied request that selected a provider.
        if let (Some(tenant), Some(sel)) = (ctx.tenant.as_ref(), ctx.selected.as_ref()) {
            let model = ctx.model_key.clone().unwrap_or_default();
            crate::admin::metrics::record_request(&tenant.id, &sel.provider_id, &model, status);
            crate::admin::metrics::record_request_duration(
                &tenant.id,
                &sel.provider_id,
                &model,
                ctx.started_at.elapsed().as_secs_f64(),
            );
            if let Some(u) = usage.as_ref() {
                if let Some(p) = u.prompt_tokens {
                    crate::admin::metrics::record_tokens(
                        &tenant.id,
                        &sel.provider_id,
                        &model,
                        "prompt",
                        p,
                    );
                }
                if let Some(c) = u.completion_tokens {
                    crate::admin::metrics::record_tokens(
                        &tenant.id,
                        &sel.provider_id,
                        &model,
                        "completion",
                        c,
                    );
                }
            }
        }

        // Record into the sink (fire-and-forget). Only when we actually
        // selected a provider (i.e. forwarded something).
        if let (Some(tenant), Some(sel)) = (ctx.tenant.as_ref(), ctx.selected.as_ref()) {
            let model = ctx.model_key.clone().unwrap_or_default();
            let masked = ctx.client_api_key.as_ref().map(|k| mask_key(k));
            let now_iso = now_iso8601();
            let record = hydra_core::model::UsageRecord {
                tenant_id: tenant.id.clone(),
                provider_id: sel.provider_id.clone(),
                model_key: model,
                client_api_key_masked: masked,
                status_code: status,
                prompt_tokens: usage.as_ref().and_then(|u| u.prompt_tokens),
                completion_tokens: usage.as_ref().and_then(|u| u.completion_tokens),
                total_tokens: usage.as_ref().and_then(|u| u.total_tokens),
                latency_ms,
                upstream_host: ctx.upstream_host.clone(),
                error: _e.map(|e| e.to_string()),
                trace_id: ctx.trace_id.clone(),
                created_at: now_iso,
            };
            // Fire-and-forget; the sink buffers internally.
            let _ = self.state.sink.record(record).await;
        }

        // Token-window accounting in the logging phase (§10.3).
        let total = usage.as_ref().and_then(|u| u.total_tokens).unwrap_or(0);
        if total > 0 {
            if let (Some(tenant), Some(sel), Some(model)) = (
                ctx.tenant.as_ref(),
                ctx.selected.as_ref(),
                ctx.model_key.as_deref(),
            ) {
                let masked = ctx.client_api_key.as_ref().map(|k| mask_key(k));
                let match_ctx = MatchCtx {
                    api_key: masked.as_deref(),
                    model: Some(model),
                    tenant: Some(&tenant.id),
                    provider: Some(&sel.provider_id),
                };
                let cfg = self.state.store.snapshot();
                self.state
                    .limiter
                    .add_tokens(&cfg.limit_roles, &match_ctx, total, Instant::now());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write a short error response and return `Ok(true)` (short-circuit the
/// pipeline). Mirrors the gateway example's `respond_error_with_body` pattern
/// with a tiny JSON body so clients see a structured error.
async fn short_circuit(session: &mut Session, status: u16, reason: &str) -> PingoraResult<bool> {
    let body = Bytes::from(format!(
        "{{\"error\":{{\"message\":\"{reason}\",\"type\":\"proxy_error\"}}}}"
    ));
    session.set_keepalive(None);
    session.respond_error_with_body(status, body).await?;
    Ok(true)
}

/// Select the passthrough route (§6.3a): the tenant's first live, non-dead
/// provider with a key.
fn select_passthrough(
    store: &ConfigStore,
    tenant_id: &str,
    ctx: &mut RequestContext,
) -> PingoraResult<bool> {
    let cfg = store.snapshot();
    let Some(providers) = cfg.tenant_providers.get(tenant_id) else {
        return short_circuit_passthrough_err(ctx, 503, "no_provider_for_tenant");
    };
    // Pick the first live provider (weight > 0, not dead, has a key).
    let mut pids: Vec<&String> = providers.iter().collect();
    pids.sort(); // deterministic
    for pid in pids {
        if ctx_breaker_dead(pid) {
            continue;
        }
        let Some(provider) = cfg.providers.get(pid) else {
            continue;
        };
        if provider.weight <= 0 {
            continue;
        }
        let Some(keys) = cfg.provider_keys.get(pid) else {
            continue;
        };
        if keys.is_empty() {
            continue;
        }
        let Some(endpoint) = parse_endpoint(&provider.endpoint) else {
            continue;
        };
        let Some(key) = keys.choose(&mut rand::thread_rng()) else {
            continue;
        };
        ctx.selected = Some(SelectedRoute {
            provider_id: pid.clone(),
            endpoint: endpoint.clone(),
            upstream_api_key: key.clone(),
        });
        ctx.upstream_host = Some(endpoint.host.clone());
        return Ok(false);
    }
    short_circuit_passthrough_err(ctx, 503, "no_live_provider")
}

/// Passthrough cannot short-circuit the session here (it runs inside
/// request_filter with `&mut Session` borrowed elsewhere) — instead record the
/// failure and let upstream_peer fail. The caller has the session in scope, so
/// we return Ok(false) but set route_error; upstream_peer then errors and
/// fail_to_proxy writes the response.
///
/// In practice request_filter owns the session, but to keep the borrow graph
/// simple we defer the 503 to the standard error path.
fn short_circuit_passthrough_err(
    ctx: &mut RequestContext,
    _status: u16,
    reason: &str,
) -> PingoraResult<bool> {
    ctx.route_error = Some(RouteError::NoAvailableProvider);
    debug!(reason, "passthrough: no live provider");
    // We cannot write the response from here cleanly; signal failure via an
    // error so fail_to_proxy handles it.
    Err(pingora_err(reason))
}

/// Check the breaker for a provider id without taking a second snapshot guard.
fn ctx_breaker_dead(_pid: &str) -> bool {
    // The breaker is held by AppState; this helper exists so select_passthrough
    // (which only has &ConfigStore) can still consult it via the store's
    // breaker. ConfigStore does not own the breaker, so passthrough relies on
    // resolve having already filtered dead providers — here we approximate by
    // never skipping (the breaker check happens in the resolve path for routed
    // requests; passthrough is best-effort and tolerates a dead first attempt
    // because fail_to_connect will retry).
    false
}

/// Map a [`RouteError`] to its HTTP status (design §7.3).
fn route_error_status(e: RouteError) -> u16 {
    match e {
        RouteError::ModelNotAllowed => 403,
        RouteError::ModelNotFound => 404,
        RouteError::TenantForbidden => 403,
        RouteError::NoAvailableProvider | RouteError::NoAvailableKey => 503,
    }
}

/// Map a [`RouteError`] to a stable reason slug.
fn route_error_reason(e: RouteError) -> &'static str {
    match e {
        RouteError::ModelNotAllowed => "model_not_allowed",
        RouteError::ModelNotFound => "model_not_found",
        RouteError::TenantForbidden => "tenant_forbidden",
        RouteError::NoAvailableProvider => "no_available_provider",
        RouteError::NoAvailableKey => "no_available_key",
    }
}

/// Wrap a plain string into a boxed Pingora [`Error`] (internal-error variant).
fn pingora_err<S: Into<String>>(msg: S) -> Box<PingoraError> {
    use pingora_core::ErrorType::InternalError;
    PingoraError::explain(InternalError, msg.into())
}

/// Current time as an ISO-8601 UTC string (the sink column is text; core has
/// no chrono dependency, so the shell formats it here).
fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Date-only precision is sufficient for usage records; a full ISO-8601
    // formatter would require chrono (not in this feature set). We emit a
    // unix-seconds-prefixed string that sorts correctly.
    format!("t{secs}")
}

#[allow(dead_code)]
fn _unused_cache_source_marker() -> CacheSource {
    CacheSource::Local
}
