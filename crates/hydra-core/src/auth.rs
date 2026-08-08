//! External-auth verdict & cache types (pure) + api-key hashing helper.
//!
//! ## Ownership split
//!
//! This foundation lane owns only the **shared types** consumed by the
//! request-filter / auth-cache glue:
//! - [`AuthVerdict`] / [`CacheSource`] — the final verdict the proxy writes
//!   (carrying the HTTP status, design §11.6).
//! - [`Verdict`] — the low-level cache hit/miss returned by the pure
//!   `cache_decision` (Auth lane).
//! - [`sha256_hex`] — the real crypto digest used as the `AuthCache` key
//!   (design §11.5); the cache never stores the plaintext api-key.
//!
//! The pure decision functions (`cache_decision`, `apply_upstream`, `decide`)
//! are implemented by the Auth lane via TDD. They take an explicit `now` for
//! deterministic testing — no hidden time. **No function stubs here.**
//!
//! `AuthVerdict` intentionally does **not** derive `Deserialize`: its
//! `reason: &'static str` field cannot be deserialised from JSON by
//! `serde_json` (`'static` borrowing). It is a runtime-only value.

use sha2::{Digest, Sha256};

/// Where an auth decision came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheSource {
    /// Served from the in-memory cache (within TTL).
    Hit,
    /// Freshly obtained by calling the tenant's `auth_url`.
    Miss,
    /// Decided locally without a cache hit (e.g. fail-open allow, or
    /// `no_auth_url` deny).
    Local,
}

/// Low-level cache verdict produced by the pure `cache_decision` function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Cache hit within TTL; carries the cached allow/deny flag.
    Hit(bool),
    /// No usable cache entry (missing or expired) — caller must go upstream.
    Miss,
}

/// Final auth verdict handed to `request_filter`. Carries the exact HTTP
/// status to write back so the shell doesn't re-derive it (design §11.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthVerdict {
    /// Allow the request to proceed.
    Allowed { source: CacheSource },
    /// Deny; `status` is the HTTP code (401 / 503), `reason` a static label.
    Denied {
        status: u16,
        reason: &'static str,
        source: CacheSource,
    },
}

/// SHA-256 digest of `input`, returned as **raw 32 bytes**.
///
/// (The `_hex` suffix in the name is historical; the return type is the raw
/// digest, matching `AuthCacheKey.api_key_hash: [u8; 32]` in design §11.5.)
/// Used to key the auth cache so plaintext api-keys are never resident.
///
/// This is a real, pure computation (sha2) — not a stub.
pub fn sha256_hex(input: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher.finalize().into()
}
