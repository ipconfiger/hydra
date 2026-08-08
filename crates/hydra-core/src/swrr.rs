//! Smooth Weighted Round-Robin state (pure).
//!
//! This foundation lane owns the per-`(tenant, model)` state struct. The pure
//! `order` / `pick` functions (Nginx SWRR) are implemented by the SWRR lane
//! (T3.x) via TDD. The outer `DashMap<(String, String), SwrrState>` sharded
//! map and its GC live in `hydra-server` (wave-1 §3.1); core delivers only
//! this single-key state + (later) the pure transition functions that mutate
//! it from explicit inputs.
//!
//! Per design §7.2: each call does `current_weight += weight` for all
//! candidates, picks the max, then subtracts `total_weight` from it.

use std::collections::HashMap;

/// `provider_id` → running `current_weight`. Keyed per `(tenant, model)` by
/// the server's outer map.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SwrrState {
    pub current_weights: HashMap<String, i32>,
}
