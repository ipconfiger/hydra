//! T5.1–T5.8 — `extract::extract_model` zero-copy request-side extraction.
//!
//! `extract_model` scans the request body's first chunk with `memchr` for the
//! byte sequence `"model"` and returns a **borrowed** slice of the value — no
//! full-body serde, no allocation, early-exit at ~20 bytes.

use hydra_core::extract::extract_model;

// T5.1 — standard form, model is the first field.
#[test]
fn extract_model_standard() {
    let body = br#"{"model":"gpt-4o","messages":[]}"#;
    assert_eq!(extract_model(body), Some(&b"gpt-4o"[..]));
}

// T5.2 — whitespace tolerance around the key, colon and value.
#[test]
fn extract_model_whitespace_tolerant() {
    let body = br#"{ "model" : "gpt-4o" }"#;
    assert_eq!(extract_model(body), Some(&b"gpt-4o"[..]));
}

// T5.3 — model is not the first field; memchr still locates it.
#[test]
fn extract_model_not_first_field() {
    let body = br#"{"a":1,"model":"x","b":2}"#;
    assert_eq!(extract_model(body), Some(&b"x"[..]));
}

// T5.4 — no model key at all returns None.
#[test]
fn extract_model_missing_returns_none() {
    let body = br#"{"messages":[{"role":"user"}]}"#;
    assert_eq!(extract_model(body), None);
}

// T5.5 — zero allocation: the returned slice must be a sub-range of the input
// (its pointer falls within the input's address bounds). The zero-alloc
// guarantee itself is enforced at compile time by the `&[u8]` return type.
#[test]
fn extract_model_no_allocation() {
    let body = br#"{"model":"gpt-4o"}"#;
    let model = extract_model(body).expect("model present");

    let body_start = body.as_ptr() as usize;
    let body_end = body_start + body.len();
    let model_start = model.as_ptr() as usize;
    let model_end = model_start + model.len();

    assert!(
        model_start >= body_start && model_end <= body_end,
        "returned slice must lie within the input buffer"
    );
    assert_eq!(model, b"gpt-4o");
}

// T5.6 — nested `"model"` handling. Real OpenAI-compatible bodies put `"model"`
// as the top-level first field, so first-match returns the top-level value.
#[test]
fn extract_model_top_level_preferred_when_first() {
    // Top-level `model` appears before the nested one → correctly returned.
    let body = br#"{"model":"real","messages":[{"model":"x"}]}"#;
    assert_eq!(extract_model(body), Some(&b"real"[..]));
}

// T5.6 (documented heuristic) — when a nested `"model"` occurs *before* the
// top-level key, first-match returns the nested value. This is the accepted
// tradeoff of zero-copy SIMD scanning (never reading the whole body); it never
// arises for well-formed OpenAI requests where `model` leads the object.
#[test]
fn extract_model_nested_first_match_documented() {
    let body = br#"{"messages":[{"model":"x"}],"model":"real"}"#;
    // First-match lands on the nested key — documented limitation.
    assert_eq!(extract_model(body), Some(&b"x"[..]));
}

// T5.7 — empty / very short input never panics.
#[test]
fn extract_model_short_input_no_panic() {
    assert_eq!(extract_model(b""), None);
    assert_eq!(extract_model(b"{"), None);
    assert_eq!(extract_model(b"\"model\""), None);
    assert_eq!(extract_model(b"\"model\":"), None);
    assert_eq!(extract_model(b"\"model\" :"), None);
    assert_eq!(extract_model(b"\"model\": 1"), None);
    assert_eq!(extract_model(b"\"model\":\""), None);
}

// T5.8 — only the first chunk is needed; the function never reads past the
// given slice (simulating a first chunk that already contains the field).
#[test]
fn extract_model_first_chunk_only() {
    // A first chunk containing model suffices; no need to scan a 10 MiB body.
    let first_chunk = br#"{"model":"claude-3-opus""#;
    assert_eq!(extract_model(first_chunk), Some(&b"claude-3-opus"[..]));

    // Even a truncated-but-complete-model first chunk works mid-stream.
    let chunk = b"data: blah\n{\"model\":\"qwen\",";
    assert_eq!(extract_model(chunk), Some(&b"qwen"[..]));
}
