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
