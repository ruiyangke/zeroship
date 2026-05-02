//! WebIDL `[EnforceRange] unsigned long long` newtype.
//!
//! Per WebIDL §3.2.10 step 5 (https://webidl.spec.whatwg.org/#abstract-opdef-converttoint):
//!
//! 1. Let `x = ToNumber(V)`. If `x` is `NaN`, `+0`, `-0`, `+Infinity`, or
//!    `-Infinity`, return 0 — but with `[EnforceRange]` set, `NaN` and the
//!    infinities throw `TypeError` instead.
//! 2. With `[EnforceRange]`: if `x < 0` or `x > 2^64 - 1` (for `unsigned long
//!    long`), throw `TypeError`.
//! 3. Otherwise integer-truncate towards zero and return the value mod 2^64.
//!
//! Streams uses this for two arguments per the design (§XIV.8):
//! - `ReadableStreamBYOBReaderReadOptions.min` (default 1, must be ≥ 1)
//! - `ReadableStreamBYOBRequest.respond(bytesWritten)`
//!
//! WPT `readable-byte-streams/read-min.any.js` tests
//! `Number.MAX_SAFE_INTEGER + 1` (i.e. 2^53), which JS `Number` cannot
//! represent precisely. The spec answer is to reject anything that would
//! lose precision through `Number → BigInt` round-trip; in the JS Number
//! sense, this is values strictly above `2^53 - 1` (the precision boundary
//! for integers).
//!
//! The `#[v8_class]` macro recognises `EnforceRangeU64` in argument
//! position and emits a call to [`read_enforce_range_u64`] which throws
//! `TypeError` for out-of-range and non-finite inputs at the WebIDL
//! boundary, before the method body runs.

use crate::state::OpError;

/// WebIDL `[EnforceRange] unsigned long long`. The integer value is
/// guaranteed to be in `[0, 2^53 - 1]` (JS Number precision).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnforceRangeU64(pub u64);

impl EnforceRangeU64 {
    /// Construct from a raw `u64`. Bypasses range checks; intended for
    /// internal callers that built the value from a non-JS source.
    pub fn new(v: u64) -> Self {
        EnforceRangeU64(v)
    }
}

impl From<EnforceRangeU64> for u64 {
    fn from(v: EnforceRangeU64) -> Self {
        v.0
    }
}

/// Read a JS value as `[EnforceRange] unsigned long long`.
///
/// Returns `Err(TypeError)` for:
/// - `NaN`, `+Infinity`, `-Infinity`
/// - negative finite numbers
/// - finite numbers strictly above `2^53 - 1` (Number precision limit)
///
/// Otherwise truncates the fractional part towards zero (per spec
/// `IntegerPart`) and returns the resulting `u64`.
pub fn read_enforce_range_u64(
    scope: &mut v8::PinScope,
    val: v8::Local<v8::Value>,
) -> Result<EnforceRangeU64, OpError> {
    // Per WebIDL §3.2.10 step 1: ToNumber(V). V8's `Value::number_value`
    // performs ToNumber and returns None only if ToNumber threw (e.g.
    // BigInt, Symbol). In that case the V8 pending exception is set;
    // we surface a fresh TypeError so the macro re-throws cleanly.
    let n = match val.number_value(scope) {
        Some(n) => n,
        None => {
            return Err(OpError::type_error(
                "[EnforceRange] unsigned long long: failed to convert to number",
            ));
        }
    };

    // Step 2 (with [EnforceRange]): NaN / ±∞ → TypeError.
    if !n.is_finite() {
        return Err(OpError::type_error(
            "[EnforceRange] unsigned long long: value is not a finite number",
        ));
    }

    // Step 3: x = IntegerPart(n). For unsigned long long, the range is
    // [0, 2^64 - 1]. With [EnforceRange] we reject negatives and overflow.
    //
    // JS Number precision boundary: integers ≤ 2^53 are exact; above
    // 2^53 the representation skips. Per WPT read-min.any.js, the spec
    // intent is "reject if the value can't round-trip"; we enforce that
    // by clamping to 2^53 - 1 (= Number.MAX_SAFE_INTEGER). Higher values
    // are rejected, NOT silently truncated.
    if n < 0.0 {
        return Err(OpError::type_error(
            "[EnforceRange] unsigned long long: value is negative",
        ));
    }

    // 2^53 - 1 is the largest exact integer in IEEE 754 double; values
    // strictly above this lose precision and the WebIDL contract can't
    // be met (the same JS Number could encode multiple distinct u64s).
    const MAX_SAFE: f64 = (1u64 << 53) as f64 - 1.0;
    if n > MAX_SAFE {
        return Err(OpError::type_error(
            "[EnforceRange] unsigned long long: value exceeds Number.MAX_SAFE_INTEGER",
        ));
    }

    // Truncate towards zero (IntegerPart). At this point n ∈ [0, 2^53-1]
    // so the cast is lossless.
    let truncated = n.trunc() as u64;
    Ok(EnforceRangeU64(truncated))
}
