//! Compile-fail snapshot tests for `#[v8_constructor(post_init = ...)]`
//! diagnostics.
//!
//! Each fixture under `tests/compile_fail_post_init/` exercises one
//! malformed shape and pairs with a `.stderr` snapshot. trybuild
//! asserts the rustc / proc-macro output matches.
//!
//! Cases:
//!   - `missing_fn.rs`        — post_init names a fn that doesn't exist
//!     (rustc-emitted "no function or associated item" — design §5.7
//!     case 1)
//!   - `wrong_signature.rs`   — hook signature doesn't match
//!     `(scope, this) -> Result<(), OpError>` (design §5.7 case 2)
//!   - `non_string_value.rs`  — `post_init = ident` (must be string lit;
//!     macro-emitted compile_error per design §5.7 case 3)
//!   - `bad_identifier.rs`    — `post_init = "1bad"` (not a valid Rust
//!     ident; macro-emitted compile_error per design §5.7 case 4)
//!
//! Re-snapshot stderr by running with `TRYBUILD=overwrite` if the
//! diagnostic wording changes — the snapshots are NOT meant to be
//! hand-edited.
//!
//! Note: rustc diagnostics evolve across versions. Snapshots are
//! generated against the toolchain version at landing; if a future
//! rustc upgrade churns the wording, run with `TRYBUILD=overwrite`
//! to refresh. Macro-emitted messages (the two `compile_error!`-
//! based cases) are toolchain-stable.

#[test]
fn post_init_compile_fail_snapshots() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail_post_init/missing_fn.rs");
    t.compile_fail("tests/compile_fail_post_init/wrong_signature.rs");
    t.compile_fail("tests/compile_fail_post_init/non_string_value.rs");
    t.compile_fail("tests/compile_fail_post_init/bad_identifier.rs");
}
