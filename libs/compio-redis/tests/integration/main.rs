//! The compio-redis integration suites, linked into one test executable.
//!
//! Cargo links one executable per `tests/*.rs`; `Cargo.toml` sets `autotests =
//! false` and registers this file as the `integration` target, so a suite is
//! compiled only once it is declared here. Add `mod <name>;` with the file or its
//! tests never run. The suites share `common`, declared once here and reached as
//! `crate::common::...`.

mod common;

mod cluster;
mod redis_ops;
