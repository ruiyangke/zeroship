//! Foundational layers shared across multiple surface APIs.
//!
//! Each submodule houses an algorithm/op backend that web/* and
//! node/* surfaces call into. No V8 types — slice-in / Vec-out.
//! Chromium-style "base/" name: a dumping ground for cross-cutting
//! foundational code.
pub mod crypto;
