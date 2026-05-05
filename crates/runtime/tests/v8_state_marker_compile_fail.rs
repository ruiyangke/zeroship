//! Compile-fail snapshot tests for `#[v8_state_marker(...)]` diagnostics
//! (MAC-01 Phase 1 design `docs/proposals/macro-v8-state.md` §6.2).
//!
//! Each fixture under `tests/compile_fail_state_marker/` exercises one
//! malformed shape and pairs with a `.stderr` snapshot. trybuild
//! asserts the rustc / proc-macro output matches.
//!
//! Cases:
//!   - `marker_equals_state.rs`   — `#[v8_state_marker(Foo)] impl Foo`
//!     (marker == receiver — strict rejection per design §4.7)
//!   - `marker_path_qualified.rs` — `#[v8_state_marker(mod::path::Foo)]`
//!     (paths forbidden — must be a bare ident per design §4.2)
//!
//! Re-snapshot stderr by running with `TRYBUILD=overwrite` if the
//! diagnostic wording changes — the snapshots are NOT meant to be
//! hand-edited.

#[test]
fn state_marker_compile_fail_snapshots() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail_state_marker/marker_equals_state.rs");
    t.compile_fail("tests/compile_fail_state_marker/marker_path_qualified.rs");
}
