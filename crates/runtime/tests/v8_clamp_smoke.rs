//! Smoke tests for `[Clamp]` WebIDL integer coercion.
//!
//! Covered newtypes: `ClampU16`, `ClampU32`, `ClampI32`, `ClampU64`,
//! `ClampI64`. Each maps to the corresponding `read_clamp_*` reader in
//! `zeroship_runtime::clamp`, which implements the WebIDL ConvertToInt
//! `[Clamp]` algorithm:
//!
//!   1. NaN → 0.
//!   2. Below min → min.
//!   3. Above max → max.
//!   4. Otherwise round-half-even (banker's rounding) to the nearest
//!      integer.
//!
//! The macro's `gen_extract` recognises each newtype in argument
//! position and emits the reader call. There is NO TypeError path:
//! `[Clamp]` is the lenient counterpart to `[EnforceRange]`, which
//! throws on the same inputs.
//!
//! Coverage:
//!   - NaN → 0 (all five widths)
//!   - Below min → clamp to min
//!   - Above max → clamp to max (saturated at the type's max OR at
//!     `2^53 - 1` for the 64-bit widths, which is JS Number precision)
//!   - Round-half-even for fractional values (1.5 → 2, 2.5 → 2)
//!   - Negative values for signed widths (i32, i64)
//!   - Pass-through of an in-range integer
#![allow(unsafe_code)]

use zeroship_runtime::clamp::{ClampI32, ClampI64, ClampU16, ClampU32, ClampU64};
use zeroship_runtime::init_v8;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_method};

// ---------------------------------------------------------------------------
// Test harness — copy of the v8_class_smoke harness so this test stays
// self-contained.
// ---------------------------------------------------------------------------

fn run_in_v8<F, R>(
    install: impl FnOnce(&mut v8::PinScope, v8::Local<v8::Object>),
    src: &str,
    f: F,
) -> R
where
    F: FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
{
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    let global = scope.get_current_context().global(scope);
    install(scope, global);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    f(result, scope)
}

fn install_class<'s, T>(
    install_fn: fn(&mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::FunctionTemplate>,
    name: &str,
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let _ = std::marker::PhantomData::<T>;
    let tmpl = install_fn(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, name).unwrap();
    global.set(scope, key.into(), class_fn.into());
}

// ---------------------------------------------------------------------------
// Test class — one method per width. Each takes a Clamp-newtype arg and
// returns it as a wider, signed-but-larger integer or string so the JS
// caller can read the clamped value back without losing information.
// ---------------------------------------------------------------------------

mod clamp_class {
    use super::*;

    pub struct Clamper;

    #[v8_class]
    impl Clamper {
        #[v8_constructor]
        fn new() -> Clamper {
            Clamper
        }

        // Return as f64 so JS sees the exact integer value back without
        // worrying about i32/u32 overflow at the V8 boundary.
        #[v8_method]
        fn u16(&self, x: ClampU16) -> f64 {
            x.0 as f64
        }

        #[v8_method]
        fn u32(&self, x: ClampU32) -> f64 {
            x.0 as f64
        }

        #[v8_method]
        fn i32(&self, x: ClampI32) -> f64 {
            x.0 as f64
        }

        #[v8_method]
        fn u64(&self, x: ClampU64) -> f64 {
            x.0 as f64
        }

        #[v8_method]
        fn i64(&self, x: ClampI64) -> f64 {
            x.0 as f64
        }
    }
}

fn run_method(method: &str, arg: &str) -> f64 {
    let src = format!(
        r#"
        const c = new Clamper();
        c.{method}({arg});
        "#
    );
    run_in_v8(
        |scope, global| {
            install_class::<clamp_class::Clamper>(
                clamp_class::Clamper::install,
                "Clamper",
                scope,
                global,
            );
        },
        &src,
        |val, scope| val.number_value(scope).unwrap(),
    )
}

// ---------------------------------------------------------------------------
// NaN → 0 (all widths).
// ---------------------------------------------------------------------------

#[test]
fn nan_clamps_to_zero() {
    assert_eq!(run_method("u16", "NaN"), 0.0);
    assert_eq!(run_method("u32", "NaN"), 0.0);
    assert_eq!(run_method("i32", "NaN"), 0.0);
    assert_eq!(run_method("u64", "NaN"), 0.0);
    assert_eq!(run_method("i64", "NaN"), 0.0);
}

// ---------------------------------------------------------------------------
// Below min → min. Unsigned widths min at 0; signed widths min at
// (i32::MIN | -(2^53 - 1)).
// ---------------------------------------------------------------------------

#[test]
fn below_min_clamps_to_min() {
    // Unsigned: -1 → 0.
    assert_eq!(run_method("u16", "-1"), 0.0);
    assert_eq!(run_method("u32", "-1"), 0.0);
    assert_eq!(run_method("u64", "-1"), 0.0);
    // Signed i32: huge negative → i32::MIN.
    assert_eq!(run_method("i32", "-1e30"), i32::MIN as f64);
    // Signed i64: huge negative → -(2^53 - 1) (Number precision floor).
    let safe_min = -((1u64 << 53) as f64 - 1.0);
    assert_eq!(run_method("i64", "-1e30"), safe_min);
}

// ---------------------------------------------------------------------------
// Above max → max. The 64-bit widths cap at `2^53 - 1` (Number precision
// boundary), not the type's actual max — same rationale as the WebIDL
// `[EnforceRange]` boundary. The 32-bit widths cap at the type's max.
// ---------------------------------------------------------------------------

#[test]
fn above_max_clamps_to_max() {
    // u16: 65_536 → 65_535.
    assert_eq!(run_method("u16", "65536"), 65_535.0);
    // u16: 1e10 → 65_535.
    assert_eq!(run_method("u16", "1e10"), 65_535.0);
    // u32: 1e10 → u32::MAX.
    assert_eq!(run_method("u32", "1e10"), u32::MAX as f64);
    // i32: 1e10 → i32::MAX.
    assert_eq!(run_method("i32", "1e10"), i32::MAX as f64);
    // u64 / i64: 1e30 → 2^53 - 1.
    let safe_max = (1u64 << 53) as f64 - 1.0;
    assert_eq!(run_method("u64", "1e30"), safe_max);
    assert_eq!(run_method("i64", "1e30"), safe_max);
}

// ---------------------------------------------------------------------------
// Round-half-even (banker's rounding). 1.5 → 2, 2.5 → 2, 100.7 → 101.
// Per WebIDL §3.2.10 step 8, ties round to the even neighbour.
// ---------------------------------------------------------------------------

#[test]
fn round_half_even() {
    // 1.5 → 2 (1 is odd, 2 is even — round to even).
    assert_eq!(run_method("u16", "1.5"), 2.0);
    // 2.5 → 2 (2 is even — round to even).
    assert_eq!(run_method("u16", "2.5"), 2.0);
    // 3.5 → 4 (4 is even).
    assert_eq!(run_method("u16", "3.5"), 4.0);
    // 100.7 → 101 (no tie; standard round-up).
    assert_eq!(run_method("u16", "100.7"), 101.0);
    // 100.3 → 100 (no tie; round-down).
    assert_eq!(run_method("u16", "100.3"), 100.0);
    // -1.5 → -2 (i32 / i64). Banker's rounding rounds away from zero
    // here because 0 is even and -2 is even, but the spec uses
    // round-half-even which Rust `round_ties_even` implements.
    assert_eq!(run_method("i32", "-1.5"), -2.0);
    assert_eq!(run_method("i32", "-2.5"), -2.0);
}

// ---------------------------------------------------------------------------
// Pass-through of in-range integers. The reader emits the value
// unchanged (after any rounding the caller might have done in JS).
// ---------------------------------------------------------------------------

#[test]
fn in_range_passthrough() {
    assert_eq!(run_method("u16", "42"), 42.0);
    assert_eq!(run_method("u32", "100000"), 100_000.0);
    assert_eq!(run_method("i32", "-7"), -7.0);
    assert_eq!(run_method("u64", "1234567"), 1_234_567.0);
    assert_eq!(run_method("i64", "-1234567"), -1_234_567.0);
}
