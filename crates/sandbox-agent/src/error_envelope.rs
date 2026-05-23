//! Standard error envelope (proposal § 10.0).
//!
//! Every error response from this crate's HTTP surface MUST be a JSON
//! object of the shape:
//!
//! ```json
//! { "error": "<machine_readable_kind>", "message": "<human readable>",
//!   ...kind-specific fields }
//! ```
//!
//! Before this helper landed, ~34 sites across `handlers.rs`,
//! `proxy.rs`, and `main.rs` emitted one of three non-compliant
//! shapes:
//!
//!   1. `{"error":"<human prose>"}` (no `message`; `handlers.rs::err`,
//!      `proxy.rs::err`, plus inline literals for unauthorized /
//!      draining / 404 not-found).
//!   2. `{"error":"<kind>"}` with no `message` (the inline
//!      `unauthorized` / `draining` literals).
//!   3. `{"error":"<human prose>","code":"<kind>"}` —
//!      `proxy.rs::err_with_code` INVERTED the A4 field order
//!      (`error` carried the prose, `code` carried the machine kind).
//!
//! All callers funnel through [`ErrorEnvelope`] now; the wire shape is
//! pinned by per-file unit tests. `pub(crate)`: the envelope is an
//! implementation detail. External callers see only the JSON.
//!
//! ## Code naming
//!
//! `code` is `snake_case`. Reuse an existing code when prose is
//! similar (e.g. every "auth required" maps to `"unauthorized"`); this
//! is what lets operators write a single dashboard rule per kind.
//!
//! ## Why a sibling of the sandbox crate's envelope
//!
//! The controller crate (`zeroship-sandbox`) has its own equivalent
//! helper. We do not lift the envelope into `zeroship-core` yet
//! because (a) the agent's surface is smaller — no `no_store`
//! Cache-Control needed (no CSRF / cookie auth in the VM), no extra
//! fields needed at the time of writing — and (b) the wire-shape
//! contracts of the two crates evolve independently. If a third
//! caller appears, fold the two into `zeroship-core` then.

use ntex::http::StatusCode;
use ntex::web::HttpResponse;
use serde_json::json;

/// A typed error envelope. Holds the HTTP status + machine-readable
/// `code` + human `message`, then renders to a `ntex::web::HttpResponse`
/// via [`ErrorEnvelope::into_response`].
///
/// Build via [`ErrorEnvelope::new`] (status + code + message). The
/// agent does not need an `with_extra` chainer (no kind-specific
/// fields in any current site); add one later if a caller needs it.
pub(crate) struct ErrorEnvelope {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ErrorEnvelope {
    /// Construct an envelope. `status` must align with the table in
    /// proposal § 10.0; `code` is `snake_case`; `message` is human prose.
    pub(crate) fn new(
        status: StatusCode,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    /// Render to a `ntex::web::HttpResponse`. `error` and `message`
    /// are both always present.
    pub(crate) fn into_response(self) -> HttpResponse {
        let body = json!({
            "error": self.code,
            "message": self.message,
        });
        HttpResponse::build(self.status).json(&body)
    }
}

/// Shorthand for the common "status + code + message" path. Equivalent
/// to `ErrorEnvelope::new(status, code, message).into_response()`.
#[inline]
pub(crate) fn error_response(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
) -> HttpResponse {
    ErrorEnvelope::new(status, code, message).into_response()
}

/// Convenience for the legacy `err(u16, msg)` shape used inside
/// `handlers.rs`. Picks the A4 `code` from the HTTP status (the
/// existing pre-A4 callers passed only prose, never a separate kind),
/// so the wire shape becomes `{ "error": "<code-from-status>",
/// "message": "<prose>" }` without forcing every call-site to thread
/// through a new argument.
///
/// Status → code mapping covers the four values the agent emits today
/// (400, 403, 404, 500); anything else falls back to `internal`.
#[inline]
pub(crate) fn error_from_status(
    status: u16,
    message: impl Into<String>,
) -> HttpResponse {
    let (st, code) = match status {
        400 => (StatusCode::BAD_REQUEST, "invalid_input"),
        403 => (StatusCode::FORBIDDEN, "forbidden"),
        404 => (StatusCode::NOT_FOUND, "not_found"),
        500 => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    };
    error_response(st, code, message)
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use ntex::web::test;

    /// Drain the body of an HttpResponse and parse it as JSON. Used
    /// by per-file wire-shape tests below.
    pub(crate) async fn body_json(resp: ntex::web::HttpResponse) -> serde_json::Value {
        // Route through ntex::web::WebResponse so test::read_body works.
        let req = test::TestRequest::default().to_http_request();
        let web_resp = ntex::web::WebResponse::new(resp, req);
        let bytes = test::read_body(web_resp).await;
        serde_json::from_slice(&bytes).expect("body is JSON")
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers::body_json;
    use super::*;

    #[ntex::test]
    async fn renders_error_and_message() {
        let resp = error_response(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "explanation goes here",
        );
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "invalid_input");
        assert_eq!(body["message"], "explanation goes here");
    }

    #[ntex::test]
    async fn error_from_status_maps_400_to_invalid_input() {
        let resp = error_from_status(400, "bad json: trailing comma");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "invalid_input");
        assert_eq!(body["message"], "bad json: trailing comma");
    }

    #[ntex::test]
    async fn error_from_status_maps_403_to_forbidden() {
        let resp = error_from_status(403, "symlink rejected");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "forbidden");
        assert_eq!(body["message"], "symlink rejected");
    }

    #[ntex::test]
    async fn error_from_status_maps_404_to_not_found() {
        let resp = error_from_status(404, "no such path");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "not_found");
        assert_eq!(body["message"], "no such path");
    }

    #[ntex::test]
    async fn error_from_status_maps_500_to_internal() {
        let resp = error_from_status(500, "exec failed");
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "internal");
        assert_eq!(body["message"], "exec failed");
    }

    #[ntex::test]
    async fn error_from_status_unknown_falls_back_to_internal() {
        let resp = error_from_status(418, "i'm a teapot");
        // Unknown statuses normalise to 500 + "internal" so callers
        // can't accidentally leak a free-form status into the wire.
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "internal");
        assert_eq!(body["message"], "i'm a teapot");
    }
}
