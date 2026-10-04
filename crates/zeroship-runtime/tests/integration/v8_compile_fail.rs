//! Compile-fail snapshots for the V8 binding macros.
//!
//! The macros run from this downstream crate because a proc-macro crate cannot
//! depend on itself. Each fixture pairs with a `.stderr` snapshot beside it, so
//! a fixture that fails for any reason other than the rule it pins fails the
//! test. A raw `compile_fail` doctest passes on ANY compilation error and could
//! not tell the pinned rejection from a typo, a renamed macro or a stale import.
//!
//! Every fixture runs from one `TestCases`, and no other test in this crate may
//! construct one. trybuild keys its generated project by crate name and names
//! the fixture bins by index, and nextest runs each test in a process of its
//! own, where trybuild's in-process mutex cannot order them. Separate tests
//! here rewrote the shared project under one another mid-run: a check parsed a
//! half-written manifest, or compiled another test's fixture under its own bin
//! name. One `TestCases` is one writer, and it checks the dependency graph once
//! for every fixture.
//!
//! Re-snapshot with `TRYBUILD=overwrite` when diagnostic wording changes, and
//! read the diff before accepting it: an overwrite that changes WHICH error a
//! fixture pins defeats the fixture. The rustc-emitted diagnostics
//! (`missing_fn.rs`, `wrong_signature.rs`) can churn across toolchains; the
//! macro-emitted ones cannot.

#[test]
fn v8_binding_misuse_does_not_compile() {
    let cases = trybuild::TestCases::new();

    // `#[v8_method(fastcall)]`: the fast path has no scope, so `&mut self`
    // cannot carry the slow path's re-entrancy guard; it cannot allocate
    // (`String`, `Vec<u8>`); and it cannot represent `null` (`Option<T>`). These
    // fixtures differ only in that one shape, so the snapshots are what keep
    // one shared scaffolding error from satisfying them all.
    cases.compile_fail("tests/compile_fail_fastcall/v8_fastcall_mut_self.rs");
    cases.compile_fail("tests/compile_fail_fastcall/v8_fastcall_string_return.rs");
    cases.compile_fail("tests/compile_fail_fastcall/v8_fastcall_vec_return.rs");
    cases.compile_fail("tests/compile_fail_fastcall/v8_fastcall_option_return.rs");

    // `#[v8_async_method]`: a `&mut self` borrow held across `.await` is
    // unsound under V8 re-entry, and the attribute requires an `async fn`.
    cases.compile_fail("tests/compile_fail_async_method/v8_async_method_mut_self.rs");
    cases.compile_fail("tests/compile_fail_async_method/v8_async_method_not_async.rs");

    // The strict marker-attribute grammar: each malformed shape is a
    // span-pinned error rather than a silent `None`. `#[v8_name(foo)]` and
    // `#[v8_name = 42]`; `#[v8_to_string_tag(Foo)]` in list form; a non-string
    // and an unsupported `#[v8_inherit_intrinsic]` value; `#[v8_state_marker]`
    // bare and in name-value form; an unknown `#[v8_constructor(...)]` key.
    cases.compile_fail("tests/compile_fail_marker_attr/v8_name_bare_ident.rs");
    cases.compile_fail("tests/compile_fail_marker_attr/v8_name_non_string_lit.rs");
    cases.compile_fail("tests/compile_fail_marker_attr/v8_to_string_tag_bare_ident.rs");
    cases.compile_fail("tests/compile_fail_marker_attr/v8_inherit_intrinsic_non_string.rs");
    cases.compile_fail("tests/compile_fail_marker_attr/v8_inherit_intrinsic_bad_value.rs");
    cases.compile_fail("tests/compile_fail_marker_attr/v8_state_marker_missing_path.rs");
    cases.compile_fail("tests/compile_fail_marker_attr/v8_state_marker_non_path.rs");
    cases.compile_fail("tests/compile_fail_marker_attr/v8_constructor_unknown_arg.rs");

    // `#[v8_constructor(post_init = "...")]`: the named hook does not exist;
    // its signature is not `(scope, this) -> Result<(), OpError>`; the value is
    // not a string literal; the string is not a Rust identifier.
    cases.compile_fail("tests/compile_fail_post_init/missing_fn.rs");
    cases.compile_fail("tests/compile_fail_post_init/wrong_signature.rs");
    cases.compile_fail("tests/compile_fail_post_init/non_string_value.rs");
    cases.compile_fail("tests/compile_fail_post_init/bad_identifier.rs");

    // `#[v8_state_marker(...)]`: the marker may not be the receiver type
    // itself, and it must be a bare identifier rather than a path.
    cases.compile_fail("tests/compile_fail_state_marker/marker_equals_state.rs");
    cases.compile_fail("tests/compile_fail_state_marker/marker_path_qualified.rs");

    // `#[derive(WebIdlDict)]`: a reference-typed field is rejected at its own
    // span, naming the owned alternatives.
    cases.compile_fail("tests/compile_fail_webidl_dict/reference_type.rs");
}
