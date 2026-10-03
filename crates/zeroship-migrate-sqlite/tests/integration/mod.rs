//! The sqlite backend's integration suites.
//!
//! `Cargo.toml` sets `autotests = false` and registers `tests/main.rs` as the
//! `main` target, so a suite is compiled only once it is declared here. Add
//! `mod <name>;` with the file or its tests never run.

mod attribute_vocabulary_export;
mod guard_answers;
