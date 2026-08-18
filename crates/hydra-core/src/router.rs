//! Router — candidate resolution (pure).
//!
//! This module re-exports the shared routing types ([`Candidate`], [`RouteError`])
//! and implements the pure [`resolve`] function.
//!
//! ## Contract
//! `resolve(&ConfigData, &dyn BreakerView, &Tenant, model_key) ->
//! Result<Vec<Candidate>, RouteError>`
//!
//! ## Purity / time-injection
//! Pure and deterministic: no I/O, no global state, no time. Inputs fully
//! describe the output.
//!
//! ## Pipeline (design §7.1)
//! 0. **TenantModel gate** — *default-open*: a tenant with **no** `tenant_models`
//!    mapping is unrestricted (every model allowed). Once any mapping exists it
//!    becomes a whitelist: `model_key` outside it ⇒
//!    [`RouteError::ModelNotAllowed`].
//! 1. **Online model providers** — providers serving `model_key` (the loader
//!    only indexes `status == 1` rows into `models_by_key`); empty ⇒
//!    [`RouteError::ModelNotFound`].
//! 2. **Tenant providers** — the tenant's authorised provider set (fail-closed:
//!    absent ⇒ [`RouteError::TenantForbidden`]).
//! 3. **Intersection** of (1) and (2); empty ⇒ [`RouteError::NoAvailableProvider`].
//! 4. **Filter** — drop dead (`breaker.is_dead`), keyless (no api-keys), and
//!    soft-disabled (`weight <= 0`); empty ⇒ [`RouteError::NoAvailableProvider`].
//! 5. **Order** — the returned candidates are sorted by `provider_id` for a
//!    deterministic set. SWRR ordering is a *subsequent* step applied by the
//!    caller, which owns the per-`(tenant, model)` [`SwrrState`] (T2.11: only
//!    the set is finalised here).

use std::collections::HashSet;

use crate::breaker::BreakerView;
use crate::config::ConfigData;
use crate::model::Tenant;

pub use crate::model::{Candidate, RouteError};

/// Resolve the candidate set for one `(tenant, model_key)` request.
///
/// See the module docs for the full pipeline. The returned `Vec` is sorted by
/// `provider_id` (deterministic regardless of `HashSet` iteration order); the
/// caller then applies [`crate::swrr::order`] with its own per-`(tenant, model)`
/// state to pick the first attempt and order failover.
pub fn resolve(
    cfg: &ConfigData,
    breaker: &dyn BreakerView,
    tenant: &Tenant,
    model_key: &str,
) -> Result<Vec<Candidate>, RouteError> {
    // (0) TenantModel access gate (design §7.1, revised — default-open): a
    // tenant with NO `tenant_models` mapping is unrestricted (all models
    // allowed); once any mapping exists it is a whitelist — a model outside
    // it is ModelNotAllowed.
    if let Some(allowed) = cfg.tenant_models.get(&tenant.id) {
        if !allowed.contains(model_key) {
            return Err(RouteError::ModelNotAllowed);
        }
    }

    // (1) Providers serving this model (online only — the loader guarantees
    //     `models_by_key` holds `status == 1` rows).
    let by_model: HashSet<String> = cfg
        .models_by_key
        .get(model_key)
        .map(|v| v.iter().map(|m| m.provider_id.clone()).collect())
        .unwrap_or_default();
    if by_model.is_empty() {
        return Err(RouteError::ModelNotFound);
    }

    // (2) Tenant-authorised providers.
    let tenant_ok = cfg
        .tenant_providers
        .get(&tenant.id)
        .ok_or(RouteError::TenantForbidden)?;

    // (3) Intersection.
    let intersection: Vec<String> = by_model.intersection(tenant_ok).cloned().collect();
    if intersection.is_empty() {
        return Err(RouteError::NoAvailableProvider);
    }

    // (4) Filter: not dead, has ≥1 api-key, weight > 0.
    let mut candidates: Vec<Candidate> = intersection
        .into_iter()
        .filter(|pid| !breaker.is_dead(pid))
        .filter(|pid| cfg.provider_keys.get(pid).is_some_and(|k| !k.is_empty()))
        .filter_map(|pid| {
            let p = cfg.providers.get(&pid)?;
            Some(Candidate {
                provider_id: pid,
                endpoint: p.endpoint.clone(),
                weight: p.weight,
            })
        })
        .filter(|c| c.weight > 0)
        .collect();

    if candidates.is_empty() {
        return Err(RouteError::NoAvailableProvider);
    }

    // (5) Deterministic order (set only — SWRR ordering is the caller's step).
    candidates.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
    Ok(candidates)
}
