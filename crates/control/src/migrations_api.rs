//! The creator-facing entry to the migration service.
//!
//! `POST /api/apps/{id}/migrations/apply` authorizes the caller for the app
//! named in ITS OWN path segment and then forwards the request to
//! `zeroship-migrated`'s `POST /v1/apps/{app_id}/migrations/apply`.
//!
//! # Why control is in this path at all
//!
//! `migrated` already authenticates and authorizes creators correctly:
//! `crates/migrated/src/api.rs` verifies the bearer and requires
//! `Action::AppsDeploy` on the app, and `crates/migrated/src/auth.rs`
//! additionally requires a `role = 'owner'` row in `zeroship.app_members`. A
//! creator PAT is a first-class caller there, not an operator-only surface -
//! `deploy/compose/docker-compose.yml` says so in as many words on the
//! `ZEROSHIP_MIGRATED_SIGNING_KEY_FILE` line: "MUST be the same file control
//! signs with, or a control-issued creator PAT will not verify here".
//!
//! What a creator does NOT have is a route. `migrated` binds loopback in every
//! deployment we ship (`ports: 127.0.0.1:9091:9091`) because it holds the
//! SUPERUSER provisioning DSN - it is the one service that may `CREATE SCHEMA`
//! and `CREATE ROLE`. The compose comment above that service states the
//! intended topology outright: "nothing outside the compose network should
//! reach it. Creators drive it through control". This module is that hop; it
//! was the only piece missing.
//!
//! # What this hop is allowed to add: nothing
//!
//! Control forwards the CALLER'S OWN bearer, never `control_key` and never a
//! minted service credential. `migrated` re-verifies it from scratch against
//! the shared signing key and re-runs the same Cedar decision plus its
//! owner-row check. So control contributes reachability and a first, cheap
//! rejection - it contributes no authority. If this handler were tricked into
//! forwarding a body for an app the caller does not own, `migrated` would still
//! answer 403.
//!
//! The app id sent downstream is the one control authorized, parsed from its
//! own path segment. Nothing in the creator-supplied body can retarget it: the
//! body is forwarded opaquely as bytes and is never parsed for an id here.

use std::sync::Arc;
use std::time::Duration;

use ntex::http::StatusCode;
use ntex::web;
use ntex::web::types::{Path, State};
use uuid::Uuid;
use zeroship_authz::{Action, Resource};

use crate::authz_guard::AuthzGuard;
use crate::AppState;

/// Payload ceiling for an apply request.
///
/// The body is a JSON envelope of recorded migration IR - one document per
/// committed `.ts` migration. 8 MiB is far above any real migration set and far
/// below a memory hazard; a creator who exceeds it has a build problem, not a
/// migration problem.
pub const MIGRATIONS_APPLY_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

/// How long control waits for `migrated` to answer.
///
/// An apply runs DDL: `CREATE SCHEMA`, `CREATE ROLE`, then the migration set
/// itself, all inside `migrated`. The 2s used for worker log fan-out would time
/// out a perfectly healthy first apply. This is deliberately generous because
/// the failure mode it guards against is a hung service, not a slow one, and
/// answering "timeout" over a migration that is still applying would tell the
/// creator to retry into a half-applied schema.
const APPLY_TIMEOUT: Duration = Duration::from_secs(300);

pub async fn apply_migrations(
    req: web::HttpRequest,
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
    body: String,
) -> web::HttpResponse {
    // Per-IP throttle FIRST, sharing the `admin` bucket with the other mutating
    // creator surfaces. This one earns it more than most: each accepted request
    // holds a control task for up to APPLY_TIMEOUT and opens a DDL session on
    // the provisioning DSN downstream, so an unthrottled loop here costs far
    // more than an unthrottled loop against a read endpoint.
    if let Some(resp) = crate::env_handlers::admin_rate_limit(&req, &state).await {
        return resp;
    }

    let Ok(uid) = id.parse::<Uuid>() else {
        return web::HttpResponse::BadRequest()
            .json(&serde_json::json!({"error": "invalid uuid"}));
    };

    // Same action the deploy handler requires, on the same resource. Applying
    // migrations is part of shipping a version of the app, and a creator who
    // may deploy may migrate - there is no separate scope to hold, and inventing
    // one would leave every already-issued `zeroship login` token unable to run
    // the step its own deploy needs.
    if let Err(resp) = authz
        .require(Action::AppsDeploy, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }

    let Some(bearer) = bearer_token(&req) else {
        // Unreachable in practice - `AuthzGuard` already rejected a request
        // with no bearer - but the forward below must never go out unsigned, so
        // it is refused here rather than assumed.
        return web::HttpResponse::Unauthorized()
            .json(&serde_json::json!({"error": "unauthenticated"}));
    };

    match forward_apply(
        &state.migrated_url,
        &uid,
        bearer,
        &authz.request_id,
        &body,
    )
    .await
    {
        Ok(response) => {
            // migrated's status and body are passed through VERBATIM. Its error
            // taxonomy is the creator's diagnostic surface: 422 names the
            // malformed document, 409 names the migration awaiting approval,
            // 403 says the caller does not own the app. Collapsing those into a
            // control-flavoured error would reproduce the exact failure this
            // endpoint exists to end - an opaque message over a specific cause.
            web::HttpResponse::build(
                StatusCode::from_u16(response.status).unwrap_or(StatusCode::BAD_GATEWAY),
            )
            .content_type("application/json")
            .body(response.body)
        }
        Err(detail) => {
            tracing::error!(
                app_id = %uid,
                error = %detail,
                "control: migration service unreachable"
            );
            web::HttpResponse::BadGateway().json(&serde_json::json!({
                "error": "migration_service_unavailable",
                "detail": detail,
            }))
        }
    }
}

fn bearer_token(req: &web::HttpRequest) -> Option<&str> {
    let header = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    zeroship_core::auth::extract_bearer(header)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForwardedResponse {
    pub status: u16,
    pub body: String,
}

/// POST the opaque body to `migrated`, carrying the caller's own bearer.
///
/// `app_id` is a `Uuid`, not a string, so the downstream path segment cannot
/// carry anything the caller wrote.
async fn forward_apply(
    migrated_url: &str,
    app_id: &Uuid,
    bearer: &str,
    request_id: &str,
    body: &str,
) -> Result<ForwardedResponse, String> {
    let url = format!(
        "{}/v1/apps/{app_id}/migrations/apply",
        migrated_url.trim_end_matches('/')
    );
    let client = cyper::Client::new();
    let builder = client
        .post(&url)
        .map_err(|e| format!("invalid migration service URL: {e}"))?
        .header("authorization", &format!("Bearer {bearer}"))
        .map_err(|e| format!("invalid auth header: {e}"))?
        .header("content-type", "application/json")
        .map_err(|e| format!("invalid content-type header: {e}"))?
        // The correlation id control already knows this request by. migrated
        // honours an inbound `x-request-id` and stamps it on its authz audit
        // row, so without this the two services audit the same apply under two
        // unrelated ids.
        .header("x-request-id", request_id)
        .map_err(|e| format!("invalid request-id header: {e}"))?
        .body(body.to_owned());

    let response = compio::time::timeout(APPLY_TIMEOUT, builder.send())
        .await
        .map_err(|_| "migration service request timed out".to_string())?
        .map_err(|e| format!("migration service request failed: {e}"))?;
    let status = response.status().as_u16();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("read migration service body: {e}"))?;
    Ok(ForwardedResponse {
        status,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    })
}
