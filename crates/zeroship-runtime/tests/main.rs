//! The runtime single test target.
//!
//! `Cargo.toml` sets `autotests = false`: a `tests/<name>.rs` file is compiled
//! by nothing until it is declared, so every new suite is registered here or in
//! a tier module in the same change as its file. Shared fixtures live under
//! `tests/support/`; in-process public-API suites live under
//! `tests/integration/`. A suite that must run in its own process keeps its own
//! `[[test]]` target, with the reason recorded in `Cargo.toml`.

mod support;
mod integration;
