//! T8.1–T8.6 — request-path rewrite (`/v1` split) & api-key masking.
//!
//! Pure contract (design §6.5 / §9.5):
//! - `rewrite_path(req_path, &EndpointUrl) -> String`
//! - `mask_key(&str) -> String`
//!
//! See `docs/waves/wave-1-pure-core.md` §3.8.

use hydra_core::rewrite::{mask_key, rewrite_path, EndpointUrl};
use pretty_assertions::assert_eq;

/// Build an `EndpointUrl` mirroring how the W2 loader parses a base URL:
/// `port` is the scheme default when the URL carries none.
fn endpoint(scheme: &str, host: &str, port: u16, path_prefix: &str) -> EndpointUrl {
    EndpointUrl {
        scheme: scheme.into(),
        host: host.into(),
        port,
        path_prefix: path_prefix.into(),
    }
}

// --- rewrite_path -----------------------------------------------------------

/// T8.1 — first `/v1` split: everything up to (not including) the first `/v1`
/// is dropped; the endpoint's scheme://host is prepended.
#[test]
fn rewrite_first_v1_split() {
    let ep = endpoint("https", "api.x.com", 443, "");
    let out = rewrite_path("/foo/v1/chat", &ep);
    assert_eq!(out, "https://api.x.com/v1/chat");
    // Also assert the tail slice semantics: only from the first `/v1` onward.
    assert!(out.ends_with("/v1/chat"));
    assert!(!out.contains("/foo"));
}

/// T8.2 — endpoint carrying a path prefix is prepended verbatim before the
/// `/v1` tail.
#[test]
fn rewrite_endpoint_with_prefix() {
    let ep = endpoint("https", "gw.x.com", 443, "/llm");
    let out = rewrite_path("/foo/v1/chat", &ep);
    assert_eq!(out, "https://gw.x.com/llm/v1/chat");
}

/// T8.3 — no `/v1` in the path ⇒ the whole path is appended to the base.
#[test]
fn rewrite_no_v1_passthrough() {
    let ep = endpoint("https", "api.x.com", 443, "");
    assert_eq!(rewrite_path("/foo/bar", &ep), "https://api.x.com/foo/bar");
    // With a prefix too.
    let ep2 = endpoint("https", "gw.x.com", 443, "/llm");
    assert_eq!(
        rewrite_path("/foo/bar", &ep2),
        "https://gw.x.com/llm/foo/bar"
    );
}

/// T8.4 — when several `/v1` segments exist, the FIRST one wins.
#[test]
fn rewrite_multiple_v1_uses_first() {
    let ep = endpoint("https", "x.com", 443, "");
    assert_eq!(rewrite_path("/v1/a/v1/b", &ep), "https://x.com/v1/a/v1/b");
    // First match is not at index 0 here either.
    let ep2 = endpoint("https", "x.com", 443, "");
    assert_eq!(
        rewrite_path("/pre/v1/mid/v1/end", &ep2),
        "https://x.com/v1/mid/v1/end"
    );
}

/// `/v1` at the very end of the path is still a valid split point.
#[test]
fn rewrite_v1_at_tail() {
    let ep = endpoint("https", "api.x.com", 443, "");
    assert_eq!(rewrite_path("/foo/v1", &ep), "https://api.x.com/v1");
}

/// http scheme with its default port (80) is omitted, https default (443) is
/// omitted, and non-default ports are rendered. Mirrors standard URL form.
#[test]
fn rewrite_port_rendering() {
    // https default omitted.
    assert_eq!(
        rewrite_path("/v1/c", &endpoint("https", "a.io", 443, "")),
        "https://a.io/v1/c"
    );
    // http default omitted.
    assert_eq!(
        rewrite_path("/v1/c", &endpoint("http", "a.io", 80, "")),
        "http://a.io/v1/c"
    );
    // Non-default ports are shown.
    assert_eq!(
        rewrite_path("/v1/c", &endpoint("https", "a.io", 8443, "")),
        "https://a.io:8443/v1/c"
    );
    assert_eq!(
        rewrite_path("/v1/c", &endpoint("http", "a.io", 8080, "")),
        "http://a.io:8080/v1/c"
    );
}

// --- mask_key ---------------------------------------------------------------

/// T8.5 — short keys (≤ 8 chars) are fully masked; never panics, never OOB.
#[test]
fn mask_key_short_input() {
    // Boundary lengths around the threshold.
    assert_eq!(mask_key(""), "****");
    assert_eq!(mask_key("abc"), "****");
    assert_eq!(mask_key("12345678"), "****"); // exactly 8 ⇒ fully masked
                                              // The masked form must never echo any of the original input.
    let secret = "sh0rt";
    let masked = mask_key(secret);
    assert_ne!(masked, secret);
    assert!(!masked.contains(secret));
}

/// T8.6 — normal-length keys keep first-4 + ellipsis + last-4.
#[test]
fn mask_key_normal() {
    // Minimal length that keeps the middle hidden: 9 chars.
    assert_eq!(mask_key("123456789"), "1234…6789");
    // Realistic api-key shape.
    assert_eq!(mask_key("sk-secretvalue-wxyz"), "sk-s…wxyz");
    // First-4 / last-4 must come from the actual head/tail.
    let key = "sk-abcdefghijkl";
    let masked = mask_key(key);
    assert_eq!(masked, "sk-a…ijkl");
    assert!(masked.starts_with("sk-a"));
    assert!(masked.ends_with("ijkl"));
    assert!(masked.contains('…'));
}
