//! Typed error classification for `plugin-kv`.
//!
//! Every fallible `Backend` method returns `Result<_, KvError>`. At the
//! V8 boundary the dispatch layer calls [`KvError::to_op_error`] to
//! materialise an [`OpError`] whose `.code` is stamped from the variant
//! — the `@zeroship/kv` SDK can then branch on `err.code` instead of
//! substring-matching opaque messages. Mirrors `plugin-db`'s `DbError`.
//!
//! The validation-class variants (`InvalidKey` / `InvalidValue` /
//! `InvalidArgument`) are thrown **synchronously** from the v8_class
//! method bodies, before any async op is dispatched — they materialise
//! as JS `TypeError`s (not coded errors) so a creator's `try/catch`
//! observes the same shape as any other bad-argument throw. The
//! remaining variants surface on the async resolution path with a
//! coded `.code`.
//!
//! ## When to use which variant
//!
//! | Variant | Cause | Wire shape |
//! |---|---|---|
//! | [`KvError::InvalidKey`] | key empty / too long / control chars / `{`/`}`/NUL | sync `TypeError` |
//! | [`KvError::InvalidValue`] | value not a string / too large | sync `TypeError` |
//! | [`KvError::InvalidArgument`] | bad option (negative ttl, non-finite delta, …) | sync `TypeError` |
//! | [`KvError::NonNumeric`] | `incr` on a non-numeric existing value | `kv_non_numeric` |
//! | [`KvError::Overflow`] | `incr` over/underflowed `i64` | `kv_overflow` |
//! | [`KvError::ListTooLarge`] | backend returned a runaway page | `kv_list_too_large` |
//! | [`KvError::Connection`] | backend connect/transport failure (credentials redacted) | `kv_connection` |
//! | [`KvError::Backend`] | anything else from the backend | `kv_backend` |

use zeroship_runtime::state::OpError;

/// Classified error origin for every fallible `plugin-kv` helper.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum KvError {
    /// Key failed a static guardrail (empty, over `MAX_KEY_LEN`, or
    /// contains a forbidden byte — `{`, `}`, NUL, control chars). Thrown
    /// synchronously as a `TypeError` before dispatch.
    InvalidKey { message: String },

    /// Value failed a static guardrail (not a string, or over
    /// `MAX_VALUE_BYTES`). Thrown synchronously as a `TypeError`.
    InvalidValue { message: String },

    /// An option argument was malformed (negative / non-finite `ttlMs`,
    /// non-finite `by`, out-of-range cursor/limit shape, …). Thrown
    /// synchronously as a `TypeError`.
    InvalidArgument { message: String },

    /// `incr` was called on a key whose existing value isn't a base-10
    /// integer. Surfaces on the async path with code `kv_non_numeric`.
    NonNumeric { message: String },

    /// `incr` over/underflowed the `i64` range. Surfaces with code
    /// `kv_overflow`. Distinct from a generic backend error so the SDK
    /// can tell the creator their counter saturated.
    Overflow { message: String },

    /// The backend returned more keys in one page than the hard cap
    /// allows (runaway protection). Surfaces with `kv_list_too_large`.
    ListTooLarge { message: String },

    /// Backend connect / transport failure. The message has any URL
    /// credentials redacted (see [`redact_url`]). Surfaces with
    /// `kv_connection`; the SDK may retry after a short backoff.
    Connection { message: String },

    /// Catch-all backend failure (unexpected reply, server error that
    /// isn't a numeric-overflow / non-numeric complaint, …). Surfaces
    /// with `kv_backend`.
    Backend { message: String },
}

impl KvError {
    /// Convenience: invalid-key error.
    pub fn invalid_key(message: impl Into<String>) -> Self {
        KvError::InvalidKey { message: message.into() }
    }

    /// Convenience: invalid-value error.
    pub fn invalid_value(message: impl Into<String>) -> Self {
        KvError::InvalidValue { message: message.into() }
    }

    /// Convenience: invalid-argument error.
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        KvError::InvalidArgument { message: message.into() }
    }

    /// Convenience: non-numeric incr error.
    pub fn non_numeric(message: impl Into<String>) -> Self {
        KvError::NonNumeric { message: message.into() }
    }

    /// Convenience: overflow incr error.
    pub fn overflow(message: impl Into<String>) -> Self {
        KvError::Overflow { message: message.into() }
    }

    /// Convenience: connection error.
    pub fn connection(message: impl Into<String>) -> Self {
        KvError::Connection { message: message.into() }
    }

    /// Convenience: catch-all backend error.
    pub fn backend(message: impl Into<String>) -> Self {
        KvError::Backend { message: message.into() }
    }

    /// `true` for the validation-class variants thrown synchronously as
    /// `TypeError`s before any dispatch. The v8_class method bodies use
    /// this to decide between a synchronous throw and an async reject.
    #[must_use]
    pub fn is_validation(&self) -> bool {
        matches!(
            self,
            KvError::InvalidKey { .. }
                | KvError::InvalidValue { .. }
                | KvError::InvalidArgument { .. }
        )
    }

    /// Stamp this `KvError` onto an [`OpError`]. The validation-class
    /// variants become `TypeError`s (so `try/catch` sees the same shape
    /// as any other bad-argument throw); the rest become coded errors
    /// carrying `.code` for the SDK to branch on.
    pub fn to_op_error(self) -> OpError {
        match self {
            KvError::InvalidKey { message }
            | KvError::InvalidValue { message }
            | KvError::InvalidArgument { message } => OpError::type_error(message),
            KvError::NonNumeric { message } => {
                OpError::coded("kv_non_numeric", message, None::<String>)
            }
            KvError::Overflow { message } => {
                OpError::coded("kv_overflow", message, None::<String>)
            }
            KvError::ListTooLarge { message } => {
                OpError::coded("kv_list_too_large", message, None::<String>)
            }
            KvError::Connection { message } => OpError::coded(
                "kv_connection",
                message,
                Some("transient backend failure; retry after a short backoff".to_string()),
            ),
            KvError::Backend { message } => {
                OpError::coded("kv_backend", message, None::<String>)
            }
        }
    }
}

impl std::fmt::Display for KvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KvError::InvalidKey { message }
            | KvError::InvalidValue { message }
            | KvError::InvalidArgument { message }
            | KvError::NonNumeric { message }
            | KvError::Overflow { message }
            | KvError::ListTooLarge { message }
            | KvError::Connection { message }
            | KvError::Backend { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for KvError {}

/// Redact any userinfo (`user:password@`) from a URL before it lands in
/// an error message. Best-effort: if the string doesn't parse as a URL
/// we return it unchanged (it carried no credentials we could find).
///
/// Connection errors routinely echo the configured URL; without this a
/// Redis password set via `redis://default:hunter2@host` would leak
/// into the worker log and the JS `err.message`.
#[must_use]
pub fn redact_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(mut u) => {
            if !u.username().is_empty() || u.password().is_some() {
                let _ = u.set_username("");
                let _ = u.set_password(None);
            }
            u.into()
        }
        Err(_) => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_runtime::state::OpErrorKind;

    #[test]
    fn validation_variants_become_type_errors() {
        for e in [
            KvError::invalid_key("k"),
            KvError::invalid_value("v"),
            KvError::invalid_argument("a"),
        ] {
            assert!(e.is_validation());
            match e.to_op_error().kind {
                OpErrorKind::TypeError => {}
                other => panic!("expected TypeError, got {other:?}"),
            }
        }
    }

    #[test]
    fn coded_variants_stamp_canonical_codes() {
        let cases = [
            (KvError::non_numeric(""), "kv_non_numeric"),
            (KvError::overflow(""), "kv_overflow"),
            (KvError::ListTooLarge { message: "".into() }, "kv_list_too_large"),
            (KvError::connection(""), "kv_connection"),
            (KvError::backend(""), "kv_backend"),
        ];
        for (variant, expected) in cases {
            assert!(!variant.is_validation());
            match variant.to_op_error().kind {
                OpErrorKind::CodedError { code, .. } => assert_eq!(code, expected),
                other => panic!("expected CodedError, got {other:?}"),
            }
        }
    }

    #[test]
    fn connection_carries_retry_hint() {
        match KvError::connection("boom").to_op_error().kind {
            OpErrorKind::CodedError { hint, .. } => assert!(hint.is_some()),
            other => panic!("expected CodedError, got {other:?}"),
        }
    }

    #[test]
    fn redact_url_strips_credentials() {
        let out = redact_url("redis://default:hunter2@host:6379/0");
        assert!(!out.contains("hunter2"), "password leaked: {out}");
        assert!(!out.contains("default"), "username leaked: {out}");
        assert!(out.contains("host:6379"));
    }

    #[test]
    fn redact_url_passes_credential_free_url_through() {
        let out = redact_url("redis://host:6379");
        assert!(out.starts_with("redis://host:6379"));
    }

    #[test]
    fn redact_url_passes_unparseable_through() {
        assert_eq!(redact_url("not a url"), "not a url");
    }
}
