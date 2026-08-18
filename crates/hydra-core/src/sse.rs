//! Zero-copy usage scanning — response side, SSE / non-streaming JSON (pure).
//!
//! Implemented by the SSE lane (T5.9–T5.19) via TDD.
//!
//! ## Contract
//! [`UsageScanner::scan_chunk`] consumes one response chunk and returns a
//! [`ScanResult`]; [`UsageScanner::finalize`] yields the accumulated
//! [`Usage`] (if any).
//!
//! ## Purity / time-injection
//! Pure byte→state computation; no I/O, no time. Each chunk is scanned with
//! `memchr` for `"usage"` (zero-alloc, ~10 GB/s). On a hit, only the ~50-byte
//! `data:` payload — or the brace-matched usage object for non-stream JSON — is
//! deserialised. Cross-chunk boundaries are handled by a small tail buffer
//! activated only when a chunk ends mid-`data:` line (the aligned common path
//! stays zero-allocation). Schema dispatch is driven by
//! [`crate::model::ProviderKind`] (OpenAI / Anthropic / generic).
//! `data: [DONE]` terminates the stream.

use memchr::{memchr, memmem, memrchr};
use serde::Deserialize;

use crate::model::{ProviderKind, Usage};

/// The outcome of scanning a single response chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanResult {
    /// No `"usage"` and no terminator seen — the common, zero-cost case.
    Skip,
    /// A usage payload was located and accumulated.
    Found,
    /// The `data: [DONE]` stream terminator was observed; scanning stops.
    Done,
}

/// Pure, incremental usage scanner for SSE / non-streaming JSON responses.
///
/// Holds a small tail buffer (populated only when a chunk splits a `data:`
/// line across a boundary, so the aligned common path stays zero-allocation),
/// the accumulated [`Usage`], and the [`ProviderKind`] that drives schema
/// normalisation.
pub struct UsageScanner {
    provider: ProviderKind,
    tail: Vec<u8>,
    usage: Usage,
    seen_any: bool,
}

impl UsageScanner {
    /// Create a scanner bound to a provider's usage schema family.
    pub fn new(provider: ProviderKind) -> Self {
        Self {
            provider,
            tail: Vec::new(),
            usage: Usage::default(),
            seen_any: false,
        }
    }

    /// Scan one response chunk.
    ///
    /// Pure byte→state transition. Allocates only when a `data:` line is split
    /// across a chunk boundary (the common newline-terminated path is
    /// zero-allocation). Never panics on malformed input.
    pub fn scan_chunk(&mut self, chunk: &[u8]) -> ScanResult {
        // Combine any pending tail with the new chunk. `Vec::new()` does not
        // allocate, so the common (tail-empty) path stays zero-allocation.
        let combined: Vec<u8> = if self.tail.is_empty() {
            Vec::new()
        } else {
            let mut buf = std::mem::take(&mut self.tail);
            buf.extend_from_slice(chunk);
            buf
        };
        let buf: &[u8] = if combined.is_empty() {
            chunk
        } else {
            &combined
        };

        let has_usage = memmem::find(buf, b"\"usage\"").is_some();
        let has_done = memmem::find(buf, b"[DONE]").is_some();

        if has_usage {
            // Absorb EVERY usage-bearing payload in the chunk, in forward
            // stream order. Multiple usage objects may coalesce into one TCP
            // chunk (e.g. Anthropic `message_start` + `message_delta` arriving
            // together); parsing only the first would silently drop the rest.
            let mut absorbed_any = false;
            for json in extract_all_usage_objects(buf) {
                if self.absorb(json) {
                    absorbed_any = true;
                }
            }
            // Buffer any incomplete trailing data: line for cross-chunk
            // reassembly (a usage object split mid-JSON across two chunks).
            // `buffer_incomplete_tail` skips already-absorbed complete objects,
            // so this never double-counts.
            self.buffer_incomplete_tail(buf);
            return if absorbed_any {
                if has_done {
                    ScanResult::Done
                } else {
                    ScanResult::Found
                }
            } else if has_done {
                ScanResult::Done
            } else {
                ScanResult::Skip
            };
        }

        if has_done {
            return ScanResult::Done;
        }

        self.buffer_incomplete_tail(buf);
        ScanResult::Skip
    }

    /// Consume the scanner and return the final accumulated usage, if any was
    /// observed. Fields are the neutral [`Usage`] names (tokens_in /
    /// cache_hit_tokens / tokens_out); there is deliberately no
    /// derived `total_tokens`.
    pub fn finalize(self) -> Option<Usage> {
        if !self.seen_any {
            return None;
        }
        Some(self.usage)
    }

    /// Deserialise the small bare usage object and fold it into the
    /// accumulator per the active provider schema. Returns `true` if a usage
    /// object was parsed.
    fn absorb(&mut self, json: &[u8]) -> bool {
        match self.provider {
            // OpenAI / Generic: a single authoritative usage object (the final
            // chunk). Last-wins — these providers are not incremental.
            ProviderKind::OpenAi | ProviderKind::Generic => {
                let Ok(u) = serde_json::from_slice::<OpenAiUsageFields>(json) else {
                    return false;
                };
                self.usage.tokens_in = u.prompt_tokens.or(u.input_tokens);
                self.usage.tokens_out = u.completion_tokens.or(u.output_tokens);
                self.usage.cache_hit_tokens = u
                    .prompt_tokens_details
                    .and_then(|d| d.cached_tokens)
                    .or(u.cached_tokens);
                self.seen_any = true;
                true
            }
            // Anthropic: all usage fields are final/cumulative values.
            // `message_start.usage` carries the final `input_tokens` and an
            // initial `output_tokens` (often 1); `message_delta.usage` carries
            // the cumulative/final `output_tokens` and `cache_read_input_tokens`.
            // Last-wins (assignment) in stream order, so message_delta's values
            // override message_start's initial ones — never delta-sum.
            ProviderKind::Anthropic => {
                let Ok(u) = serde_json::from_slice::<AnthropicUsageFields>(json) else {
                    return false;
                };
                if let Some(v) = u.input_tokens {
                    self.usage.tokens_in = Some(v);
                }
                if let Some(v) = u.output_tokens {
                    self.usage.tokens_out = Some(v);
                }
                if let Some(v) = u.cache_read_input_tokens {
                    self.usage.cache_hit_tokens = Some(v);
                }
                self.seen_any = true;
                true
            }
        }
    }

    /// Carry an incomplete trailing `data:` line into the next chunk. Only
    /// activates when the buffer is not newline-terminated and the trailing
    /// portion begins a `data:` line, so aligned chunks stay zero-allocation.
    ///
    /// If the trailing line already yielded a COMPLETE usage object (absorbed
    /// above), it is NOT re-buffered — that would double-count on the next
    /// chunk. Only genuinely incomplete usage JSON (or non-usage `data:` lines)
    /// is carried forward for reassembly.
    fn buffer_incomplete_tail(&mut self, buf: &[u8]) {
        if buf.is_empty() || buf.ends_with(b"\n") {
            return;
        }
        let start = memrchr(b'\n', buf).map(|i| i + 1).unwrap_or(0);
        let trailing = &buf[start..];
        if memmem::find(trailing, b"data:").is_none() {
            return;
        }
        // A complete, already-absorbed usage object on the trailing line must
        // not be re-buffered (double-count). Skip only when the brace-match
        // succeeds; an incomplete object (no closing `}`) still needs the tail.
        if memmem::find(trailing, b"\"usage\"").is_some()
            && extract_usage_object(trailing).is_some()
        {
            return;
        }
        self.tail.extend_from_slice(trailing);
    }
}

/// Advance `idx` past any ASCII whitespace, clamped to `buf.len()`.
fn skip_ws(buf: &[u8], mut idx: usize) -> usize {
    while idx < buf.len() && buf[idx].is_ascii_whitespace() {
        idx += 1;
    }
    idx
}

/// Extract every bare usage object in `buf`, in forward stream order.
///
/// For SSE-framed responses, iterates ALL `data:` lines that contain a
/// `"usage"` key and brace-matches the usage object within each payload — so
/// multiple usage objects coalesced into one chunk (e.g. Anthropic
/// `message_start` + `message_delta`) are all yielded. For non-streaming JSON
/// (no `data:` framing), brace-matches the usage object in the raw buffer.
/// Returns borrowed slices — the JSON content is never copied.
fn extract_all_usage_objects(buf: &[u8]) -> Vec<&'_ [u8]> {
    let mut out = Vec::new();
    let mut search_from = 0;
    let mut found_sse_line = false;
    while search_from < buf.len() {
        let Some(rest) = buf.get(search_from..) else {
            break;
        };
        let Some(rel) = memmem::find(rest, b"data:") else {
            break;
        };
        let data_start = search_from + rel;
        let payload_start = skip_ws(buf, data_start + b"data:".len());
        let line_end = match buf.get(payload_start..).and_then(|r| memchr(b'\n', r)) {
            Some(r) => payload_start + r,
            None => buf.len(),
        };
        let Some(payload) = buf.get(payload_start..line_end) else {
            break;
        };
        if memmem::find(payload, b"\"usage\"").is_some() {
            found_sse_line = true;
            if let Some(json) = extract_usage_object(payload) {
                out.push(json);
            }
        }
        if line_end <= data_start {
            break; // defensive: avoid infinite loop on zero-advance
        }
        search_from = line_end;
    }
    if !found_sse_line {
        // Non-streaming JSON (no SSE `data:` framing): brace-match the whole buffer.
        if let Some(json) = extract_usage_object(buf) {
            out.push(json);
        }
    }
    out
}

/// For non-streaming JSON (no SSE `data:` framing): brace-match the object
/// following the `"usage"` key and return it as a borrowed slice. Handles
/// quoted strings and escapes so braces inside string values are ignored.
fn extract_usage_object(buf: &[u8]) -> Option<&[u8]> {
    let usage_pos = memmem::find(buf, b"\"usage\"")?;
    let after_key = usage_pos + b"\"usage\"".len();
    let rest = buf.get(after_key..)?;
    let colon_rel = memchr(b':', rest)?;
    let obj_start = skip_ws(buf, after_key + colon_rel + 1);
    if buf.get(obj_start) != Some(&b'{') {
        return None;
    }

    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut i = obj_start;
    while i < buf.len() {
        let byte = buf[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if byte == b'"' {
            in_string = true;
        } else if byte == b'{' {
            depth += 1;
        } else if byte == b'}' && depth > 0 {
            depth -= 1;
            if depth == 0 {
                return buf.get(obj_start..=i);
            }
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Minimal deserialisation shapes (serde ignores unknown fields). Only the
// ~50-byte bare usage object is ever fed to these — never the full response.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct OpenAiUsageFields {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    /// Generic-provider fallback field name.
    input_tokens: Option<u64>,
    /// Generic-provider fallback field name.
    output_tokens: Option<u64>,
    /// OpenAI prompt-cache breakdown (`usage.prompt_tokens_details`).
    /// `#[serde(default)]` so its absence does not fail deserialisation.
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    /// Generic-provider fallback: a top-level `cached_tokens` (some
    /// OpenAI-compatible gateways surface it outside the details sub-object).
    #[serde(default)]
    cached_tokens: Option<u64>,
}

/// OpenAI `usage.prompt_tokens_details` — currently only `cached_tokens` is
/// interesting. Unknown fields are ignored; a missing sub-object is tolerated
/// via the `Option` on the parent.
#[derive(Deserialize, Default)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct AnthropicUsageFields {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    /// Anthropic prompt-cache read hits (mirrors OpenAI `cached_tokens`).
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}
