//! Compile-fail snapshot tests for the `#[v8_async_method]` rejection rules.
//!
//! These live at the runtime crate level (not on the proc macro itself)
//! because proc-macro crates can't depend on themselves, so tests that
//! exercise the macro must run from a downstream crate.
//!
//! # Why trybuild rather than a ```compile_fail doctest
//!
//! Both rules below used to be pinned by raw `compile_fail` doctests in
//! `src/lib.rs`. A raw `compile_fail` passes on ANY compilation error,
//! so it cannot distinguish the rejection it means to pin from a typo, a
//! renamed macro, or a stale import - the pin keeps passing while the
//! property goes untested, and reads as coverage the whole time.
//!
//! trybuild asserts the emitted diagnostic MATCHES the `.stderr`
//! snapshot beside each fixture. A different error is a different
//! snapshot, so failing for the wrong reason is a test failure.
//!
//! Cases:
//!   - `v8_async_method_mut_self.rs`  - `&mut self` (borrow across
//!     `.await` is unsound under V8 re-entry)
//!   - `v8_async_method_not_async.rs` - the attribute on a non-`async` fn
//!
//! Re-snapshot stderr by running with `TRYBUILD=overwrite` if the
//! diagnostic wording changes. Read the diff before accepting it: an
//! overwrite that silently changes WHICH error is pinned defeats the
//! reason these are fixtures.

#[test]
fn async_method_compile_fail_snapshots() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail_async_method/v8_async_method_mut_self.rs");
    t.compile_fail("tests/compile_fail_async_method/v8_async_method_not_async.rs");
}
