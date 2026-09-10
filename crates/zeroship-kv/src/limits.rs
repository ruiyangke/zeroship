//! Shared key/value guardrails and backend-independent limits.
//!
//! Scoped Rust handles validate input; V8 also rejects malformed arguments
//! synchronously before dispatch. Low-level backends accept typed arguments.

use crate::error::KvError;

/// Maximum key length in bytes.
pub const MAX_KEY_LEN: usize = 512;

/// Maximum value size in bytes; larger payloads belong in object storage.
pub const MAX_VALUE_BYTES: usize = 256 * 1024;

/// Default `list` page size when the caller doesn't specify `limit`.
pub const LIST_DEFAULT_LIMIT: usize = 1000;

/// Maximum requested list page size. Redis treats it as a scan hint.
pub const LIST_MAX_LIMIT: usize = 10_000;

/// Maximum TTL accepted by the creator-facing KV contract.
pub const MAX_TTL_MS: u64 = 100 * 365 * 24 * 60 * 60 * 1000;

/// Validate a user-supplied key. Rejects empty keys, keys over
/// [`MAX_KEY_LEN`] bytes, and keys carrying bytes that would corrupt
/// the scoping wire format or a SCAN pattern: the hash-tag braces
/// `{` / `}`, NUL, and ASCII control chars.
///
/// Braces are reserved by the key grammar. Tenant isolation comes from
/// the trusted app scope supplied separately to [`crate::backend::scope`].
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

/// Validate the byte size of a string value. Serialization and language-level
/// type checks belong to the caller.
pub fn validate_value(value: &str) -> Result<(), KvError> {
    if value.len() > MAX_VALUE_BYTES {
        return Err(KvError::invalid_value(format!(
            "kv: value exceeds {MAX_VALUE_BYTES} bytes (got {})",
            value.len()
        )));
    }
    Ok(())
}

/// Validate a typed TTL before backend deadline arithmetic or Redis commands.
///
/// # Errors
/// Returns `InvalidArgument` for zero or values above [`MAX_TTL_MS`].
pub fn validate_ttl_ms(ttl_ms: u64) -> Result<(), KvError> {
    if ttl_ms == 0 {
        return Err(KvError::invalid_argument(
            "kv: ttlMs must be greater than 0",
        ));
    }
    if ttl_ms > MAX_TTL_MS {
        return Err(KvError::invalid_argument("kv: ttlMs exceeds the maximum"));
    }
    Ok(())
}

/// Normalize a typed page-size hint, using the default when unspecified as zero.
#[must_use]
pub fn normalize_list_limit(limit: usize) -> usize {
    if limit == 0 {
        LIST_DEFAULT_LIMIT
    } else {
        limit.min(LIST_MAX_LIMIT)
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
        assert!(matches!(
            validate_value(&big),
            Err(KvError::InvalidValue { .. })
        ));
        let ok = "x".repeat(MAX_VALUE_BYTES);
        assert!(validate_value(&ok).is_ok());
        assert!(validate_value("").is_ok()); // empty string allowed
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
