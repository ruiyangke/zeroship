//! `/consent` GET handler. Phase 2: first-party clients (`skip_consent=true`).
//! Third-party consent UI is Phase 4+.
//!
//! Algorithm:
//!
//! 1. Read `consent_challenge` from the query.
//! 2. Fetch challenge metadata via `admin.get_consent(challenge)`.
//! 3. If `info.client.skip_consent` is false → render the generic error page
//!    explaining that third-party consent UI is not yet implemented.
//! 4. If `info.client.skip_consent` is true → build an `AcceptConsentRequest`
//!    granting the requested scopes/audience verbatim, with an empty ID-token
//!    session payload (RPs hit `/userinfo` for user-derived claims in Phase 2;
//!    Phase 5 will populate `id_token` with email/name from the user row).
//!    Call `accept_consent` and 302 to hydra's `redirect_to`.
//!
//! Route wiring happens in P2-U6; the handler here is a plain `pub async fn`
//! with no `#[ntex::web::*]` attribute.

use ntex::http::header::{HeaderValue, LOCATION};
use ntex::web::HttpResponse;
use serde::Deserialize;
use serde_json::json;

use crate::hydra_client::types::{AcceptConsentRequest, ConsentSession};
use crate::hydra_client::HydraAdmin;

#[derive(Debug, Deserialize)]
pub struct ConsentQuery {
    pub consent_challenge: String,
}

// ntex's per-thread service futures are intentionally `!Send`.
#[allow(clippy::future_not_send)]
pub async fn get(
    query: ntex::web::types::Query<ConsentQuery>,
    admin: ntex::web::types::State<HydraAdmin>,
) -> HttpResponse {
    let challenge = &query.consent_challenge;

    let info = match admin.get_consent(challenge).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, challenge = %challenge, "consent challenge fetch failed");
            return error_response(&e.to_string());
        }
    };

    // Phase 2: only handle the skip path. Third-party UI lands in Phase 4.
    if !info.client.skip_consent {
        return error_response("third-party consent UI not yet implemented");
    }

    let accept = AcceptConsentRequest {
        grant_scope: info.requested_scope.clone(),
        grant_access_token_audience: info.requested_access_token_audience.clone(),
        remember: Some(true),
        remember_for: Some(3600),
        session: Some(ConsentSession {
            // Phase 2: empty ID-token claims; RPs hit /userinfo for the rest.
            // Phase 5 will populate this with email/name from the user row.
            id_token: Some(json!({})),
            // Intentionally empty per §13 "session.access_token leakage" row.
            access_token: None,
        }),
    };

    match admin.accept_consent(challenge, &accept).await {
        Ok(resp) => {
            let mut r = HttpResponse::Found();
            r.header(
                LOCATION,
                HeaderValue::from_str(&resp.redirect_to)
                    .unwrap_or_else(|_| HeaderValue::from_static("/")),
            );
            r.finish()
        }
        Err(e) => {
            tracing::error!(error = %e, "accept_consent failed");
            error_response(&e.to_string())
        }
    }
}

fn error_response(msg: &str) -> HttpResponse {
    use crate::ui::ErrorPage;
    use askama::Template;
    let page = ErrorPage {
        error: "Consent failed",
        error_description: Some(msg),
    };
    let body = page.render().unwrap_or_else(|_| format!("<h1>{msg}</h1>"));
    let mut r = HttpResponse::Ok();
    r.content_type("text/html; charset=utf-8");
    r.body(body)
}
