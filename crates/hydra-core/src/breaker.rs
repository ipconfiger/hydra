//! Circuit-breaker abstraction (pure).
//!
//! This foundation lane owns only the read-only view the router depends on.
//! The pure `Breaker` state machine (`on_failure` / `on_success` / dead-set
//! transitions) is implemented by the Breaker lane (T4.x) via TDD; the
//! concurrent shell (`Arc<CircuitBreaker>`, probe task) is assembled in
//! `hydra-server` (W4) — see wave-1 §3.1.
//!
//! Contract for the router: it accepts `&impl BreakerView` and treats any
//! `is_dead(provider_id) == true` provider as filtered out. Probing (real
//! HTTP/TCP) is I/O and stays out of core entirely; the breaker merely exposes
//! its dead-set for the shell's probe task to mutate.

/// Read-only view over a circuit breaker's dead-set.
///
/// Required `Send + Sync` so the router can accept the server's
/// `Arc<CircuitBreaker>` (DashMap-backed) without core depending on dashmap.
pub trait BreakerView: Send + Sync {
    /// Whether `provider_id` is currently considered dead (excluded from
    /// candidates). Called on every routing decision.
    fn is_dead(&self, provider_id: &str) -> bool;
}
