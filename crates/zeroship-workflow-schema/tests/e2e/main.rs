//! The end-to-end contracts for `zeroship-workflow-schema`, in one binary.
//!
//! Each case re-runs the schema generator and its owned-name binder through
//! `node`, so it exercises the shipped JavaScript tools as external processes.
//! `Cargo.toml` sets `autotests = false`, so a new `tests/e2e/<name>.rs` is
//! compiled by nothing until it is declared below; add `mod <name>;` in the
//! same change as the file, or its tests never run.

mod generated_artifacts;
mod owned_names;
