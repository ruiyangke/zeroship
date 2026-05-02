//! Native Body model per WHATWG Fetch §3 (https://fetch.spec.whatwg.org/#body).
//!
//! This module hosts the **shared** machinery that both Request and
//! Response build on:
//!
//! - `body::BodyImpl` — the two-headed body (D-2): a native ReadableStream
//!   plus an optional source for redirect rewinding / cheap clone.
//! - `body::Body` — Rust trait implemented by Request and Response so
//!   `text` / `json` / `arrayBuffer` / `bytes` / `blob` / `formData`
//!   can be installed on each class's prototype via a single shared
//!   installer (per v2 fix Process-8 — NOT a V8 base class, just a
//!   shared Rust trait).
//! - `extract::extract_body` — Fetch §3.2 "extract a body" with the
//!   v2 dispatch order fix (C-10) and the proper USVString conversion
//!   for string bodies (C-11).
//! - `consumers` — the 6 body consumer methods, including the v2
//!   error-shape fix (MAJOR-25): `json()` rejects with **SyntaxError**
//!   not TypeError, `arrayBuffer()` rejects with **RangeError** at
//!   2GB, etc.
//! - `body_stream` — `read_all_bytes` + `read_one_chunk` helpers that
//!   drive consumers through the streams' public reader API. Per
//!   design §III.4, consumers MUST flow through the JS-visible reader
//!   to honour spec lock checks (locked stream → TypeError).
//!
//! ## D-2 single-source slot rule
//!
//! Body state is split between the BodyImpl struct (Rust-side: source,
//! length) and a private V8 symbol on the wrapper for the stream Global.
//! No state is duplicated. Spec slots that need JS identity (the body
//! ReadableStream returned by `request.body`) live in V8 storage; pure
//! data (the source bytes for clone) lives on the Rust side.

pub mod body;
pub mod body_stream;
pub mod consumers;
pub mod extract;

pub use body::{Body, BodyImpl, BodySource};
pub use extract::extract_body;
