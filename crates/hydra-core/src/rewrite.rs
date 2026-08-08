//! Request-path rewrite & api-key masking (pure).
//!
//! This foundation lane owns the parsed-endpoint value type. The pure
//! `rewrite_path` (first `/v1` split) and `mask_key` (first-4 + last-4)
//! functions are implemented by the Rewrite lane (T8.x) via TDD. Contract:
//! `rewrite_path(req_path, &EndpointUrl) -> String` and `mask_key(&str) ->
//! String` — both pure, allocation only for the returned `String`.

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
