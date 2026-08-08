//! Circuit-breaker — pure state machine (consecutive failures → dead-set).
//!
//! This module owns **both** the read-only view the router depends on
//! (`BreakerView`) and the pure state machine (`Breaker`) that transitions it.
//!
//! ## Purity / what lives where
//!
//! `Breaker` is a **pure** state machine: it holds a per-`provider_id`
//! consecutive-failure counter and a dead-set, and it transitions *only* from
//! explicit `on_failure` / `on_success` events. There is no `Instant::now()`,
//! no background thread, no real HTTP/TCP probe in this crate — probing is I/O
//! and is assembled in `hydra-server` (W4). The shell's probe task recovers a
//! provider by calling [`Breaker::on_success`]; until then a dead provider
//! stays dead (the dead-set is additive — see T4.6).
//!
//! ## Why a plain `HashMap`/`HashSet` here
//!
//! The concurrent wrapper (`Arc<CircuitBreaker>` over `DashMap`/`DashSet`) is a
//! *server-shell* concern and is explicitly out of scope for core
//! (wave-1 §3.1). By delivering the pure transition logic here, the shell is a
//! thin lock-free adapter that forwards events to these methods.

use std::collections::{HashMap, HashSet};

/// Read-only view over a circuit breaker's dead-set.
///
/// Required `Send + Sync` so the router can accept the server's
/// `Arc<CircuitBreaker>` (DashMap-backed) without core depending on dashmap.
pub trait BreakerView: Send + Sync {
    /// Whether `provider_id` is currently considered dead (excluded from
    /// candidates). Called on every routing decision.
    fn is_dead(&self, provider_id: &str) -> bool;
}

/// Configuration for the pure breaker state machine.
///
/// Only the failure threshold lives in core. Cooldown / probe-interval are
/// timing concerns owned by the server shell's probe task (design §8.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreakerConfig {
    /// Number of **consecutive** failures after which a provider enters the
    /// dead-set. Reaching exactly `threshold` trips the breaker.
    pub threshold: u32,
}

impl BreakerConfig {
    /// Build a config with the given consecutive-failure threshold.
    pub const fn new(threshold: u32) -> Self {
        Self { threshold }
    }
}

impl Default for BreakerConfig {
    /// Default threshold of 5 (matches design §8.4).
    fn default() -> Self {
        Self { threshold: 5 }
    }
}

/// Pure circuit-breaker state: consecutive-failure counts + the dead-set.
///
/// Transition rules (design §8.4):
/// - [`on_failure`](Self::on_failure): increments the consecutive counter; when
///   it reaches the configured threshold the provider is inserted into the
///   dead-set.
/// - [`on_success`](Self::on_success): clears both the counter and the dead
///   flag for that provider (consecutive semantics — a single success resets
///   the streak, T4.4). This is also the probe-success hook the shell calls.
/// - [`is_dead`](Self::is_dead): membership test on the dead-set.
///
/// These are the read/write interfaces the shell's probe task and request
/// hooks invoke; core itself performs no time-based recovery.
#[derive(Clone, Debug, Default)]
pub struct Breaker {
    /// `provider_id` → consecutive failure count.
    fails: HashMap<String, u32>,
    /// `provider_id` ∈ dead-set ⇒ excluded from routing candidates.
    dead: HashSet<String>,
    /// Consecutive-failure threshold.
    cfg: BreakerConfig,
}

impl Breaker {
    /// Build a breaker from an explicit config.
    pub fn new(cfg: BreakerConfig) -> Self {
        Self {
            fails: HashMap::new(),
            dead: HashSet::new(),
            cfg,
        }
    }

    /// Record a failure for `provider_id`. Once the **consecutive** failure
    /// count reaches the threshold, the provider enters the dead-set.
    pub fn on_failure(&mut self, provider_id: &str) {
        let count = self.fails.entry(provider_id.to_string()).or_insert(0);
        *count += 1;
        if *count >= self.cfg.threshold {
            self.dead.insert(provider_id.to_string());
        }
    }

    /// Record a success for `provider_id`: resets its consecutive-failure
    /// counter to zero and removes it from the dead-set. The shell's probe task
    /// calls this on a successful probe to recover a provider.
    pub fn on_success(&mut self, provider_id: &str) {
        self.fails.remove(provider_id);
        self.dead.remove(provider_id);
    }

    /// Whether `provider_id` is currently dead (excluded from candidates).
    /// Unknown providers are simply not dead.
    pub fn is_dead(&self, provider_id: &str) -> bool {
        self.dead.contains(provider_id)
    }
}

impl BreakerView for Breaker {
    fn is_dead(&self, provider_id: &str) -> bool {
        Breaker::is_dead(self, provider_id)
    }
}
