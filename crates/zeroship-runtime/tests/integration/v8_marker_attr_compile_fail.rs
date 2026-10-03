//! Compile-fail snapshot tests for the strict `MarkerAttr` parsers.
//!
//! Earlier versions used impl-block / per-method `extract_*` helpers
//! that silently fell back to `None` on malformed shape. Closing
//! finding H5 / H6 of
//! `runtime-macros-architecture-critique-2026-05-05.md` made each
//! `MarkerAttr::merge` strict — return `Err` (and thus emit a span-
//! pinned `compile_error!`) on shapes that don't match the documented
//! attribute grammar.
//!
//! Each fixture under `tests/compile_fail_marker_attr/` triggers one
//! such error and pairs with a `.stderr` snapshot. trybuild asserts
//! the rustc / proc-macro output matches.
//!
//! Cases:
//!   - `v8_name_bare_ident.rs`             — `#[v8_name(foo)]` (list
//!     form, missing `=` sign — earlier versions silently accepted it as `None`)
//!   - `v8_name_non_string_lit.rs`         — `#[v8_name = 42]` (non-
//!     string literal value)
//!   - `v8_to_string_tag_bare_ident.rs`    — `#[v8_to_string_tag]` /
//!     `#[v8_to_string_tag(Foo)]` (missing `=` shape)
//!   - `v8_inherit_intrinsic_non_string.rs` — `#[v8_inherit_intrinsic
//!     = 42]` (non-string literal value)
//!   - `v8_inherit_intrinsic_bad_value.rs`  — `#[v8_inherit_intrinsic
//!     = "ArrayPrototype"]` (recognised SHAPE but unsupported VALUE;
//!     the diagnostic now comes from the analyze phase rather than the
//!     generated install body)
//!   - `v8_state_marker_missing_path.rs`   — `#[v8_state_marker]`
//!     (bare attribute, no parenthesised marker)
//!   - `v8_state_marker_non_path.rs`       — `#[v8_state_marker = "M"]`
//!     (NameValue shape — must be list form)
//!   - `v8_constructor_unknown_arg.rs`     — `#[v8_constructor(foo =
//!     "bar")]` (unrecognised NameValue inside the constructor list)
//!
//! Re-snapshot stderr by running with `TRYBUILD=overwrite` if the
//! diagnostic wording changes.

#[test]
fn marker_attr_compile_fail_snapshots() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail_marker_attr/v8_name_bare_ident.rs");
    t.compile_fail("tests/compile_fail_marker_attr/v8_name_non_string_lit.rs");
    t.compile_fail("tests/compile_fail_marker_attr/v8_to_string_tag_bare_ident.rs");
    t.compile_fail("tests/compile_fail_marker_attr/v8_inherit_intrinsic_non_string.rs");
    t.compile_fail("tests/compile_fail_marker_attr/v8_inherit_intrinsic_bad_value.rs");
    t.compile_fail("tests/compile_fail_marker_attr/v8_state_marker_missing_path.rs");
    t.compile_fail("tests/compile_fail_marker_attr/v8_state_marker_non_path.rs");
    t.compile_fail("tests/compile_fail_marker_attr/v8_constructor_unknown_arg.rs");
}
