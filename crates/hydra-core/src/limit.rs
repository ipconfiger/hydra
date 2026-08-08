//! Access-limit matching context (pure).
//!
//! This foundation lane owns the borrowed matching context. The pure
//! `match_roles(&[LimitRole], &MatchCtx) -> Vec<&LimitRole>` and the
//! `SlidingWindow` (driven by an explicit `now`) are implemented by the Limit
//! lane (T6.x) via TDD. The `DashMap<LimitKey, SlidingWindow>` and GC live in
//! `hydra-server` (wave-1 §3.1).
//!
//! `MatchCtx` borrows request attributes (api-key / model / tenant / provider)
//! with no allocation; `provider` is `None` until routing selects one, so
//! provider-dimension limits are checked in the `logging` phase (design §10.3).

/// Borrowed per-request context used to match `LimitRole`s. All fields
/// optional; `None` means "not yet known" (not "wildcard" — wildcards are
/// expressed as `None` on the *role* side).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchCtx<'a> {
    pub api_key: Option<&'a str>,
    pub model: Option<&'a str>,
    pub tenant: Option<&'a str>,
    pub provider: Option<&'a str>,
}
