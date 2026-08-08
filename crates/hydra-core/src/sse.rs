//! Zero-copy usage scanning — response side, SSE / non-streaming JSON (pure).
//!
//! Implemented by the SSE lane (T5.9–T5.19) via TDD.
//!
//! ## Contract
//! `UsageScanner::scan_chunk(&mut State, &[u8], ProviderKind) -> ScanResult`
//! and `State::finalize() -> Option<Usage>`.
//!
//! ## Purity / time-injection
//! Pure byte→value computation; no I/O, no time. Each chunk is scanned with
//! `memchr` for `"usage"` (zero-alloc, ~10 GB/s). On a hit, only the ~50-byte
//! JSON slice is deserialised. Cross-chunk boundaries handled by a small tail
//! buffer activated only when an incomplete `data:` line is seen (nominal
//! path stays zero-alloc). Schema dispatch is driven by [`crate::model::ProviderKind`]
//! (OpenAI / Anthropic / generic). `data: [DONE]` terminates the stream.
