//! Static guardrails for the `env.kv.*` surface.
//!
//! Every validation here runs **synchronously** in the v8_class method
//! body, before any async op is dispatched or any allocation is made
//! against the backend. A failure becomes a [`KvError`] of a
//! validation class (→ JS `TypeError`); the method returns early
//! without touching the backend.
//!
//! The constants are the "forever" shape of the surface (see the
//! redesign proposal §3): a generous-but-bounded key/value size, and a
//! `list` page that defaults to a sane batch and clamps (NOT errors) at
//! a hard max so a creator paging with a huge `limit` gets capped
//! rather than refused.

use crate::error::KvError;

/// Maximum key length in bytes. Generous enough for namespaced keys
/// (`user:123:session:abc`) without letting a key become an accidental
/// value store. 512 B matches the proposal.
pub const MAX_KEY_LEN: usize = 512;

/// Maximum value size in bytes (256 KiB). KV is for ephemeral hot-path
/// data, not blobs — large payloads belong in `env.storage`.
pub const MAX_VALUE_BYTES: usize = 256 * 1024;

/// Default `list` page size when the caller doesn't specify `limit`.
pub const LIST_DEFAULT_LIMIT: usize = 1000;

/// Hard ceiling on a `list` page. A caller-supplied `limit` above this
/// is **clamped** to it (not rejected) — `ListTooLarge` is reserved for
/// a backend that hands back a runaway page despite the cap.
pub const LIST_MAX_LIMIT: usize = 10_000;

/// Maximum `ttlMs` (100 years, in milliseconds). A TTL is an *absolute*
/// deadline computed as `now_ms() + ttl_ms` on the redb tier; an
/// unbounded `ttlMs` (e.g. `1e19`, which is finite and passes the
/// integer/negative checks) would overflow that `u64` add — a debug
/// panic in the isolate pump (DoS) or a silent already-expired write in
/// release. 100 years is absurdly generous for a cache TTL yet sits far
/// below `u64::MAX - now_ms()`, so the deadline arithmetic can never
/// wrap. It also pins the redb/Redis divergence shut alongside the
/// `ttlMs == 0` rejection below (Dragonfly rejects `PX 0`; redb would
/// store an instantly-expired key).
pub const MAX_TTL_MS: u64 = 100 * 365 * 24 * 60 * 60 * 1000;

/// Validate a user-supplied key. Rejects empty keys, keys over
/// [`MAX_KEY_LEN`] bytes, and keys carrying bytes that would corrupt
/// the scoping wire format or a SCAN pattern: the hash-tag braces
/// `{` / `}`, NUL, and ASCII control chars.
///
/// The braces are forbidden because [`crate::backend::scope`] wraps the
/// app_id in `{...}` for Redis-cluster hash-tagging; a user key
/// containing a brace could forge a second hash-tag and escape its
/// app's slot.
pub fn validate_key(key: &str) -> Result<(), KvError> {
    if key.is_empty() {
        return Err(KvError::invalid_key("kv: key must be a non-empty string"));
    }
    if key.len() > MAX_KEY_LEN {
        return Err(KvError::invalid_key(format!(
            "kv: key exceeds {MAX_KEY_LEN} bytes (got {})",
            key.len()
        )));
    }
    for c in key.chars() {
        if c == '{' || c == '}' || c == '\0' || c.is_control() {
            return Err(KvError::invalid_key(
                "kv: key must not contain '{', '}', NUL, or control characters",
            ));
        }
    }
    Ok(())
}

/// Validate a user-supplied value. The value arrives already
/// stringified by the SDK (JSON-encoded), so the only checks are
/// "is it actually a string" (handled by the caller extracting it) and
/// the byte-size cap.
pub fn validate_value(value: &str) -> Result<(), KvError> {
    if value.len() > MAX_VALUE_BYTES {
        return Err(KvError::invalid_value(format!(
            "kv: value exceeds {MAX_VALUE_BYTES} bytes (got {})",
            value.len()
        )));
    }
    Ok(())
}

/// Validate an `incr` delta read off a JS number. The SDK hands deltas
/// as f64 (JS has no integer type); reject non-finite values and values
/// outside the `i64` range, then return the integral delta.
///
/// A fractional delta is rejected too — `incr` is integer-only; a
/// creator wanting fractional accumulation should compute it in JS and
/// `set` the result.
pub fn validate_delta(by: f64) -> Result<i64, KvError> {
    if !by.is_finite() {
        return Err(KvError::invalid_argument("kv: incr `by` must be a finite number"));
    }
    if by.fract() != 0.0 {
        return Err(KvError::invalid_argument(
            "kv: incr `by` must be an integer (no fractional part)",
        ));
    }
    // i64::MAX / MIN are not exactly representable as f64; compare
    // against the f64 bounds that DO round-trip to avoid an out-of-range
    // cast. 2^63 is the first f64 above i64::MAX.
    if by >= 9_223_372_036_854_775_808.0 || by < -9_223_372_036_854_775_808.0 {
        return Err(KvError::invalid_argument("kv: incr `by` is out of i64 range"));
    }
    Ok(by as i64)
}

/// Validate a `ttlMs` option read off a JS number. Rejects non-finite,
/// fractional, negative, zero, and out-of-range values (> [`MAX_TTL_MS`]);
/// returns the integral milliseconds.
///
/// `ttlMs == 0` is rejected because the backends diverge on it: Dragonfly
/// rejects `PX 0` with `ERR invalid expire time`, while redb would store
/// an instantly-expired key. The upper bound keeps the redb deadline
/// arithmetic (`now_ms() + ttl_ms`) from overflowing `u64` — see
/// [`MAX_TTL_MS`].
pub fn validate_ttl_ms(ttl_ms: f64) -> Result<u64, KvError> {
    if !ttl_ms.is_finite() {
        return Err(KvError::invalid_argument("kv: ttlMs must be a finite number"));
    }
    if ttl_ms.fract() != 0.0 {
        return Err(KvError::invalid_argument("kv: ttlMs must be an integer"));
    }
    if ttl_ms < 0.0 {
        return Err(KvError::invalid_argument("kv: ttlMs must not be negative"));
    }
    if ttl_ms == 0.0 {
        return Err(KvError::invalid_argument("kv: ttlMs must be greater than 0"));
    }
    if ttl_ms > MAX_TTL_MS as f64 {
        return Err(KvError::invalid_argument(
            "kv: ttlMs exceeds the maximum (100 years)",
        ));
    }
    Ok(ttl_ms as u64)
}

/// Normalise a caller-supplied `list` limit into the effective page
/// size. `None` → [`LIST_DEFAULT_LIMIT`]; anything above
/// [`LIST_MAX_LIMIT`] is clamped (not rejected); a non-positive value
/// falls back to the default.
#[must_use]
pub fn resolve_list_limit(limit: Option<f64>) -> usize {
    match limit {
        Some(l) if l.is_finite() && l >= 1.0 => (l as usize).min(LIST_MAX_LIMIT),
        _ => LIST_DEFAULT_LIMIT,
    }
}

/// Escape Redis glob metacharacters in a literal prefix so a SCAN
/// `MATCH` pattern treats it as a literal, not a glob. Redis glob
/// special chars are `*`, `?`, `[`, `]`, `\`, and `^` (inside a class);
/// each is escaped with a leading backslash.
///
/// Without this, a creator listing keys under prefix `a[b` would have
/// `[b` interpreted as a character class and silently match the wrong
/// keys. The redb backend does literal `starts_with` and
/// don't need escaping, so this lives here for the Redis backend to
/// call (and is unit-tested here, away from a live server).
#[must_use]
pub fn escape_glob(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len());
    for c in prefix.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\' | '^') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_key_accepts_normal_keys() {
        assert!(validate_key("user:123:session").is_ok());
        assert!(validate_key("a").is_ok());
    }

    #[test]
    fn validate_key_rejects_empty() {
        assert!(matches!(validate_key(""), Err(KvError::InvalidKey { .. })));
    }

    #[test]
    fn validate_key_rejects_too_long() {
        let k = "x".repeat(MAX_KEY_LEN + 1);
        assert!(matches!(validate_key(&k), Err(KvError::InvalidKey { .. })));
        let ok = "x".repeat(MAX_KEY_LEN);
        assert!(validate_key(&ok).is_ok());
    }

    #[test]
    fn validate_key_rejects_braces_and_control() {
        assert!(validate_key("a{b").is_err());
        assert!(validate_key("a}b").is_err());
        assert!(validate_key("a\0b").is_err());
        assert!(validate_key("a\nb").is_err());
        assert!(validate_key("a\tb").is_err());
    }

    #[test]
    fn validate_value_caps_size() {
        let big = "x".repeat(MAX_VALUE_BYTES + 1);
        assert!(matches!(validate_value(&big), Err(KvError::InvalidValue { .. })));
        let ok = "x".repeat(MAX_VALUE_BYTES);
        assert!(validate_value(&ok).is_ok());
        assert!(validate_value("").is_ok()); // empty string allowed
    }

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

    #[test]
    fn escape_glob_escapes_metacharacters() {
        assert_eq!(escape_glob("a*b"), "a\\*b");
        assert_eq!(escape_glob("a?b"), "a\\?b");
        assert_eq!(escape_glob("a[b]"), "a\\[b\\]");
        assert_eq!(escape_glob("a\\b"), "a\\\\b");
        assert_eq!(escape_glob("a^b"), "a\\^b");
        assert_eq!(escape_glob("plain"), "plain");
    }
}
