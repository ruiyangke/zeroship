//! Storage and validation errors, independent of a language runtime.

/// Classified failure from a KV operation or input guardrail.
#[derive(Debug, Clone)]
pub enum KvError {
    InvalidKey { message: String },
    InvalidValue { message: String },
    InvalidArgument { message: String },
    NonNumeric { message: String },
    Overflow { message: String },
    ListTooLarge { message: String },
    Connection { message: String },
    Backend { message: String },
}

impl KvError {
    /// Convenience: invalid-key error.
    pub fn invalid_key(message: impl Into<String>) -> Self {
        KvError::InvalidKey {
            message: message.into(),
        }
    }

    /// Convenience: invalid-value error.
    pub fn invalid_value(message: impl Into<String>) -> Self {
        KvError::InvalidValue {
            message: message.into(),
        }
    }

    /// Convenience: invalid-argument error.
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        KvError::InvalidArgument {
            message: message.into(),
        }
    }

    /// Convenience: non-numeric incr error.
    pub fn non_numeric(message: impl Into<String>) -> Self {
        KvError::NonNumeric {
            message: message.into(),
        }
    }

    /// Convenience: overflow incr error.
    pub fn overflow(message: impl Into<String>) -> Self {
        KvError::Overflow {
            message: message.into(),
        }
    }

    /// Convenience: connection error.
    pub fn connection(message: impl Into<String>) -> Self {
        KvError::Connection {
            message: message.into(),
        }
    }

    /// Convenience: catch-all backend error.
    pub fn backend(message: impl Into<String>) -> Self {
        KvError::Backend {
            message: message.into(),
        }
    }

    /// Whether this error describes invalid caller input.
    #[must_use]
    pub fn is_validation(&self) -> bool {
        matches!(
            self,
            KvError::InvalidKey { .. }
                | KvError::InvalidValue { .. }
                | KvError::InvalidArgument { .. }
        )
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
