use std::sync::Arc;

use ntex::http::StatusCode;
use ntex::web;
use ntex::web::types::{Json, Path, State};
use serde_json::json;
use uuid::Uuid;
use zeroship_authz::Action;

use crate::apply::{
    apply_error_kind, apply_ir_documents, ApplyMigrationsRequest, ApplyRequestError,
};
use crate::auth::AuthError;
use crate::MigrationServiceState;

/// THE APPROVAL AND POLICY ENDPOINTS ARE GONE, and their absence is the change
/// rather than a gap.
///
/// `POST /v1/apps/{app}/migrations/{migration}/approve` drove a state machine no
/// caller could reach: the level it gated on started at `Never` and only ever
/// loosened on a policy rule no artifact in the tree sets. Removing it removes a
/// capability - operator approval of destructive creator migrations - deliberately.
///
/// `PUT/GET /v1/apps/{app}/policy` and `GET .../policy/versions` wrote and read
/// `zeroship.migrated_app_policies`, a table this change deletes with them. A
/// migration policy now arrives with the apply request, declared in the
/// creator's repository and folded at build time, so there is nothing to store
/// and nothing to mutate at runtime.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/v1/apps/{app_id}/migrations/apply").route(web::post().to(apply)),
    )
    .service(
        web::resource("/v1/apps/{app_id}/migrations/plan").route(web::post().to(stub_phase2)),
    )
    .service(
        web::resource("/v1/apps/{app_id}/migrations/status").route(web::get().to(stub_phase2)),
    )
    .service(
        web::resource("/v1/apps/{app_id}/migrations/rollback").route(web::post().to(stub_phase2)),
    )
    .service(web::resource("/healthz").route(web::get().to(healthz)))
    .service(web::resource("/readyz").route(web::get().to(readyz)));
}

/// Liveness. Constant 200 by design: it must not touch Postgres, or a database
/// blip would get this container killed on top of the outage.
pub async fn healthz() -> web::HttpResponse {
    web::HttpResponse::Ok().json(&json!({"ok": true}))
}

/// Readiness. Every endpoint this service exposes reads or writes Postgres, so
/// an unreachable database means it cannot serve. The probe is bounded,
/// cached and single-flighted; the body carries no DSN and no driver text.
///
/// A process that booted on the credential dev escape is NOT ready, whatever
/// Postgres says: the seal key it would confine a creator migration with is a
/// placeholder. That check is short-circuited FIRST so a doomed process does
/// not also generate database probes.
pub async fn readyz(state: State<Arc<MigrationServiceState>>) -> web::HttpResponse {
    if zeroship_core::config::dev_escape_active() {
        return web::HttpResponse::ServiceUnavailable().json(&json!({"ready": false}));
    }
    let ready = state
        .readiness
        .ready(|| async {
            match state.schema_apply_store.probe().await {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "migrate-server readiness: postgres unreachable"
                    );
                    false
                }
            }
        })
        .await;
    if ready {
        web::HttpResponse::Ok().json(&json!({"ready": true}))
    } else {
        web::HttpResponse::ServiceUnavailable().json(&json!({"ready": false}))
    }
}

pub async fn apply(
    req: web::HttpRequest,
    state: State<Arc<MigrationServiceState>>,
    app_id: Path<Uuid>,
    body: Json<ApplyMigrationsRequest>,
) -> web::HttpResponse {
    let Some(token) = bearer_token(&req) else {
        return web::HttpResponse::Unauthorized().json(&json!({"error": "unauthenticated"}));
    };
    let app_id = app_id.into_inner();
    let caller = match state
        .authenticator
        .verify_action(token, app_id, Action::AppsDeploy, &request_id(&req))
        .await
    {
        Ok(caller) => caller,
        Err(err) => return auth_error_response(err),
    };

    match apply_ir_documents(
        &state.provision_dsn,
        &state.tmp_dir,
        &app_id,
        &body,
        &state.policy_config,
        &state.schema_apply_store,
        caller.principal_id,
    )
    .await
    {
        Ok(report) => {
            tracing::info!(
                app_id = %app_id,
                principal_id = %caller.principal_id,
                applied = report.applied.len(),
                skipped = report.skipped.len(),
                "migrate-server: applied frozen IR migrations"
            );
            web::HttpResponse::Ok().json(&report)
        }
        Err(err) => apply_error_response(err),
    }
}


pub async fn stub_phase2() -> web::HttpResponse {
    web::HttpResponse::build(StatusCode::NOT_IMPLEMENTED).json(&json!({
        "error": "not_implemented",
        "detail": "migration plan/status/rollback endpoints are Phase 2"
    }))
}

/// The correlation id for this request, stamped onto the authz audit row.
///
/// Honours an inbound `x-request-id` so the id in our audit matches the one the
/// caller and the rest of the platform already know the request by; mints one
/// only when the caller sent none. Minting unconditionally would produce an
/// audit trail whose ids appear in no other log. Mirrors the control plane's
/// `request_id` helper so the two services agree on the identifier.
fn request_id(req: &web::HttpRequest) -> String {
    req.headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string())
}

fn bearer_token(req: &web::HttpRequest) -> Option<&str> {
    let header = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    zeroship_core::auth::extract_bearer(header)
}


fn auth_error_response(err: AuthError) -> web::HttpResponse {
    match err {
        AuthError::Unauthorized => {
            web::HttpResponse::Unauthorized().json(&json!({"error": "unauthenticated"}))
        }
        AuthError::Forbidden => web::HttpResponse::Forbidden().json(&json!({"error": "forbidden"})),
        AuthError::Infrastructure(detail) => {
            tracing::error!(error = %detail, "migrated: auth infrastructure error");
            web::HttpResponse::InternalServerError().json(&json!({"error": "auth_error"}))
        }
    }
}

fn apply_error_response(err: ApplyRequestError) -> web::HttpResponse {
    let (status, kind) = apply_error_kind(&err);
    if status.is_server_error() {
        tracing::error!(error = %err, "migrated: migration apply failed");
        return web::HttpResponse::build(status).json(&json!({
            "error": kind,
            "detail": "migration service unavailable",
        }));
    } else {
        tracing::debug!(error = %err, "migrated: migration request rejected");
    }
    // `migration_id` and `gated_versions` used to ride along here, and both existed
    // only for the approval refusals: the id so an operator could approve that row,
    // the version list so they knew what they were approving. With no approval
    // endpoint there is no row to name and no decision to inform, so a body that
    // still carried them would be describing an action the caller cannot take.
    web::HttpResponse::build(status).json(&json!({
        "error": kind,
        "detail": err.to_string(),
    }))
}

