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
    /// A transient transport failure (connect refused/reset, socket I/O,
    /// hyper connection error, request timeout). Retrying the SAME request may
    /// succeed once the network blip clears — `is_retryable()` is `true`.
    Transport(String),
    /// A *deterministic* transport-layer failure that will recur on every
    /// attempt: a request-build error, an invalid URL / bad scheme, a TLS
    /// handshake/config error, or an HTTP redirect we cannot follow (e.g. a
    /// wrong-region 301). Retrying burns the attempt budget for nothing, so
    /// `is_retryable()` is `false`.
    TransportTerminal(String),
}

impl S3Error {
    /// Whether an idempotent operation (GET/HEAD/LIST/DELETE) or an idempotent
    /// part-PUT may be retried.
    ///
    /// `Retryable` (429/5xx/timeout) and `Transport` (transient connect/reset/
    /// socket blips) are retryable. Everything else is terminal for a single
    /// attempt — including `TransportTerminal` (build/URL/TLS/redirect errors
    /// that will recur identically), auth, not-found, precondition, conflict,
    /// invalid-response, too-large, integrity. Retrying a terminal transport
    /// error just burns the whole attempt budget plus backoff for nothing.
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
            // 408 Request Timeout is a server-side "you were too slow" — the
            // same class as our own send/body timeouts, so it is retryable.
            408 | 429 | 500 | 502 | 503 | 504 => Self::Retryable {
                status: Some(status),
                detail,
            },
            // 3xx on an S3 data-plane request is virtually always a wrong-region
            // / wrong-endpoint misconfiguration (e.g. AWS 301
            // PermanentRedirect). We do NOT auto-follow; surface it clearly and
            // terminally rather than burying it as a generic InvalidResponse or
            // (worse) retrying it.
            300..=399 => Self::TransportTerminal(format!(
                "unexpected redirect (status {status}); check region/endpoint config: {detail}"
            )),
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
            Self::TransportTerminal(d) => write!(f, "transport (terminal): {d}"),
        }
    }
}

impl std::error::Error for S3Error {}

impl From<cyper::Error> for S3Error {
    fn from(e: cyper::Error) -> Self {
        use cyper::Error as C;
        match e {
            // A timeout maps to the retryable timeout class.
            C::Timeout => Self::timeout("cyper request timeout"),
            // Transient transport blips — a connect/reset/socket error or a
            // hyper connection error. Retrying the SAME idempotent request may
            // clear the blip.
            C::System(_) | C::Hyper(_) | C::HyperClient(_) => Self::Transport(e.to_string()),
            // Deterministic, recur-on-every-attempt failures: no TLS backend, a
            // malformed URL / bad scheme, an `http` crate build error, a URL
            // parse/encode error, a JSON error, or a TLS handshake/config error.
            // Retrying these just burns the attempt budget; classify terminal.
            other => Self::TransportTerminal(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_is_retryable_terminal_is_not() {
        assert!(S3Error::Transport("connection reset".into()).is_retryable());
        assert!(!S3Error::TransportTerminal("no TLS backend".into()).is_retryable());
    }

    #[test]
    fn timeout_408_and_5xx_are_retryable() {
        assert!(S3Error::from_status(408, "").is_retryable());
        assert!(S3Error::from_status(429, "slow down").is_retryable());
        assert!(S3Error::from_status(503, "").is_retryable());
        // Our own send/body timeout helper is retryable too.
        assert!(S3Error::timeout("send timeout").is_retryable());
    }

    #[test]
    fn redirect_is_terminal_and_surfaces_region_hint() {
        let e = S3Error::from_status(301, "PermanentRedirect");
        assert!(!e.is_retryable(), "a 3xx redirect must not be retried");
        assert!(matches!(e, S3Error::TransportTerminal(_)));
        assert!(
            format!("{e}").contains("region"),
            "redirect error should hint at region/endpoint config: {e}"
        );
    }

    #[test]
    fn cyper_build_class_errors_are_terminal() {
        // A bad-scheme cyper error is deterministic → terminal, NOT retryable.
        let e: S3Error = cyper::Error::BadScheme("ftp".into()).into();
        assert!(matches!(e, S3Error::TransportTerminal(_)));
        assert!(!e.is_retryable());
    }

    #[test]
    fn cyper_io_class_errors_are_retryable() {
        // A socket I/O error is a transient transport blip → retryable.
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let e: S3Error = cyper::Error::System(io).into();
        assert!(matches!(e, S3Error::Transport(_)));
        assert!(e.is_retryable());
    }
}
