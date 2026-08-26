//! WebIDL default-case integer coercion newtypes — `Wrap{U8,U16,U32,
//! I8,I16,I32}`.
//!
//! Per WebIDL §3.2.10 ConvertToInt without `[Clamp]` or `[EnforceRange]`
//! (the implicit default case):
//!
//!   1. Let `x = ToNumber(V)`.
//!   2. If `x` is `NaN`, `+0`, `-0`, `+Infinity`, or `-Infinity`, return 0.
//!   3. Let `i = sign(x) * floor(abs(x))` — truncate toward zero.
//!   4. Apply modulo 2^N where N is the bit width of the target.
//!   5. Reinterpret as signed-or-unsigned per the target type.
//!
//! These are the LENIENT default case — `[Clamp]` saturates and
//! `[EnforceRange]` throws, but the bare `unsigned short` etc. types
//! wrap silently. CloseEvent.code (`unsigned short`) is the canonical
//! consumer; the existing `convert_unsigned_short_modulo` hand-rolled
//! 30 LOC is the same algorithm. Future spec sections that take
//! default-case integer parameters get the surface for free.
//!
//! The `#[v8_class]` macro recognises these newtypes in argument
//! position via the `wrap_kind` helper and emits a call to the
//! matching `read_wrap_*` reader, mirroring the `clamp_kind` pattern.

/// WebIDL `unsigned short` (default case). Result `u8` is the low 8
/// bits of the truncated `f64` modulo 256, NaN/Infinity -> 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapU8(pub u8);

/// WebIDL `unsigned short` (default case). Result `u16` is the low 16
/// bits of the truncated `f64` modulo 65536, NaN/Infinity -> 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapU16(pub u16);

/// WebIDL `unsigned long` (default case) — modulo 2^32, NaN/Infinity -> 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapU32(pub u32);

/// WebIDL `byte` (default case) — modulo 2^8 reinterpreted as signed
/// (`u8 as i8`). NaN/Infinity -> 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapI8(pub i8);

/// WebIDL `short` (default case) — modulo 2^16 reinterpreted as
/// signed. NaN/Infinity -> 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapI16(pub i16);

/// WebIDL `long` (default case) — modulo 2^32 reinterpreted as
/// signed. NaN/Infinity -> 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapI32(pub i32);

impl From<WrapU8>  for u8  { fn from(v: WrapU8)  -> u8  { v.0 } }
impl From<WrapU16> for u16 { fn from(v: WrapU16) -> u16 { v.0 } }
impl From<WrapU32> for u32 { fn from(v: WrapU32) -> u32 { v.0 } }
impl From<WrapI8>  for i8  { fn from(v: WrapI8)  -> i8  { v.0 } }
impl From<WrapI16> for i16 { fn from(v: WrapI16) -> i16 { v.0 } }
impl From<WrapI32> for i32 { fn from(v: WrapI32) -> i32 { v.0 } }

/// Implement steps 1-5 of WebIDL §3.2.10 ConvertToInt for an unsigned
/// width N <= 32. `n` is the number of bits, `mask = 2^n`.
///
/// Returns the wrapped integer. Pulled out as a helper so each width's
/// reader is a 4-line wrapper.
fn convert_to_unsigned_modulo(value: f64, mask: u64) -> u64 {
    // Step 2: NaN / ±Infinity / ±0 -> 0.
    if !value.is_finite() {
        return 0;
    }
    if value == 0.0 {
        return 0;
    }
    // Step 3: truncate toward zero. Rust's `f64 as i64` already
    // truncates toward zero for finite values; we go through i64 so
    // negative values can be brought into the unsigned range via
    // `rem_euclid` below.
    //
    // We bound the cast: if `|value|` exceeds the i64 representable
    // range, the cast is implementation-defined (Rust's `as` is
    // saturating to the i64 bounds, which would make the modulo step
    // observably wrong). For the spec's ConvertToInt path, the
    // conventional answer is: reduce `value` modulo 2^N first via
    // f64 arithmetic, *then* cast. f64 has 53 bits of precision so
    // reductions of values up to ~2^53 lossless; beyond that the
    // result is implementation-defined per spec (V8 itself diverges).
    //
    // The simple-and-correct path: take `value % (mask as f64)` first
    // (truncating toward zero implicitly via fmod-style remainder),
    // then cast to i64. f64's `%` is fmod-like (truncation toward
    // zero), so the sign of the result matches `value`'s.
    let mask_f = mask as f64;
    // Truncate toward zero, THEN apply modulo, per spec step 3 -> 4.
    let truncated = value.trunc();
    let reduced = truncated.rem_euclid(mask_f);
    // `reduced` is in [0, mask_f). The cast to u64 is exact for
    // values up to mask <= 2^32 (which we always have here).
    reduced as u64
}

/// `unsigned short` default — modulo 2^8.
pub fn read_wrap_u8(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> WrapU8 {
    let n = val.number_value(scope).unwrap_or(0.0);
    let r = convert_to_unsigned_modulo(n, 1u64 << 8);
    WrapU8(r as u8)
}

/// `unsigned short` default — modulo 2^16.
pub fn read_wrap_u16(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> WrapU16 {
    let n = val.number_value(scope).unwrap_or(0.0);
    let r = convert_to_unsigned_modulo(n, 1u64 << 16);
    WrapU16(r as u16)
}

/// `unsigned long` default — modulo 2^32.
pub fn read_wrap_u32(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> WrapU32 {
    let n = val.number_value(scope).unwrap_or(0.0);
    let r = convert_to_unsigned_modulo(n, 1u64 << 32);
    WrapU32(r as u32)
}

/// `byte` default — modulo 2^8 reinterpreted as signed.
pub fn read_wrap_i8(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> WrapI8 {
    let n = val.number_value(scope).unwrap_or(0.0);
    let r = convert_to_unsigned_modulo(n, 1u64 << 8);
    WrapI8(r as u8 as i8)
}

/// `short` default — modulo 2^16 reinterpreted as signed.
pub fn read_wrap_i16(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> WrapI16 {
    let n = val.number_value(scope).unwrap_or(0.0);
    let r = convert_to_unsigned_modulo(n, 1u64 << 16);
    WrapI16(r as u16 as i16)
}

/// `long` default — modulo 2^32 reinterpreted as signed.
pub fn read_wrap_i32(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> WrapI32 {
    let n = val.number_value(scope).unwrap_or(0.0);
    let r = convert_to_unsigned_modulo(n, 1u64 << 32);
    WrapI32(r as u32 as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modulo_helper_handles_nan_and_infinity() {
        assert_eq!(convert_to_unsigned_modulo(f64::NAN, 256), 0);
        assert_eq!(convert_to_unsigned_modulo(f64::INFINITY, 256), 0);
        assert_eq!(convert_to_unsigned_modulo(f64::NEG_INFINITY, 256), 0);
    }

    #[test]
    fn modulo_helper_handles_negatives() {
        // -1 mod 256 = 255 per Rust's rem_euclid (canonical answer).
        assert_eq!(convert_to_unsigned_modulo(-1.0, 256), 255);
        assert_eq!(convert_to_unsigned_modulo(-300.0, 256), 212);
    }

    #[test]
    fn modulo_helper_handles_overflow() {
        // 256 -> 0; 257 -> 1; 65536 -> 0 mod 256 (=0).
        assert_eq!(convert_to_unsigned_modulo(256.0, 256), 0);
        assert_eq!(convert_to_unsigned_modulo(257.0, 256), 1);
        assert_eq!(convert_to_unsigned_modulo(65537.0, 256), 1);
    }

    #[test]
    fn modulo_helper_truncates_fractions_toward_zero() {
        assert_eq!(convert_to_unsigned_modulo(3.7, 256), 3);
        assert_eq!(convert_to_unsigned_modulo(-3.7, 256), 256 - 3);
    }
}
