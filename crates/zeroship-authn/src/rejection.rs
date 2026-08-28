//! The one type every authn refusal is constructed as.
//!
//! # Why this exists rather than `ntex::web::error::ErrorUnauthorized`
//!
//! ntex's helpers (`ErrorUnauthorized`, `ErrorInternalServerError`,
//! `InternalError::from_response`) all build a `web::error::InternalError`,
//! which implements `WebResponseError` by overriding `error_response` ONLY -
//! see `ntex-3.7.2/src/web/error.rs`, `impl<T, E> WebResponseError<E> for
//! InternalError<T, E>`. It never overrides `status_code`, so the trait
//! default applies and `status_code()` answers `500 INTERNAL_SERVER_ERROR`
//! whatever status the value was constructed with.
//!
//! That splits an `InternalError` in two. A caller that RENDERS it reads the
//! status the constructor chose; a caller that ASKS it for its status reads
//! 500. `zeroship-migrate-server` asks (`crates/zeroship-migrate-server/src/auth.rs`,
//! `map_bearer_error`), so every 401 authn produced arrived there as a 500 and
//! was classified as broken infrastructure rather than a rejected credential.
//!
//! `AuthnRejection` carries its status as data and answers both halves from
//! it, so the two can no longer disagree.

use std::fmt;

use ntex::http::StatusCode;
use ntex::web::{DefaultError, HttpRequest, HttpResponse, WebResponseError};
use serde_json::json;

/// A refusal from the bearer path, rendered as `{"error": "<code>"}`.
///
/// `code` is a stable machine-readable token, not prose: it is the response
/// body, the `Display` text, and what a caller matches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthnRejection {
    status: StatusCode,
    code: &'static str,
}

impl AuthnRejection {
    /// The credential was presented and refused: 401.
    #[must_use]
    pub const fn unauthorized(code: &'static str) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code,
        }
    }

    /// Authn could not reach the state it needs to decide: 500.
    ///
    /// Reserved for a genuinely broken dependency. A refusal that IS a
    /// decision - including a fail-closed one - is [`Self::unauthorized`], so
    /// that a client learns to stop rather than to retry.
    #[must_use]
    pub const fn internal(code: &'static str) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code,
        }
    }

    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for AuthnRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code)
    }
}

impl std::error::Error for AuthnRejection {}

impl WebResponseError<DefaultError> for AuthnRejection {
    fn status_code(&self) -> StatusCode {
        self.status
    }

    fn error_response(&self, _: &HttpRequest) -> HttpResponse {
        HttpResponse::build(self.status).json(&json!({ "error": self.code }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the type: the status a rejection is CONSTRUCTED with
    /// is the status it REPORTS.
    ///
    /// This is what `ntex::web::error::InternalError` does not do. Swap either
    /// constructor below back to `web::error::ErrorUnauthorized` /
    /// `InternalError::from_response` and the 401 case reads 500 here.
    ///
    /// It does not check the rendered response body, the `Display` text, or
    /// any call site; `rejection_carries_its_status_through_web_error` covers
    /// the boxing step, and the call sites are covered in `lib.rs`.
    #[test]
    fn rejection_reports_the_status_it_was_constructed_with() {
        assert_eq!(
            AuthnRejection::unauthorized("bad_credential").status_code(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            AuthnRejection::internal("lookup_failed").status_code(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// Callers do not hold an `AuthnRejection`; they hold the `web::Error` it
    /// was boxed into, and read the status back through `as_response_error()`.
    /// That is the exact expression `zeroship-migrate-server`'s `map_bearer_error`
    /// evaluates, so this asserts the boxing does not lose the status.
    ///
    /// It does not assert anything about which rejection any given authn path
    /// picks.
    #[test]
    fn rejection_carries_its_status_through_web_error() {
        let boxed: ntex::web::Error = AuthnRejection::unauthorized("bad_credential").into();
        assert_eq!(
            boxed.as_response_error().status_code(),
            StatusCode::UNAUTHORIZED,
            "a boxed 401 that reads back as {} is what made a rejected \
             credential look like a broken service",
            boxed.as_response_error().status_code()
        );
    }

    /// The body a client parses and the text a server logs are both the code,
    /// so the two cannot drift apart.
    #[test]
    fn rejection_displays_its_code() {
        assert_eq!(
            AuthnRejection::unauthorized("wrong_audience").to_string(),
            "wrong_audience"
        );
    }
}
