//! Smoke tests for the WebIDL default-case integer coercion newtypes
//! `WrapU8` / `WrapU16` / `WrapU32` / `WrapI8` / `WrapI16` / `WrapI32`.
//!
//! Per WebIDL §3.2.10 ConvertToInt without `[Clamp]` or `[EnforceRange]`:
//!
//!   1. Let `x = ToNumber(V)`.
//!   2. If `x` is `NaN`, `+0`, `-0`, `+Infinity`, or `-Infinity`,
//!      return 0.
//!   3. Otherwise truncate `x` toward zero to integer `i`.
//!   4. Apply modulo 2^N (where N = bit width of the target type) so
//!      the result fits the target representation.
//!   5. Reinterpret as signed or unsigned per the target type.
//!
//! The macro recognises `Wrap{U8,U16,U32,I8,I16,I32}` in argument
//! position via the `wrap_kind` helper and emits the matching
//! `read_wrap_*` reader (which performs the spec algorithm) before the
//! method body runs. Newtypes mirror the existing `Clamp{*}` /
//! `EnforceRange{U32,U64}` family.
//!
//! Coverage:
//!   - NaN -> 0
//!   - Whole number in range passes through unchanged
//!   - Out-of-range positive -> modulo 2^N
//!   - Out-of-range negative -> modulo 2^N (signed wrap-around)
//!   - Infinity -> 0 (per step 2 of the spec)
//!   - Truncation toward zero (3.7 -> 3, -3.7 -> -3)
#![allow(unsafe_code)]

use zeroship_runtime::convert::{WrapI16, WrapI32, WrapI8, WrapU16, WrapU32, WrapU8};
use zeroship_runtime::init_v8;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_method};

// ---------------------------------------------------------------------------
// Test harness — local copy.
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
// Test class — one method per width. Each takes a Wrap-newtype arg and
// returns f64 so JS sees the converted value back.
// ---------------------------------------------------------------------------

mod wrap_class {
    use super::*;

    pub struct Wrapper;

    #[v8_class]
    impl Wrapper {
        #[v8_constructor]
        fn new() -> Wrapper {
            Wrapper
        }

        #[v8_method]
        fn u8_(&self, x: WrapU8) -> f64 {
            x.0 as f64
        }

        #[v8_method]
        fn u16_(&self, x: WrapU16) -> f64 {
            x.0 as f64
        }

        #[v8_method]
        fn u32_(&self, x: WrapU32) -> f64 {
            x.0 as f64
        }

        #[v8_method]
        fn i8_(&self, x: WrapI8) -> f64 {
            x.0 as f64
        }

        #[v8_method]
        fn i16_(&self, x: WrapI16) -> f64 {
            x.0 as f64
        }

        #[v8_method]
        fn i32_(&self, x: WrapI32) -> f64 {
            x.0 as f64
        }
    }
}

fn run_method(method: &str, arg: &str) -> f64 {
    let src = format!(
        r#"
        const w = new Wrapper();
        w.{method}({arg});
        "#
    );
    run_in_v8(
        |scope, global| {
            install_class::<wrap_class::Wrapper>(
                wrap_class::Wrapper::install,
                "Wrapper",
                scope,
                global,
            );
        },
        &src,
        |val, scope| val.number_value(scope).unwrap(),
    )
}

// ---------------------------------------------------------------------------
// NaN -> 0 (all widths).
// ---------------------------------------------------------------------------

#[test]
fn nan_wraps_to_zero() {
    assert_eq!(run_method("u8_", "NaN"), 0.0);
    assert_eq!(run_method("u16_", "NaN"), 0.0);
    assert_eq!(run_method("u32_", "NaN"), 0.0);
    assert_eq!(run_method("i8_", "NaN"), 0.0);
    assert_eq!(run_method("i16_", "NaN"), 0.0);
    assert_eq!(run_method("i32_", "NaN"), 0.0);
}

// ---------------------------------------------------------------------------
// Infinity -> 0 (all widths). Per WebIDL §3.2.10 step 2.
// ---------------------------------------------------------------------------

#[test]
fn infinity_wraps_to_zero() {
    assert_eq!(run_method("u8_", "Infinity"), 0.0);
    assert_eq!(run_method("u16_", "Infinity"), 0.0);
    assert_eq!(run_method("u32_", "Infinity"), 0.0);
    assert_eq!(run_method("u32_", "-Infinity"), 0.0);
    assert_eq!(run_method("i32_", "Infinity"), 0.0);
    assert_eq!(run_method("i32_", "-Infinity"), 0.0);
}

// ---------------------------------------------------------------------------
// Whole numbers in range pass through unchanged.
// ---------------------------------------------------------------------------

#[test]
fn whole_in_range_passes_through() {
    assert_eq!(run_method("u8_", "200"), 200.0);
    assert_eq!(run_method("u16_", "60000"), 60000.0);
    assert_eq!(run_method("u32_", "4000000000"), 4000000000.0);
    assert_eq!(run_method("i8_", "100"), 100.0);
    assert_eq!(run_method("i8_", "-100"), -100.0);
    assert_eq!(run_method("i16_", "30000"), 30000.0);
    assert_eq!(run_method("i16_", "-30000"), -30000.0);
    assert_eq!(run_method("i32_", "2000000000"), 2000000000.0);
    assert_eq!(run_method("i32_", "-2000000000"), -2000000000.0);
}

// ---------------------------------------------------------------------------
// Out-of-range positive -> modulo 2^N.
// ---------------------------------------------------------------------------

#[test]
fn above_max_wraps_modulo() {
    // u8: 256 -> 0, 257 -> 1, 65537 -> 1.
    assert_eq!(run_method("u8_", "256"), 0.0);
    assert_eq!(run_method("u8_", "257"), 1.0);
    assert_eq!(run_method("u8_", "65537"), 1.0);
    // u16: 65536 -> 0, 65537 -> 1.
    assert_eq!(run_method("u16_", "65536"), 0.0);
    assert_eq!(run_method("u16_", "65537"), 1.0);
    // u32: 2^32 -> 0, 2^32 + 5 -> 5.
    assert_eq!(run_method("u32_", "4294967296"), 0.0);
    assert_eq!(run_method("u32_", "4294967301"), 5.0);
    // i8: 128 -> -128 (sign flip due to modulo + signed reinterpret).
    assert_eq!(run_method("i8_", "128"), -128.0);
    assert_eq!(run_method("i8_", "200"), -56.0);
    // i16: 32768 -> -32768.
    assert_eq!(run_method("i16_", "32768"), -32768.0);
    // i32: 2^31 -> -2^31.
    assert_eq!(run_method("i32_", "2147483648"), -2147483648.0);
}

// ---------------------------------------------------------------------------
// Out-of-range negative -> modulo wrap to high half of unsigned, or
// negative reinterpret for signed.
// ---------------------------------------------------------------------------

#[test]
fn below_zero_wraps_for_unsigned() {
    // u8: -1 -> 255.
    assert_eq!(run_method("u8_", "-1"), 255.0);
    // u16: -1 -> 65535.
    assert_eq!(run_method("u16_", "-1"), 65535.0);
    // u32: -1 -> 2^32 - 1.
    assert_eq!(run_method("u32_", "-1"), 4294967295.0);
    // u8: -300 -> 256 - (300 mod 256) = 256 - 44 = 212.
    // Computed: -300 mod 256 = -44 -> 256 + (-44) = 212.
    assert_eq!(run_method("u8_", "-300"), (-300i32).rem_euclid(256) as f64);
}

#[test]
fn below_min_wraps_for_signed() {
    // i8: -129 -> 127.
    assert_eq!(run_method("i8_", "-129"), 127.0);
    // i16: -32769 -> 32767.
    assert_eq!(run_method("i16_", "-32769"), 32767.0);
    // i32: -2^31 - 1 -> 2^31 - 1.
    assert_eq!(run_method("i32_", "-2147483649"), 2147483647.0);
}

// ---------------------------------------------------------------------------
// Truncation toward zero. WebIDL §3.2.10 step 3 says "truncate the
// fractional part toward 0".
// ---------------------------------------------------------------------------

#[test]
fn truncates_toward_zero() {
    // 3.7 -> 3, 3.2 -> 3.
    assert_eq!(run_method("u8_", "3.7"), 3.0);
    assert_eq!(run_method("u8_", "3.2"), 3.0);
    // -3.7 -> -3 (toward zero, not floor).
    assert_eq!(run_method("i8_", "-3.7"), -3.0);
    assert_eq!(run_method("i8_", "-3.2"), -3.0);
    // 100.999 -> 100.
    assert_eq!(run_method("u32_", "100.999"), 100.0);
}
