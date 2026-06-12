//! Typed S3 client errors and retryability classification.
//!
//! The taxonomy here is the contract the higher layers (`S3BlobStore`,
//! `plugin-storage::S3`) map from. Each variant carries enough shape for a
//! caller to decide retry / not-found / auth / integrity handling without
//! re-parsing strings. See the proposal §4 "Error taxonomy and retries".

use std::fmt;

/// Result alias for the S3 client surface.
pub type S3Result<T> = std::result::Result<T, S3Error>;

/// A typed S3 client error.
#[derive(Debug)]
#[non_exhaustive]
pub enum S3Error {
    /// HTTP 404 — object/key absent.
    NotFound,
    /// HTTP 401/403, or a credential/signature failure.
    Auth {
        /// HTTP status if it came from a response.
        status: Option<u16>,
        /// Short diagnostic (provider message / our own).
        detail: String,
    },
    /// HTTP 412 — conditional precondition (e.g. `If-None-Match: *`) failed.
    PreconditionFailed,
    /// HTTP 409 — conditional-write race / conflict.
    Conflict,
    /// Retryable status (429/500/502/503/504) or a timeout.
    Retryable {
        /// HTTP status if from a response; `None` for timeouts.
        status: Option<u16>,
        /// Short diagnostic.
        detail: String,
    },
    /// Malformed XML, missing required headers, bad content length, or a
    /// checksum/header mismatch.
    InvalidResponse(String),
    /// A response or request exceeded a configured cap.
    TooLarge {
        /// The cap that was exceeded.
        limit: u64,
        /// The observed size, if known.
        observed: Option<u64>,
    },
    /// Content-addressing integrity violation: bytes did not hash to the
    /// expected SHA-256.
    Integrity {
        /// Expected (caller-supplied) hex SHA-256.
        expected: String,
        /// Computed hex SHA-256.
        computed: String,
    },
    /// cyper/HTTP/TLS/socket transport error.
    Transport(String),
}

impl S3Error {
    /// Whether an idempotent operation (GET/HEAD/LIST/DELETE) may be retried.
    ///
    /// `Retryable` and `Transport` are retryable; everything else (auth,
    /// not-found, precondition, conflict, invalid-response, too-large,
    /// integrity) is terminal for a single attempt.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Retryable { .. } | Self::Transport(_))
    }

    /// Build a `Retryable` from a timeout (no HTTP status).
    #[must_use]
    pub fn timeout(detail: impl Into<String>) -> Self {
        Self::Retryable {
            status: None,
            detail: detail.into(),
        }
    }

    /// Map a raw HTTP status (with a small diagnostic body) to a typed error.
    ///
    /// Used after a non-2xx response. `body` is the (already capped/drained)
    /// error body for diagnostics; it may be empty.
    #[must_use]
    pub fn from_status(status: u16, body: &str) -> Self {
        let detail = Self::clip(body);
        match status {
            404 => Self::NotFound,
            401 | 403 => Self::Auth {
                status: Some(status),
                detail,
            },
            412 => Self::PreconditionFailed,
            409 => Self::Conflict,
            429 | 500 | 502 | 503 | 504 => Self::Retryable {
                status: Some(status),
                detail,
            },
            _ => Self::InvalidResponse(format!("unexpected status {status}: {detail}")),
        }
    }

    /// Clip an error/diagnostic body to a sane length so we never log or
    /// embed an unbounded provider payload.
    fn clip(s: &str) -> String {
        const MAX: usize = 512;
        let trimmed = s.trim();
        if trimmed.len() <= MAX {
            trimmed.to_string()
        } else {
            let mut end = MAX;
            while !trimmed.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &trimmed[..end])
        }
    }
}

impl fmt::Display for S3Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "not found"),
            Self::Auth { status, detail } => {
                write!(f, "auth error")?;
                if let Some(s) = status {
                    write!(f, " (status {s})")?;
                }
                if !detail.is_empty() {
                    write!(f, ": {detail}")?;
                }
                Ok(())
            }
            Self::PreconditionFailed => write!(f, "precondition failed"),
            Self::Conflict => write!(f, "conflict"),
            Self::Retryable { status, detail } => {
                write!(f, "retryable")?;
                if let Some(s) = status {
                    write!(f, " (status {s})")?;
                }
                if !detail.is_empty() {
                    write!(f, ": {detail}")?;
                }
                Ok(())
            }
            Self::InvalidResponse(d) => write!(f, "invalid response: {d}"),
            Self::TooLarge { limit, observed } => {
                write!(f, "too large (limit {limit}")?;
                if let Some(o) = observed {
                    write!(f, ", observed {o}")?;
                }
                write!(f, ")")
            }
            Self::Integrity { expected, computed } => {
                write!(f, "integrity: expected {expected}, computed {computed}")
            }
            Self::Transport(d) => write!(f, "transport: {d}"),
        }
    }
}

impl std::error::Error for S3Error {}

impl From<cyper::Error> for S3Error {
    fn from(e: cyper::Error) -> Self {
        match e {
            cyper::Error::Timeout => Self::timeout("cyper request timeout"),
            other => Self::Transport(other.to_string()),
        }
    }
}
