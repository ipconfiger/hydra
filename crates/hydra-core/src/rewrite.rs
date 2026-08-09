//! Request-path rewrite & api-key masking (pure).
//!
//! Two pure functions over plain data (design §6.5 / §9.5):
//! - [`rewrite_path`] locates the first `/v1` in the request path and rebuilds
//!   an upstream URL against a parsed [`EndpointUrl`].
//! - [`mask_key`] redacts an api-key to `first10 + *** + last4` (never
//!   plaintext — P1-5).
//!
//! Both are allocation-only on the returned `String`; no I/O, no global state.
//! The [`EndpointUrl`] value type is the shared parsed-endpoint form consumed
//! by both the proxy shell and these helpers.

use serde::{Deserialize, Serialize};

/// A parsed upstream endpoint, derived from `Provider::endpoint`.
/// `path_prefix` is the path component of the base URL (empty for a bare host).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointUrl {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path_prefix: String,
}

/// Rewrite a downstream request path onto an upstream endpoint (design §6.5).
///
/// Rules:
/// - Locate the **first** `/v1` in `req_path` and keep everything from there to
///   the end as the *tail* (so `/v1/a/v1/b` keeps the whole thing — the first
///   `/v1` wins).
/// - If there is no `/v1`, the entire `req_path` is the tail (passthrough).
/// - Prepend the endpoint base: `scheme://host[:port]` + `path_prefix`.
/// - The `:port` is omitted when it equals the scheme default (`443` for
///   `https`, `80` for `http`) and shown otherwise — matching how the W2 loader
///   fills `EndpointUrl` from a URL that carries no explicit port.
///
/// Allocation is limited to building the returned `String`.
pub fn rewrite_path(req_path: &str, endpoint: &EndpointUrl) -> String {
    let tail = match memchr::memmem::find(req_path.as_bytes(), b"/v1") {
        Some(idx) => &req_path[idx..],
        None => req_path,
    };

    let mut out = String::with_capacity(
        endpoint.scheme.len()
            + 3
            + endpoint.host.len()
            + endpoint.path_prefix.len()
            + tail.len()
            + 6,
    );
    out.push_str(&endpoint.scheme);
    out.push_str("://");
    out.push_str(&endpoint.host);
    // Only render the port when it is not the scheme default.
    let default_port = match endpoint.scheme.as_str() {
        "https" => Some(443),
        "http" => Some(80),
        _ => None,
    };
    if default_port != Some(endpoint.port) {
        out.push(':');
        out.push_str(&endpoint.port.to_string());
    }
    out.push_str(&endpoint.path_prefix);
    out.push_str(tail);
    out
}

/// Mask an api-key so it is identifiable but never plaintext (design §9.5 /
/// P1-5: the admin API NEVER returns plaintext provider keys).
///
/// Format (operates on `char` boundaries — safe for any valid `&str`):
///
/// | key length `L` | mask |
/// |----------------|------|
/// | `L >= 14` | first 10 chars + `'*'` × `(L − 14)` + last 4 chars |
/// | `6 <= L < 14` | first 2 chars + `'*'` × `(L − 4)` + last 2 chars |
/// | `L < 6` | `'*'` × `L` (fully masked) |
///
/// The three tiers ensure the masked form never reveals enough to reconstruct
/// the original: long keys expose a recognisable prefix + suffix (for
/// identification) but hide the entire middle; short keys expose less to avoid
/// revealing the whole value.
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    let len = chars.len();

    if len >= 14 {
        // first 10 + stars(L-14) + last 4
        let star_count = len - 14;
        let mut out = String::with_capacity(len);
        for &c in &chars[..10] {
            out.push(c);
        }
        for _ in 0..star_count {
            out.push('*');
        }
        for &c in &chars[len - 4..] {
            out.push(c);
        }
        out
    } else if len >= 6 {
        // first 2 + stars(L-4) + last 2
        let star_count = len - 4;
        let mut out = String::with_capacity(len);
        for &c in &chars[..2] {
            out.push(c);
        }
        for _ in 0..star_count {
            out.push('*');
        }
        for &c in &chars[len - 2..] {
            out.push(c);
        }
        out
    } else {
        // L < 6: all stars
        "*".repeat(len)
    }
}
