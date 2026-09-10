//! Convert JavaScript numeric options into typed KV arguments.

use zeroship_kv::limits::LIST_DEFAULT_LIMIT;
#[cfg(test)]
use zeroship_kv::limits::{LIST_MAX_LIMIT, MAX_TTL_MS};
use zeroship_kv::KvError;

/// Validate an `incr` delta read off a JS number. The SDK hands deltas
/// as f64 (JS has no integer type); reject non-finite values and values
/// outside the `i64` range, then return the integral delta.
///
/// A fractional delta is rejected too — `incr` is integer-only; a
/// creator wanting fractional accumulation should compute it in JS and
/// `set` the result.
pub fn validate_delta(by: f64) -> Result<i64, KvError> {
    if !by.is_finite() {
        return Err(KvError::invalid_argument(
            "kv: incr `by` must be a finite number",
        ));
    }
    if by.fract() != 0.0 {
        return Err(KvError::invalid_argument(
            "kv: incr `by` must be an integer (no fractional part)",
        ));
    }
    // i64::MAX / MIN are not exactly representable as f64; compare
    // against the f64 bounds that DO round-trip to avoid an out-of-range
    // cast. 2^63 is the first f64 above i64::MAX.
    if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&by) {
        return Err(KvError::invalid_argument(
            "kv: incr `by` is out of i64 range",
        ));
    }
    Ok(by as i64)
}

/// Validate a `ttlMs` option read off a JS number. Rejects non-finite,
/// fractional, negative, zero, and out-of-range values (> [`zeroship_kv::limits::MAX_TTL_MS`]);
/// returns the integral milliseconds.
///
/// `ttlMs == 0` is rejected because the backends diverge on it: Dragonfly
/// rejects `PX 0` with `ERR invalid expire time`, while redb would store
/// an instantly-expired key. The upper bound keeps the redb deadline
/// arithmetic (`now_ms() + ttl_ms`) from overflowing `u64` — see
/// [`MAX_TTL_MS`].
pub fn validate_ttl_ms(ttl_ms: f64) -> Result<u64, KvError> {
    if !ttl_ms.is_finite() {
        return Err(KvError::invalid_argument(
            "kv: ttlMs must be a finite number",
        ));
    }
    if ttl_ms.fract() != 0.0 {
        return Err(KvError::invalid_argument("kv: ttlMs must be an integer"));
    }
    if ttl_ms < 0.0 {
        return Err(KvError::invalid_argument("kv: ttlMs must not be negative"));
    }
    let ttl_ms = ttl_ms as u64;
    zeroship_kv::limits::validate_ttl_ms(ttl_ms)?;
    Ok(ttl_ms)
}

/// Normalise a caller-supplied `list` limit into the effective page
/// size. `None` → [`LIST_DEFAULT_LIMIT`]; anything above
/// [`zeroship_kv::limits::LIST_MAX_LIMIT`] is clamped (not rejected); a non-positive value
/// falls back to the default.
#[must_use]
pub fn resolve_list_limit(limit: Option<f64>) -> usize {
    match limit {
        Some(l) if l.is_finite() && l >= 1.0 => {
            zeroship_kv::limits::normalize_list_limit(l as usize)
        }
        _ => LIST_DEFAULT_LIMIT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_delta_round_trips_integers() {
        assert_eq!(validate_delta(1.0).unwrap(), 1);
        assert_eq!(validate_delta(-5.0).unwrap(), -5);
        assert_eq!(validate_delta(0.0).unwrap(), 0);
    }

    #[test]
    fn validate_delta_rejects_fractional_and_nonfinite() {
        assert!(validate_delta(1.5).is_err());
        assert!(validate_delta(f64::NAN).is_err());
        assert!(validate_delta(f64::INFINITY).is_err());
    }

    #[test]
    fn validate_ttl_rejects_negative_and_fractional() {
        assert!(validate_ttl_ms(-1.0).is_err());
        assert!(validate_ttl_ms(1.5).is_err());
        assert_eq!(validate_ttl_ms(1000.0).unwrap(), 1000);
    }

    #[test]
    fn validate_ttl_rejects_zero() {
        // Backends diverge on PX 0 — reject it up front.
        assert!(matches!(
            validate_ttl_ms(0.0),
            Err(KvError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn validate_ttl_rejects_above_max() {
        // Just over the 100-year cap (the `1e19`-class overflow vector).
        let over = MAX_TTL_MS as f64 + 1.0;
        assert!(matches!(
            validate_ttl_ms(over),
            Err(KvError::InvalidArgument { .. })
        ));
        assert!(validate_ttl_ms(1e19).is_err());
    }

    #[test]
    fn validate_ttl_accepts_normal_and_exact_max() {
        assert_eq!(validate_ttl_ms(60_000.0).unwrap(), 60_000);
        assert_eq!(validate_ttl_ms(MAX_TTL_MS as f64).unwrap(), MAX_TTL_MS);
    }

    #[test]
    fn resolve_list_limit_defaults_and_clamps() {
        assert_eq!(resolve_list_limit(None), LIST_DEFAULT_LIMIT);
        assert_eq!(resolve_list_limit(Some(50.0)), 50);
        assert_eq!(resolve_list_limit(Some(1e9)), LIST_MAX_LIMIT);
        assert_eq!(resolve_list_limit(Some(0.0)), LIST_DEFAULT_LIMIT);
        assert_eq!(resolve_list_limit(Some(-5.0)), LIST_DEFAULT_LIMIT);
    }
}
