//! Upstream HTTP client for the **terminate-in-Pingora** proxy mode
//! (design-change `terminate-mode` §4.2).
//!
//! In terminate mode the proxy no longer hands the body off to Pingora's
//! upstream dialler. Instead `request_filter` reads the full downstream body,
//! resolves a route, then uses this client to issue the request to the chosen
//! provider itself, streaming the response back chunk-by-chunk through the
//! downstream `Session`.
//!
//! ## Why a *separate* reqwest client
//!
//! The proxy shell already owns two other reqwest clients — the auth-upstream
//! client ([`crate::http::HttpAuthChecker`]) and the breaker probe client
//! ([`crate::proxy::breaker_wrap::probe_task`]). Those use **short** timeouts
//! (sub-second to ~1.5 s) because they must fail fast. LLM chat-completion
//! requests — especially streaming SSE — can take **minutes** to first byte.
//! Sharing a short-timeout client would abort legitimate long generations, so
//! `ProviderClient` carries its own long-lived (300 s timeout) connection pool.
//!
//! ## Failover replay is free
//!
//! The request body is held as [`bytes::Bytes`] by the caller.
//! [`Bytes::clone`] is an O(1) refcount bump, so the failover loop can hand
//! the *same* body bytes to N candidate providers with zero memcpy — the
//! "body replay is free" property that the old stream-through design fought
//! Pingora's retry machinery to approximate (design §4.3).

use std::time::Duration;

use bytes::Bytes;
use hydra_core::model::Provider;
use hydra_core::rewrite::rewrite_path;
use pingora_http::RequestHeader;

use crate::proxy::peer::parse_endpoint;

/// Long-lived upstream HTTP client used by the terminate-mode proxy to call LLM
/// providers (design-change §4.2). One instance lives on [`crate::proxy::HydraProxy`]
/// for the whole server lifetime; the inner `reqwest::Client` owns a shared
/// connection pool.
pub struct ProviderClient {
    client: reqwest::Client,
}

impl ProviderClient {
    /// Build a client tuned for long-lived LLM/SSE upstream calls: a generous
    /// per-host idle pool, a long idle timeout, and a 300 s overall timeout so
    /// slow first-token providers are not aborted. Uses `rustls` (matched by
    /// the `http-client` feature in `Cargo.toml`).
    ///
    /// Infallible: on the (catastrophic, essentially never happens with rustls)
    /// case the TLS backend fails to initialise, we fall back to reqwest's
    /// default client rather than panicking — production code must not `unwrap`.
    #[must_use]
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(32)
            .pool_idle_timeout(Duration::from_secs(90))
            .timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }

    /// Construct the upstream request for one failover attempt.
    ///
    /// Performs the three rewrites the stream-through design did across
    /// `upstream_request_filter` (design §6.5), now collapsed into one call:
    ///
    /// 1. **Path rewrite** — `rewrite_path` re-joins the downstream `/v1/…`
    ///    tail onto the provider's parsed endpoint base (`scheme://host[:port]`
    ///    + path prefix).
    /// 2. **Key swap** — the client's `Authorization` is replaced with the
    ///    provider api-key chosen for this attempt.
    /// 3. **Host** — the `Host` header is set to the provider host.
    ///
    /// The body is attached as `reqwest::Body::from(body.clone())` — an O(1)
    /// `Bytes` refcount bump, so each failover attempt reuses the same bytes
    /// (design §4.3, Oracle correction #2).
    ///
    /// Returns a `RequestBuilder` (infallible to construct) so the failover
    /// loop can rebuild fresh per candidate without owning a `Client` handle.
    /// The downstream-relevant headers (`Accept`, `Content-Type`) are forwarded
    /// from the original request where present, then overridden as needed.
    pub fn build_request(
        &self,
        original: &RequestHeader,
        provider: &Provider,
        upstream_key: &str,
        body: &Bytes,
        trace_id: &str,
    ) -> reqwest::RequestBuilder {
        let endpoint = parse_endpoint(&provider.endpoint);
        let url = match &endpoint {
            Some(ep) => rewrite_path(original.uri.path(), ep),
            None => {
                // Malformed endpoint (loader normally rejects these; a reload
                // data-graph inconsistency can still reach here). Fall back to
                // the raw endpoint string + the request path so the attempt
                // fails loudly at the network layer rather than panicking.
                format!(
                    "{}/{}",
                    provider.endpoint.trim_end_matches('/'),
                    original.uri.path().trim_start_matches('/')
                )
            }
        };

        let mut builder = self
            .client
            .request(original.method.clone(), &url)
            .header("Authorization", format!("Bearer {upstream_key}"))
            .header("X-Hydra-Trace-Id", trace_id)
            .body(body.clone());

        if let Some(ep) = &endpoint {
            builder = builder.header("Host", &ep.host);
        }

        // Forward a minimal set of client hints that affect provider behaviour.
        // We deliberately do NOT forward `Authorization` (swapped above) and we
        // let providers set their own `Content-Type` default when the client
        // omitted one.
        if let Some(accept) = original.headers.get("accept").and_then(|v| v.to_str().ok()) {
            builder = builder.header("Accept", accept);
        }
        if let Some(ct) = original
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
        {
            builder = builder.header("Content-Type", ct);
        }

        builder
    }

    /// Execute a built request and return the streaming response. Thin wrapper
    /// so the failover loop has a single, mockable seam for "send to upstream".
    pub async fn send(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, reqwest::Error> {
        req.send().await
    }
}

impl Default for ProviderClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(path: &str) -> RequestHeader {
        let mut h = RequestHeader::build("POST", path.as_bytes(), Some(0)).expect("build header");
        let uri: http::Uri = path.parse().expect("parse uri");
        h.set_uri(uri);
        h
    }

    #[test]
    fn build_rewrites_path_and_swaps_key() {
        let pc = ProviderClient::new();
        let provider = Provider {
            id: "p1".into(),
            key: "openai".into(),
            name: "OpenAI".into(),
            endpoint: "https://api.openai.com".into(),
            weight: 1,
            created_at: "t".into(),
            updated_at: "t".into(),
        };
        let body = Bytes::from_static(b"{\"model\":\"gpt-4\"}");
        // We can't easily inspect a RequestBuilder's headers/url without
        // building it; build() is infallible for well-formed input.
        let rb = pc.build_request(
            &header("/v1/chat/completions"),
            &provider,
            "sk-upstream-secret",
            &body,
            "trace-1",
        );
        let req = rb.build().expect("build request");
        assert_eq!(req.method(), reqwest::Method::POST);
        assert_eq!(
            req.url().as_str(),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            req.headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer sk-upstream-secret")
        );
        assert_eq!(
            req.headers().get("host").and_then(|v| v.to_str().ok()),
            Some("api.openai.com")
        );
        assert_eq!(
            req.headers()
                .get("x-hydra-trace-id")
                .and_then(|v| v.to_str().ok()),
            Some("trace-1")
        );
    }

    #[test]
    fn build_applies_endpoint_path_prefix() {
        let pc = ProviderClient::new();
        let provider = Provider {
            id: "gw".into(),
            key: "gw".into(),
            name: "GW".into(),
            endpoint: "https://gw.example.com/llm/".into(),
            weight: 1,
            created_at: "t".into(),
            updated_at: "t".into(),
        };
        let body = Bytes::from_static(b"{}");
        let rb = pc.build_request(
            &header("/v1/chat/completions"),
            &provider,
            "k",
            &body,
            "trace",
        );
        let req = rb.build().expect("build");
        assert_eq!(
            req.url().as_str(),
            "https://gw.example.com/llm/v1/chat/completions"
        );
    }
}
