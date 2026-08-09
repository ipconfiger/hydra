//! T5.9–T5.19 — `sse::UsageScanner` zero-copy response-side usage scanning.
//!
//! Each chunk is scanned with `memchr` for `"usage"` (zero-alloc, ~10 GB/s).
//! On a hit, only the ~50-byte `data:` payload (or the brace-matched usage
//! object for non-stream JSON) is deserialised. `ProviderKind` drives schema
//! normalisation; Anthropic deltas accumulate. `data: [DONE]` terminates.

use hydra_core::model::{ProviderKind, Usage};
use hydra_core::sse::{ScanResult, UsageScanner};

// T5.9 — no `"usage"` in the chunk ⇒ Skip, no deserialisation, no allocation.
#[test]
fn usage_scan_no_usage_skips_zero_alloc() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    let chunk = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n";
    assert_eq!(scanner.scan_chunk(chunk), ScanResult::Skip);
    assert_eq!(scanner.finalize(), None);
}

// T5.10 — a chunk containing `"usage"` ⇒ Found and only that chunk is parsed.
#[test]
fn usage_scan_finds_usage_memchr() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    let chunk = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4,\"total_tokens\":7}}\n";
    assert_eq!(scanner.scan_chunk(chunk), ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            prompt_tokens: Some(3),
            completion_tokens: Some(4),
            total_tokens: Some(7),
            ..Default::default()
        })
    );
}

// T5.11 — OpenAI final chunk carries the authoritative usage object.
#[test]
fn usage_openai_final_chunk() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    // Stream of content chunks, then the final usage-bearing chunk.
    scanner.scan_chunk(b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n");
    let final_chunk = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"total_tokens\":150}}\n";
    assert_eq!(scanner.scan_chunk(final_chunk), ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            total_tokens: Some(150),
            ..Default::default()
        })
    );
}

// T5.12 — Anthropic message_delta: input_tokens→prompt, output_tokens→completion.
#[test]
fn usage_anthropic_message_delta() {
    let mut scanner = UsageScanner::new(ProviderKind::Anthropic);
    let chunk = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":12,\"output_tokens\":8}}\n\n";
    assert_eq!(scanner.scan_chunk(chunk), ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            prompt_tokens: Some(12),
            completion_tokens: Some(8),
            // Anthropic omits total ⇒ computed as prompt + completion.
            total_tokens: Some(20),
            ..Default::default()
        })
    );
}

// T5.13 — Anthropic incremental: multiple deltas' output_tokens accumulate.
#[test]
fn usage_anthropic_incremental_accumulate() {
    let mut scanner = UsageScanner::new(ProviderKind::Anthropic);
    // First delta establishes input_tokens and an initial output delta.
    scanner.scan_chunk(
        b"event: message_delta\ndata: {\"usage\":{\"input_tokens\":40,\"output_tokens\":5}}\n\n",
    );
    // Subsequent deltas carry only output_tokens increments.
    scanner.scan_chunk(b"event: message_delta\ndata: {\"usage\":{\"output_tokens\":3}}\n\n");
    scanner.scan_chunk(b"event: message_delta\ndata: {\"usage\":{\"output_tokens\":2}}\n\n");

    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            prompt_tokens: Some(40),     // single input_tokens occurrence
            completion_tokens: Some(10), // 5 + 3 + 2 accumulated
            total_tokens: Some(50),      // computed
            ..Default::default()
        })
    );
}

// T5.14 — `data: [DONE]` terminates the stream; later bytes are ignored.
#[test]
fn usage_done_marker() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    let usage = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n";
    scanner.scan_chunk(usage);
    assert_eq!(scanner.scan_chunk(b"data: [DONE]\n"), ScanResult::Done);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            prompt_tokens: Some(1),
            completion_tokens: Some(1),
            total_tokens: Some(2),
            ..Default::default()
        })
    );
}

// T5.15 — `"usage"` line split across two chunks ⇒ tail buffer rejoins it.
#[test]
fn usage_cross_chunk_boundary() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    // First chunk ends mid-`data:` line, before `"usage"` is complete.
    scanner.scan_chunk(b"data: {\"choices\":[]}\ndata: {\"usa");
    // Second chunk completes the usage payload.
    let result = scanner
        .scan_chunk(b"ge\":{\"prompt_tokens\":9,\"completion_tokens\":1,\"total_tokens\":10}}\n");
    assert_eq!(result, ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            prompt_tokens: Some(9),
            completion_tokens: Some(1),
            total_tokens: Some(10),
            ..Default::default()
        })
    );
}

// T5.16 — non-streaming JSON: a single full JSON object (no SSE framing).
#[test]
fn usage_non_stream_json() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    let body = b"{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":14,\"total_tokens\":21}}";
    assert_eq!(scanner.scan_chunk(body), ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            prompt_tokens: Some(7),
            completion_tokens: Some(14),
            total_tokens: Some(21),
            ..Default::default()
        })
    );
}

// T5.17 — schema dispatch by ProviderKind from the same payload.
#[test]
fn usage_schema_dispatch_by_provider() {
    // Payload carries BOTH OpenAI-style and Anthropic-style field names.
    let payload =
        b"data: {\"usage\":{\"prompt_tokens\":7,\"input_tokens\":99,\"output_tokens\":5}}\n";

    let mut oai = UsageScanner::new(ProviderKind::OpenAi);
    oai.scan_chunk(payload);
    let oai_usage = oai.finalize().expect("openai usage");
    // OpenAI prefers prompt_tokens.
    assert_eq!(oai_usage.prompt_tokens, Some(7));

    let mut ant = UsageScanner::new(ProviderKind::Anthropic);
    ant.scan_chunk(payload);
    let ant_usage = ant.finalize().expect("anthropic usage");
    // Anthropic reads input_tokens/output_tokens.
    assert_eq!(ant_usage.prompt_tokens, Some(99));
    assert_eq!(ant_usage.completion_tokens, Some(5));

    // Generic falls back to OpenAI-style field names.
    let mut generic = UsageScanner::new(ProviderKind::Generic);
    generic.scan_chunk(payload);
    let generic_usage = generic.finalize().expect("generic usage");
    assert_eq!(generic_usage.prompt_tokens, Some(7));
}

// T5.18 — malformed JSON containing `"usage"` is skipped without panicking.
#[test]
fn usage_malformed_chunk_skipped() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    // Garbage after "usage" that is not valid JSON.
    let malformed = b"data: {\"usage\":!!!not-json}\n";
    // No panic; the malformed line is skipped (no usage absorbed).
    assert_eq!(scanner.scan_chunk(malformed), ScanResult::Skip);
    // A separate scanner fed only the malformed chunk reports no usage.
    let mut only_bad = UsageScanner::new(ProviderKind::OpenAi);
    only_bad.scan_chunk(malformed);
    assert_eq!(only_bad.finalize(), None);

    // The original scanner survives and parses a subsequent valid chunk.
    let valid =
        b"data: {\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2,\"total_tokens\":4}}\n";
    assert_eq!(scanner.scan_chunk(valid), ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            prompt_tokens: Some(2),
            completion_tokens: Some(2),
            total_tokens: Some(4),
            ..Default::default()
        })
    );
}

// T5.19 — OpenAI stream without `stream_options.include_usage` ⇒ finalize None.
#[test]
fn usage_openai_no_include_usage() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    scanner.scan_chunk(b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n");
    scanner.scan_chunk(b"data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n");
    scanner.scan_chunk(b"data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n");
    scanner.scan_chunk(b"data: [DONE]\n");
    // No usage ever observed.
    assert_eq!(scanner.finalize(), None);
}

// T5.20 — OpenAI `usage.prompt_tokens_details.cached_tokens` ⇒ cached_tokens.
#[test]
fn usage_openai_cached_tokens_from_details() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    let chunk = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"total_tokens\":150,\"prompt_tokens_details\":{\"cached_tokens\":42}}}\n";
    assert_eq!(scanner.scan_chunk(chunk), ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            total_tokens: Some(150),
            cached_tokens: Some(42),
        })
    );
}

// T5.20b — OpenAI usage WITHOUT `prompt_tokens_details` ⇒ cached_tokens None.
#[test]
fn usage_openai_no_details_cached_tokens_none() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    let chunk = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"total_tokens\":150}}\n";
    scanner.scan_chunk(chunk);
    let u = scanner.finalize().expect("usage");
    assert_eq!(u.cached_tokens, None);
}

// T5.20c — Anthropic `cache_read_input_tokens` ⇒ cached_tokens.
#[test]
fn usage_anthropic_cached_tokens() {
    let mut scanner = UsageScanner::new(ProviderKind::Anthropic);
    let chunk = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":120,\"output_tokens\":30,\"cache_read_input_tokens\":77}}\n\n";
    assert_eq!(scanner.scan_chunk(chunk), ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            prompt_tokens: Some(120),
            completion_tokens: Some(30),
            total_tokens: Some(150),
            cached_tokens: Some(77),
        })
    );
}

// T5.20d — Generic provider surfacing a top-level `cached_tokens`.
#[test]
fn usage_generic_top_level_cached_tokens() {
    let mut scanner = UsageScanner::new(ProviderKind::Generic);
    let chunk = b"data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15,\"cached_tokens\":8}}\n";
    scanner.scan_chunk(chunk);
    let u = scanner.finalize().expect("generic usage");
    assert_eq!(u.cached_tokens, Some(8));
}

// T5.20e — OpenAI details present but `cached_tokens` key absent ⇒ None.
#[test]
fn usage_openai_details_without_cached_field() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    // `prompt_tokens_details` present with an unrelated field only.
    let chunk = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"total_tokens\":5,\"prompt_tokens_details\":{\"audio_tokens\":1}}}\n";
    scanner.scan_chunk(chunk);
    let u = scanner.finalize().expect("usage");
    assert_eq!(u.cached_tokens, None);
}
