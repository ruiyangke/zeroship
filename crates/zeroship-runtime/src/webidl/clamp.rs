//! WebIDL `[Clamp]` integer coercion newtypes.
//!
//! Per WebIDL §3.2.10 step 8 (https://webidl.spec.whatwg.org/#abstract-opdef-converttoint)
//! when `[Clamp]` is set:
//!
//! 1. Let `x = ToNumber(V)`.
//! 2. If `x` is `NaN`, return 0.
//! 3. Compare `x` to `min` / `max` of the target integer type:
//!    - if `x < min`, return `min`.
//!    - if `x > max`, return `max`.
//! 4. Otherwise, round to the nearest integer using **round-half-even**
//!    (banker's rounding — Rust's `f64::round_ties_even`).
//!
//! Per the spec there is NO TypeError path: `[Clamp]` is the lenient
//! counterpart to `[EnforceRange]`. Used by Streams' chunk-size strategies
//! (`HighWaterMark` is `[Clamp] unsigned long`), Blob.slice (`[Clamp]
//! long long`), and the WebSocket close code (`[Clamp] unsigned short`).
//!
//! The `#[v8_class]` macro recognises these newtypes in argument position
//! and emits a call to the matching `read_clamp_*` function which performs
//! the conversion at the WebIDL boundary, before the method body runs.

/// WebIDL `[Clamp] unsigned short` — value in `[0, 65_535]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClampU16(pub u16);

/// WebIDL `[Clamp] unsigned long` — value in `[0, 2^32 - 1]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClampU32(pub u32);

/// WebIDL `[Clamp] long` — value in `[i32::MIN, i32::MAX]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClampI32(pub i32);

/// WebIDL `[Clamp] unsigned long long` — value in `[0, 2^53 - 1]`. Above
/// `Number.MAX_SAFE_INTEGER` JS Number precision is lost; `[Clamp]` clamps
/// at the safe boundary rather than throwing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClampU64(pub u64);

/// WebIDL `[Clamp] long long` — value in `[-(2^53 - 1), 2^53 - 1]` for
/// the same JS Number precision reason as [`ClampU64`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClampI64(pub i64);

impl From<ClampU16> for u16 { fn from(v: ClampU16) -> u16 { v.0 } }
impl From<ClampU32> for u32 { fn from(v: ClampU32) -> u32 { v.0 } }
impl From<ClampI32> for i32 { fn from(v: ClampI32) -> i32 { v.0 } }
impl From<ClampU64> for u64 { fn from(v: ClampU64) -> u64 { v.0 } }
impl From<ClampI64> for i64 { fn from(v: ClampI64) -> i64 { v.0 } }

/// JS Number cannot represent integers above 2^53 - 1 exactly. `[Clamp]
/// long long` and `[Clamp] unsigned long long` clamp at this boundary
/// rather than at 2^63 - 1 / 2^64 - 1, because higher values cannot
/// round-trip through Number without loss.
const SAFE_INT_MAX: f64 = (1u64 << 53) as f64 - 1.0;
const SAFE_INT_MIN: f64 = -((1u64 << 53) as f64 - 1.0);

/// `[Clamp] unsigned short`. Clamp to `[0, 65_535]`, NaN → 0,
/// round-half-even.
pub fn read_clamp_u16(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> u16 {
    let n = match val.number_value(scope) {
        Some(n) => n,
        // ToNumber threw (BigInt, Symbol). The spec for `[Clamp]` doesn't
        // throw on out-of-range, but ToNumber predecessors (BigInt,
        // Symbol) DO throw — V8's `number_value` returns None and leaves
        // the pending exception set. The caller (macro) detects via the
        // pending exception and propagates; we surface 0 here so the
        // emitted code path is uniform. In practice the macro reads the
        // pending exception flag separately.
        None => return 0,
    };
    if n.is_nan() {
        return 0;
    }
    if n <= 0.0 {
        return 0;
    }
    if n >= u16::MAX as f64 {
        return u16::MAX;
    }
    n.round_ties_even() as u16
}

/// `[Clamp] unsigned long`. Clamp to `[0, 2^32 - 1]`, NaN → 0,
/// round-half-even.
pub fn read_clamp_u32(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> u32 {
    let n = match val.number_value(scope) {
        Some(n) => n,
        None => return 0,
    };
    if n.is_nan() {
        return 0;
    }
    if n <= 0.0 {
        return 0;
    }
    if n >= u32::MAX as f64 {
        return u32::MAX;
    }
    n.round_ties_even() as u32
}

/// `[Clamp] long`. Clamp to `[i32::MIN, i32::MAX]`, NaN → 0,
/// round-half-even.
pub fn read_clamp_i32(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> i32 {
    let n = match val.number_value(scope) {
        Some(n) => n,
        None => return 0,
    };
    if n.is_nan() {
        return 0;
    }
    if n <= i32::MIN as f64 {
        return i32::MIN;
    }
    if n >= i32::MAX as f64 {
        return i32::MAX;
    }
    n.round_ties_even() as i32
}

/// `[Clamp] unsigned long long`. Clamp to `[0, 2^53 - 1]`. Higher values
/// are clamped to `2^53 - 1` (Number precision boundary), not `2^64 - 1`.
/// NaN → 0, round-half-even.
pub fn read_clamp_u64(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> u64 {
    let n = match val.number_value(scope) {
        Some(n) => n,
        None => return 0,
    };
    if n.is_nan() {
        return 0;
    }
    if n <= 0.0 {
        return 0;
    }
    if n >= SAFE_INT_MAX {
        return SAFE_INT_MAX as u64;
    }
    n.round_ties_even() as u64
}

/// `[Clamp] long long`. Clamp to `[-(2^53 - 1), 2^53 - 1]` (Number
/// precision boundary on both sides). NaN → 0, round-half-even.
pub fn read_clamp_i64(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> i64 {
    let n = match val.number_value(scope) {
        Some(n) => n,
        None => return 0,
    };
    if n.is_nan() {
        return 0;
    }
    if n <= SAFE_INT_MIN {
        return SAFE_INT_MIN as i64;
    }
    if n >= SAFE_INT_MAX {
        return SAFE_INT_MAX as i64;
    }
    n.round_ties_even() as i64
}

#[cfg(test)]
mod tests {
    // Unit tests for the clamping math; the smoke test in
    // `tests/v8_clamp_smoke.rs` covers the macro wiring end-to-end.
    use super::*;

    #[test]
    fn round_half_even_is_bankers() {
        // 0.5 → 0, 1.5 → 2, 2.5 → 2, 3.5 → 4. Standard banker's
        // rounding pattern matches WebIDL ConvertToInt step 8.
        assert_eq!((0.5_f64).round_ties_even(), 0.0);
        assert_eq!((1.5_f64).round_ties_even(), 2.0);
        assert_eq!((2.5_f64).round_ties_even(), 2.0);
        assert_eq!((3.5_f64).round_ties_even(), 4.0);
        assert_eq!((-0.5_f64).round_ties_even(), 0.0);
        assert_eq!((-1.5_f64).round_ties_even(), -2.0);
        assert_eq!((-2.5_f64).round_ties_even(), -2.0);
    }
}
