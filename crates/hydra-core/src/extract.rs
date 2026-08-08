//! Zero-copy metadata extraction — request side (pure).
//!
//! Implemented by the Extract lane (T5.1–T5.8) via TDD.
//!
//! ## Contract
//! `extract_model(&[u8]) -> Option<&[u8]>`
//!
//! Scans the request body's **first chunk** with `memchr::memmem::find` for
//! the `"model"` key, returns a **borrowed slice** of the value (zero JSON
//! parse, zero allocation, early-exit at ~20 bytes). The returned slice is
//! always a sub-slice of the input — pointer-bounded, compile-time guaranteed
//! zero-alloc. No complete-body deserialisation ever occurs on the hot path.

use memchr::memmem;

/// The `"model"` JSON key as it appears on the wire, including quotes.
const MODEL_KEY: &[u8] = b"\"model\"";

/// Zero-copy extraction of the `"model"` field value from a request body chunk.
///
/// Scans `body` with `memchr` for the byte sequence `"model"`, then walks
/// forward past the key, the `:` separator and any ASCII whitespace, and
/// returns the **borrowed** byte slice between the value's opening and closing
/// `"`. The returned slice is always a sub-slice of `body` (pointer-bounded),
/// so this performs **zero allocation** and parses **no JSON** — only a SIMD
/// byte scan plus index arithmetic.
///
/// Every index is bounds-checked via `slice::get`, so empty / truncated / short
/// input returns `None` and never panics.
///
/// # First-match heuristic
///
/// `memchr` returns the *first* occurrence of `"model"`. Real
/// OpenAI-compatible request bodies place `"model"` as the top-level first
/// field (`{"model":"…","messages":[…]}`), so first-match yields the correct
/// top-level value without ever reading the whole body. A `"model"` key nested
/// deeper (e.g. inside `messages`) that textually precedes the top-level key
/// would be matched first; this is an accepted tradeoff for ~10 GB/s zero-copy
/// scanning. The hot path never needs more than the first chunk.
pub fn extract_model(body: &[u8]) -> Option<&[u8]> {
    // Locate the first `"model"` key (zero-alloc SIMD scan, ~20-byte early exit).
    let key_start = memmem::find(body, MODEL_KEY)?;
    let mut idx = key_start + MODEL_KEY.len();

    // Find the `:` separating the key from its value.
    let rest = body.get(idx..)?;
    let colon = memchr::memchr(b':', rest)?;
    idx += colon + 1;

    // Skip ASCII whitespace between `:` and the value.
    while let Some(&byte) = body.get(idx) {
        if byte.is_ascii_whitespace() {
            idx += 1;
        } else {
            break;
        }
    }

    // Expect an opening quote for the (string) value.
    if body.get(idx) != Some(&b'"') {
        return None;
    }
    let value_start = idx + 1;

    // Find the closing quote — the value is the bytes in between (borrowed).
    let tail = body.get(value_start..)?;
    let close = memchr::memchr(b'"', tail)?;
    let value_end = value_start + close;
    body.get(value_start..value_end)
}
