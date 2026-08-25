// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

//! Types.
//!
//! This module is a reexport of the `postgres_types` crate.
//!
//! The crate's `with-chrono-0_4`, `with-uuid-1`, `with-serde_json-1` and the
//! rest of that family gate NO code here - checked 2026-08-25, nothing in
//! `src/` reads one. Each forwards to the matching `postgres-types` feature,
//! so a round-trip test for them would be measuring an upstream crate rather
//! than this driver, and their absence from the suite is deliberate. The
//! clippy gate still compiles all of them via `--all-features`, which is what
//! catches a feature that no longer resolves.

#[doc(inline)]
pub use postgres_types::*;
