// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

//! Types.
//!
//! This module is a reexport of the driver's source-local `postgres_types`
//! fork. It tracks upstream 0.2.14; see `vendor/postgres-types/FORK.md` for the
//! deliberately small codec delta.
//!
//! The crate's `with-chrono-0_4`, `with-uuid-1`, `with-serde_json-1` and the
//! rest of that family gate NO code in this file. Each forwards to the matching
//! fork feature. Most implementations remain byte-for-byte upstream; the live
//! regressions under `tests/suite` cover every local divergence. The clippy
//! gate still compiles all of them via `--all-features`, which is what catches
//! a feature that no longer resolves.

#[doc(inline)]
pub use postgres_types::*;
