//! Smooth Weighted Round-Robin — pure single-key transition (Nginx SWRR).
//!
//! This module owns the per-`(tenant, model)` state struct ([`SwrrState`]) and
//! the pure transition function ([`order`]). The outer
//! `DashMap<(String, String), SwrrState>` sharded map and its GC live in
//! `hydra-server` (wave-1 §3.1); core delivers only this single-key state plus
//! the pure transition that mutates it from explicit inputs.
//!
//! ## Purity
//!
//! No I/O, no time, no global state. The caller owns the [`SwrrState`] (keyed
//! per `(tenant, model)` by the server's outer map) and passes it in by
//! `&mut`; `order` performs one SWRR round deterministically.
//!
//! ## Algorithm (design §7.2)
//!
//! For one selection:
//! 1. for each candidate `i`: `current_weight[i] += weight[i]`;
//! 2. `total = Σ weight[i]`;
//! 3. pick the candidate with the **max** `current_weight` (ties broken by the
//!    candidate's position in the slice — first wins, matching Nginx);
//! 4. `current_weight[picked] -= total`;
//! 5. the picked candidate is moved to the front of the slice (the rest keep
//!    their relative order, so failover can walk the tail by cursor).
//!
//! Because `resolve` guarantees `weight > 0`, `total` is always `> 0` here;
//! `order` nonetheless guards `total <= 0` to stay panic-free when called
//! directly with degenerate input.

use std::collections::HashMap;

use crate::model::Candidate;

/// `provider_id` → running `current_weight`. Keyed per `(tenant, model)` by
/// the server's outer map.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SwrrState {
    pub current_weights: HashMap<String, i32>,
}

/// Perform one Nginx Smooth Weighted Round-Robin step.
///
/// Mutates `state` (the per-`(tenant, model)` running weights) and reorders
/// `candidates` so the selected candidate is first, preserving the relative
/// order of the remaining candidates (so a single request's failover can walk
/// the tail by incrementing a cursor — design §7.2 / T3.6).
///
/// The caller owns `state`; it does **not** call `order` again within one
/// request (failover advances the cursor instead). See T3.6.
///
/// No-op on an empty slice; never divides by zero.
pub fn order(candidates: &mut [Candidate], state: &mut SwrrState) {
    let total: i32 = candidates.iter().map(|c| c.weight).sum();
    if total <= 0 {
        // Nothing selectable (resolve always supplies weight > 0; guard for
        // direct callers with degenerate input).
        return;
    }

    // Step 1: advance every candidate's running current_weight by its weight.
    // Providers unseen by the state default to 0.
    for c in candidates.iter() {
        let entry = state
            .current_weights
            .entry(c.provider_id.clone())
            .or_insert(0);
        *entry += c.weight;
    }

    // Steps 2–3: pick the max current_weight, first wins on ties (Nginx).
    let mut picked = 0usize;
    for i in 1..candidates.len() {
        let cw_i = current_weight(state, &candidates[i].provider_id);
        let cw_picked = current_weight(state, &candidates[picked].provider_id);
        if cw_i > cw_picked {
            picked = i;
        }
    }

    // Step 4: subtract total from the picked candidate.
    if let Some(cw) = state
        .current_weights
        .get_mut(&candidates[picked].provider_id)
    {
        *cw -= total;
    }

    // Step 5: move the picked candidate to the front, preserving the relative
    // order of the rest. `[a, b, PICK, c]` → `[PICK, a, b, c]`.
    candidates[..=picked].rotate_right(1);
}

/// Read a provider's running weight, defaulting to 0 when unseen.
fn current_weight(state: &SwrrState, provider_id: &str) -> i32 {
    state.current_weights.get(provider_id).copied().unwrap_or(0)
}
