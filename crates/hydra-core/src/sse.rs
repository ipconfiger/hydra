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
            // Locate the ~50-byte bare usage object (brace-matched), whether the
            // carrier is an SSE `data:` payload or a raw non-stream JSON body.
            if let Some(json) = extract_usage_json(buf) {
                if self.absorb(json) {
                    return if has_done {
                        ScanResult::Done
                    } else {
                        ScanResult::Found
                    };
                }
            }
            // `"usage"` was present but no payload parsed (incomplete across a
            // boundary, or malformed) — never panic; buffer the trailing line.
            self.buffer_incomplete_tail(buf);
            return if has_done {
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
    /// observed. Computes `total_tokens` from `prompt + completion` when the
    /// provider omitted it (e.g. Anthropic, which has no `total_tokens` field).
    pub fn finalize(mut self) -> Option<Usage> {
        if !self.seen_any {
            return None;
        }
        if self.usage.total_tokens.is_none()
            && self.usage.prompt_tokens.is_some()
            && self.usage.completion_tokens.is_some()
        {
            self.usage.total_tokens = Some(
                self.usage.prompt_tokens.unwrap_or(0) + self.usage.completion_tokens.unwrap_or(0),
            );
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
                self.usage.prompt_tokens = u.prompt_tokens.or(u.input_tokens);
                self.usage.completion_tokens = u.completion_tokens.or(u.output_tokens);
                self.usage.total_tokens = u.total_tokens;
                self.usage.cached_tokens = u
                    .prompt_tokens_details
                    .and_then(|d| d.cached_tokens)
                    .or(u.cached_tokens);
                self.seen_any = true;
                true
            }
            // Anthropic: message_delta usage is incremental — accumulate the
            // input/output token deltas across chunks.
            ProviderKind::Anthropic => {
                let Ok(u) = serde_json::from_slice::<AnthropicUsageFields>(json) else {
                    return false;
                };
                accumulate(&mut self.usage.prompt_tokens, u.input_tokens);
                accumulate(&mut self.usage.completion_tokens, u.output_tokens);
                // Anthropic reports cache reads via `cache_read_input_tokens`.
                // It is a running total (not a delta) on the final usage object,
                // so last-wins (not accumulate) is correct.
                accumulate(&mut self.usage.cached_tokens, u.cache_read_input_tokens);
                self.seen_any = true;
                true
            }
        }
    }

    /// Carry an incomplete trailing `data:` line into the next chunk. Only
    /// activates when the buffer is not newline-terminated and the trailing
    /// portion begins a `data:` line, so aligned chunks stay zero-allocation.
    fn buffer_incomplete_tail(&mut self, buf: &[u8]) {
        if buf.is_empty() || buf.ends_with(b"\n") {
            return;
        }
        let start = memrchr(b'\n', buf).map(|i| i + 1).unwrap_or(0);
        let trailing = &buf[start..];
        if memmem::find(trailing, b"data:").is_some() {
            self.tail.extend_from_slice(trailing);
        }
    }
}

/// Add `delta` into `slot` (treating a `None` slot as zero), preserving `None`
/// when `delta` itself is absent.
fn accumulate(slot: &mut Option<u64>, delta: Option<u64>) {
    if let Some(d) = delta {
        *slot = Some(slot.unwrap_or(0) + d);
    }
}

/// Advance `idx` past any ASCII whitespace, clamped to `buf.len()`.
fn skip_ws(buf: &[u8], mut idx: usize) -> usize {
    while idx < buf.len() && buf[idx].is_ascii_whitespace() {
        idx += 1;
    }
    idx
}

/// Extract the ~50-byte bare usage object as a borrowed slice.
///
/// The carrier is the SSE `data:` payload when present, otherwise the whole
/// buffer (non-streaming JSON). [`extract_usage_object`] then brace-matches the
/// object following the `"usage"` key within that carrier — so the hot path
/// deserialises only the bare usage object, never the full response body.
fn extract_usage_json(buf: &[u8]) -> Option<&[u8]> {
    let carrier = extract_sse_data_json(buf).unwrap_or(buf);
    extract_usage_object(carrier)
}

/// Locate the SSE `data:` payload (borrowed) of the first `data:` line that
/// contains a `"usage"` key. The payload is sliced up to the next newline —
/// no full-body serde, no allocation. Returns `None` when no such line exists
/// (e.g. non-streaming JSON).
fn extract_sse_data_json(buf: &[u8]) -> Option<&[u8]> {
    let mut search_from = 0;
    while let Some(rest) = buf.get(search_from..) {
        let rel = memmem::find(rest, b"data:")?;
        let data_start = search_from + rel;
        let payload_start = skip_ws(buf, data_start + b"data:".len());
        let line_end = match memchr(b'\n', buf.get(payload_start..)?) {
            Some(r) => payload_start + r,
            None => buf.len(),
        };
        let payload = buf.get(payload_start..line_end)?;
        if memmem::find(payload, b"\"usage\"").is_some() {
            return Some(payload);
        }
        search_from = line_end;
    }
    None
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
    total_tokens: Option<u64>,
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
