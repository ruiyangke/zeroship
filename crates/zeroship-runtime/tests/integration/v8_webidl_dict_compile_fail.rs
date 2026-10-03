//! Compile-fail snapshot tests for `#[derive(WebIdlDict)]`.
//!
//! Locks the derive-time rejection of reference-typed fields.
//! Earlier versions accepted `&str` (or any
//! `Type::Reference`) silently and the codegen panicked at
//! extraction time with a confusing "expected &str, found String"
//! error pointing at synthetic tokens the user did not write.
//!
//! Cases:
//!   - `reference_type.rs` — a `'a`-borrowing struct with `name:
//!     &'a str`. Derive errors at the field span with the supported
//!     owned-type alternatives.
//!
//! Re-snapshot stderr by running with `TRYBUILD=overwrite` if the
//! diagnostic wording changes.

#[test]
fn webidl_dict_compile_fail_snapshots() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail_webidl_dict/reference_type.rs");
}
