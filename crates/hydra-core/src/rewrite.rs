//! Request-path rewrite & api-key masking (pure).
//!
//! Two pure functions over plain data (design §6.5 / §9.5):
//! - [`rewrite_path`] locates the first `/v1` in the request path and rebuilds
//!   an upstream URL against a parsed [`EndpointUrl`].
//! - [`mask_key`] redacts an api-key to `first4…last4`.
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

/// Mask an api-key to its first 4 + last 4 characters (design §9.5).
///
/// A key of more than 8 characters becomes `first4…last4` (using the
/// ellipsis `…`). Keys of **8 characters or fewer** cannot safely expose a
/// head/tail without revealing the whole value (or overlapping), so they are
/// fully redacted to `****`. This branch also guarantees no out-of-bounds
/// panic on short or empty input.
///
/// Operates on Unicode scalar values (not raw bytes) so any valid `&str` —
/// including non-ASCII — is masked without crossing a char boundary.
pub fn mask_key(key: &str) -> String {
    const MASKED: &str = "****";

    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return MASKED.to_string();
    }

    let last_start = chars.len() - 4;
    let mut out = String::with_capacity(4 + 1 + 4);
    for &c in &chars[..4] {
        out.push(c);
    }
    out.push('…');
    for &c in &chars[last_start..] {
        out.push(c);
    }
    out
}
