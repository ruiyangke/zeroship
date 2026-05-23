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
//! Before this helper landed, 18 sites across `admin_handlers.rs`,
//! `handlers.rs`, `preview.rs`, and `preview_share_handlers.rs` emitted
//! one of three non-compliant shapes:
//!
//!   1. `{"error":"<human prose>"}` (no `message`).
//!   2. `{"error":"<human prose>","code":"<kind>"}` (no `message`).
//!   3. `{"error":"<kind>","code":"<kind>"}` (duplicate `code`, no
//!      `message`).
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

use ntex::http::StatusCode;
use ntex::web::HttpResponse;
use serde_json::{json, Value};

/// A typed error envelope. Holds the HTTP status + machine-readable
/// `code` + human `message` + optional kind-specific extra fields,
/// then renders to a `ntex::web::HttpResponse` via
/// [`ErrorEnvelope::into_response`].
///
/// Build via [`ErrorEnvelope::new`] (status + code + message); chain
/// [`ErrorEnvelope::with_extra`] to attach kind-specific fields
/// (e.g. `expected` / `current` for `state_mismatch`).
///
/// Cache-Control: set `no_store=true` to emit `Cache-Control: no-store`
/// (needed for CSRF / auth-pre-flight responses).
pub(crate) struct ErrorEnvelope {
    status: StatusCode,
    code: &'static str,
    message: String,
    extra: Option<Value>,
    no_store: bool,
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
            extra: None,
            no_store: false,
        }
    }

    /// Attach kind-specific fields (a JSON object). Each top-level
    /// key is merged into the envelope at render time. Keys MUST NOT
    /// be `"error"` or `"message"` (the helper does not overwrite the
    /// envelope-reserved fields).
    pub(crate) fn with_extra(mut self, extra: Value) -> Self {
        self.extra = Some(extra);
        self
    }

    /// Add `Cache-Control: no-store`. Used by CSRF and uniform-auth
    /// responses where intermediate caches must not stash the error.
    pub(crate) fn no_store(mut self) -> Self {
        self.no_store = true;
        self
    }

    /// Render to a `ntex::web::HttpResponse`. `error` and `message`
    /// always present; `extra` is merged at the top level.
    pub(crate) fn into_response(self) -> HttpResponse {
        let mut body = json!({
            "error": self.code,
            "message": self.message,
        });
        if let (Some(extra), Some(obj)) = (self.extra, body.as_object_mut()) {
            if let Some(extra_obj) = extra.as_object() {
                for (k, v) in extra_obj {
                    // Belt-and-suspenders: never let extra clobber the
                    // envelope-reserved keys.
                    if k == "error" || k == "message" {
                        continue;
                    }
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
        let mut resp = HttpResponse::build(self.status);
        if self.no_store {
            resp.header("Cache-Control", "no-store");
        }
        resp.json(&body)
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

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::*;
    use ntex::util::{stream_recv, BytesMut};

    /// Drain the response body and parse it as JSON. Used by every
    /// per-site test in this crate to assert the wire shape.
    pub(crate) async fn body_json(mut resp: HttpResponse) -> Value {
        let mut body = resp.take_body();
        let mut buf = BytesMut::new();
        while let Some(item) = stream_recv(&mut body).await {
            buf.extend_from_slice(&item.expect("body chunk"));
        }
        serde_json::from_slice(&buf).expect("body is JSON")
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers::body_json;
    use super::*;

    #[compio::test]
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

    #[compio::test]
    async fn extra_merges_at_top_level() {
        let resp = ErrorEnvelope::new(
            StatusCode::CONFLICT,
            "state_mismatch",
            "current state forbids this transition",
        )
        .with_extra(json!({"expected": "running", "current": "snapshotted"}))
        .into_response();
        let body = body_json(resp).await;
        assert_eq!(body["error"], "state_mismatch");
        assert_eq!(body["message"], "current state forbids this transition");
        assert_eq!(body["expected"], "running");
        assert_eq!(body["current"], "snapshotted");
    }

    #[compio::test]
    async fn extra_must_not_overwrite_reserved_keys() {
        let resp = ErrorEnvelope::new(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "real message",
        )
        .with_extra(json!({
            "error": "attacker_chose_this",
            "message": "attacker_chose_this_too",
            "fine": "ok"
        }))
        .into_response();
        let body = body_json(resp).await;
        assert_eq!(body["error"], "invalid_input", "reserved key must survive");
        assert_eq!(body["message"], "real message", "reserved key must survive");
        assert_eq!(body["fine"], "ok");
    }

    #[compio::test]
    async fn no_store_header_emitted_when_set() {
        let resp = ErrorEnvelope::new(
            StatusCode::FORBIDDEN,
            "csrf",
            "not a top-level navigation",
        )
        .no_store()
        .into_response();
        let cc = resp
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(cc, "no-store");
    }

    #[compio::test]
    async fn default_has_no_cache_control() {
        let resp = error_response(StatusCode::NOT_FOUND, "not_found", "x");
        assert!(resp.headers().get("cache-control").is_none());
    }
}
