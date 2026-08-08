//! Upstream peer construction + endpoint URL parsing (wave-4 §2.1 T1.1–T1.3).
//!
//! `parse_endpoint` turns a `Provider::endpoint` string
//! (`https://api.openai.com`, `http://up:8080`, `https://x.com:8443`) into the
//! shared [`EndpointUrl`] form used by both [`crate::proxy`] (for `HttpPeer`
//! construction + SNI/Host) and `hydra_core::rewrite::rewrite_path` (for the
//! upstream path). `build_peer` turns an `EndpointUrl` into a Pingora
//! [`HttpPeer`] with the correct TLS flag + SNI (design §6.4).

use pingora_core::upstreams::peer::HttpPeer;

// Re-export the shared parsed-endpoint type so callers depend on the core
// definition (and `rewrite_path`) without a second copy.
pub use hydra_core::rewrite::EndpointUrl;

/// Parse a provider endpoint URL into scheme / host / port / path-prefix.
///
/// Accepts `http://` and `https://` (the only schemes the W2 loader's
/// `is_usable_endpoint` allows through). When the URL omits the port the
/// scheme default is used (`443` for https, `80` for http). The path prefix is
/// the URL path with any trailing `/` stripped (so `https://gw/llm/` → `/llm`),
/// matching how `rewrite_path` re-joins it onto the request tail.
///
/// Returns `None` for malformed input (missing scheme, empty host); the loader
/// has already rejected unparseable endpoints, so reaching `None` here is a
/// post-reload data-graph inconsistency the shell logs and routes around.
pub fn parse_endpoint(endpoint: &str) -> Option<EndpointUrl> {
    let (scheme, rest) = endpoint
        .strip_prefix("https://")
        .map(|r| ("https", r))
        .or_else(|| endpoint.strip_prefix("http://").map(|r| ("http", r)))?;

    // Split authority from path/query/fragment.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = rest.get(..authority_end)?;
    if authority.is_empty() {
        return None;
    }
    let tail = rest.get(authority_end..).unwrap_or("");

    // Authority = host[:port]. IPv6 brackets are not expected for configured
    // provider endpoints; if present we still split on the last colon.
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => {
            let port: u16 = p.parse().ok()?;
            (h.to_string(), port)
        }
        _ => (authority.to_string(), default_port(scheme)),
    };

    // Path prefix: take the path component (drop ?query / #frag), strip the
    // trailing slash so `rewrite_path` concatenation is clean.
    let path_prefix = tail
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string();

    Some(EndpointUrl {
        scheme: scheme.to_string(),
        host,
        port,
        path_prefix,
    })
}

/// Scheme-default port (RFC 7230 §2.7).
fn default_port(scheme: &str) -> u16 {
    match scheme {
        "https" => 443,
        _ => 80,
    }
}

/// Build a Pingora [`HttpPeer`] from a parsed endpoint (design §6.4).
///
/// TLS is enabled iff the scheme is `https`; the SNI is the endpoint host
/// (without port). The address is `host:port`. The caller is responsible for
/// the path rewrite (handled in `upstream_request_filter` via `rewrite_path`).
pub fn build_peer(endpoint: &EndpointUrl) -> HttpPeer {
    let tls = endpoint.scheme == "https";
    let addr = format!("{}:{}", endpoint.host, endpoint.port);
    HttpPeer::new(addr, tls, endpoint.host.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_build_https_sni() {
        let ep = parse_endpoint("https://127.0.0.1").unwrap();
        assert_eq!(ep.scheme, "https");
        assert_eq!(ep.host, "127.0.0.1");
        assert_eq!(ep.port, 443);
        let peer = build_peer(&ep);
        assert!(peer.is_tls());
        assert_eq!(peer.sni, "127.0.0.1");
    }

    #[test]
    fn peer_build_http() {
        let ep = parse_endpoint("http://127.0.0.1:8080").unwrap();
        assert_eq!(ep.scheme, "http");
        assert_eq!(ep.port, 8080);
        let peer = build_peer(&ep);
        assert!(!peer.is_tls());
        assert_eq!(peer.sni, "127.0.0.1");
    }

    #[test]
    fn peer_build_custom_port() {
        let ep = parse_endpoint("https://127.0.0.1:8443").unwrap();
        assert_eq!(ep.port, 8443);
        let peer = build_peer(&ep);
        assert!(peer.is_tls());
        assert_eq!(peer.sni, "127.0.0.1");
    }

    #[test]
    fn peer_build_with_path_prefix() {
        let ep = parse_endpoint("https://gw.provider.com/llm/").unwrap();
        assert_eq!(ep.host, "gw.provider.com");
        assert_eq!(ep.path_prefix, "/llm");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_endpoint("ftp://x").is_none());
        assert!(parse_endpoint("https://").is_none());
        assert!(parse_endpoint("not a url").is_none());
    }

    #[test]
    fn parse_http_default_port_80() {
        let ep = parse_endpoint("http://upstream.local").unwrap();
        assert_eq!(ep.port, 80);
    }
}
