//! Compile-fail snapshot tests for the `#[v8_method(fastcall)]` signature
//! rejection rules.
//!
//! Fastcall codegen restricts the user's signature to fit V8's fast API
//! constraints. The macro rejects unsupported shapes at the right span so
//! mistakes surface as clear messages rather than runtime UB.
//!
//! # Why trybuild rather than a ```compile_fail doctest
//!
//! These four rules used to be pinned by raw `compile_fail` doctests in
//! `src/lib.rs`. A raw `compile_fail` passes on ANY compilation error, so
//! all four could have been failing for one shared unrelated reason and
//! still read as four green checks.
//!
//! That risk is concrete here rather than theoretical: the four fixtures
//! differ ONLY in return type, so a single error affecting the common
//! scaffolding would satisfy every one of them. The `.stderr` snapshots
//! are what force each fixture to fail on its own return type.
//!
//! Cases:
//!   - `v8_fastcall_mut_self.rs`       - `&mut self` (no scope on the fast
//!     path, so the slow path's re-entrancy guard cannot be emitted)
//!   - `v8_fastcall_string_return.rs`  - `String` return (fast path forbids
//!     allocation)
//!   - `v8_fastcall_vec_return.rs`     - `Vec<u8>` return (same rule)
//!   - `v8_fastcall_option_return.rs`  - `Option<T>` return (fast path
//!     cannot represent `null`)
//!
//! Re-snapshot stderr by running with `TRYBUILD=overwrite` if the
//! diagnostic wording changes. Read the diff before accepting it: an
//! overwrite that silently changes WHICH error is pinned defeats the
//! reason these are fixtures.

#[test]
fn fastcall_compile_fail_snapshots() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail_fastcall/v8_fastcall_mut_self.rs");
    t.compile_fail("tests/compile_fail_fastcall/v8_fastcall_string_return.rs");
    t.compile_fail("tests/compile_fail_fastcall/v8_fastcall_vec_return.rs");
    t.compile_fail("tests/compile_fail_fastcall/v8_fastcall_option_return.rs");
}
