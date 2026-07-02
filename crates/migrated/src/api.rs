use std::sync::Arc;

use ntex::http::StatusCode;
use ntex::web;
use ntex::web::types::{Json, Path, Query, State};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;
use zeroship_authz::Action;

use crate::apply::{
    apply_error_kind, apply_ir_documents, approve_pending_migration, ApplyMigrationsRequest,
    ApplyRequestError,
};
use crate::auth::AuthError;
use crate::policy::{CreatorPolicyDraft, ManagedPolicyError, MIGRATE_POLICY_FILENAME};
use crate::policy_store::AppPolicyStoreError;
use crate::MigrationServiceState;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/v1/apps/{app_id}/migrations/apply").route(web::post().to(apply)),
    )
    .service(
        web::resource("/v1/apps/{app_id}/migrations/{migration_id}/approve")
            .route(web::post().to(approve)),
    )
    .service(
        web::resource("/v1/apps/{app_id}/policy")
            .route(web::put().to(put_policy))
            .route(web::get().to(get_policy)),
    )
    .service(
        web::resource("/v1/apps/{app_id}/policy/versions")
            .route(web::get().to(list_policy_versions)),
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
    .service(web::resource("/health").route(web::get().to(health)));
}

pub async fn health() -> web::HttpResponse {
    web::HttpResponse::Ok().json(&json!({"ok": true}))
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
        .verify_action(token, app_id, Action::AppsDeploy)
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
        &state.policy_store,
        &state.migration_store,
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
                "migrated: applied frozen IR migrations"
            );
            web::HttpResponse::Ok().json(&report)
        }
        Err(err) => apply_error_response(err),
    }
}

pub async fn approve(
    req: web::HttpRequest,
    state: State<Arc<MigrationServiceState>>,
    path: Path<(Uuid, Uuid)>,
) -> web::HttpResponse {
    let Some(token) = bearer_token(&req) else {
        return web::HttpResponse::Unauthorized().json(&json!({"error": "unauthenticated"}));
    };
    let (app_id, migration_id) = path.into_inner();
    let caller = match state
        .authenticator
        .verify_action(token, app_id, Action::AppsApproveMigration)
        .await
    {
        Ok(caller) => caller,
        Err(err) => return auth_error_response(err),
    };

    match approve_pending_migration(
        &state.provision_dsn,
        &state.tmp_dir,
        &app_id,
        migration_id,
        &state.policy_config,
        &state.policy_store,
        &state.migration_store,
        caller.principal_id,
    )
    .await
    {
        Ok(report) => {
            tracing::info!(
                app_id = %app_id,
                migration_id = %migration_id,
                principal_id = %caller.principal_id,
                applied = report.applied.len(),
                skipped = report.skipped.len(),
                "migrated: approved and applied pending migration"
            );
            web::HttpResponse::Ok().json(&report)
        }
        Err(err) => apply_error_response(err),
    }
}

#[derive(Debug, Deserialize)]
pub struct GetPolicyQuery {
    version: Option<i64>,
}

pub async fn put_policy(
    req: web::HttpRequest,
    state: State<Arc<MigrationServiceState>>,
    app_id: Path<Uuid>,
    body: String,
) -> web::HttpResponse {
    let app_id = app_id.into_inner();
    let caller = match verify_apps_migrate(&req, &state, app_id).await {
        Ok(caller) => caller,
        Err(resp) => return resp,
    };
    let draft = CreatorPolicyDraft {
        filename: MIGRATE_POLICY_FILENAME,
        body: body.as_str(),
    };
    let parsed = match state.policy_config.parse_draft(&draft) {
        Ok(parsed) => parsed,
        Err(err) => return policy_validation_error_response(err),
    };
    let effective = match state
        .policy_config
        .compose_effective_for_app(&app_id, None, Some(&parsed))
    {
        Ok(effective) => effective,
        Err(err) => return policy_validation_error_response(err),
    };

    match state
        .policy_store
        .insert_version(app_id, caller.principal_id, &body, &parsed, &effective)
        .await
    {
        Ok(record) => {
            tracing::info!(
                app_id = %app_id,
                principal_id = %caller.principal_id,
                version = record.version,
                ceiling_version = record.ceiling_version,
                "migrated: stored creator migration policy"
            );
            web::HttpResponse::Ok().json(&record)
        }
        Err(err) => policy_store_error_response(err),
    }
}

pub async fn get_policy(
    req: web::HttpRequest,
    state: State<Arc<MigrationServiceState>>,
    app_id: Path<Uuid>,
    query: Query<GetPolicyQuery>,
) -> web::HttpResponse {
    let app_id = app_id.into_inner();
    if let Err(resp) = verify_apps_migrate(&req, &state, app_id).await {
        return resp;
    }
    if matches!(query.version, Some(version) if version <= 0) {
        return web::HttpResponse::BadRequest().json(&json!({
            "error": "invalid_policy_version",
            "detail": "policy version must be positive"
        }));
    }

    match state.policy_store.get(app_id, query.version).await {
        Ok(Some(record)) => web::HttpResponse::Ok().json(&record),
        Ok(None) => policy_not_found_response(),
        Err(err) => policy_store_error_response(err),
    }
}

pub async fn list_policy_versions(
    req: web::HttpRequest,
    state: State<Arc<MigrationServiceState>>,
    app_id: Path<Uuid>,
) -> web::HttpResponse {
    let app_id = app_id.into_inner();
    if let Err(resp) = verify_apps_migrate(&req, &state, app_id).await {
        return resp;
    }

    match state.policy_store.list_versions(app_id).await {
        Ok(versions) => web::HttpResponse::Ok().json(&json!({ "versions": versions })),
        Err(err) => policy_store_error_response(err),
    }
}

pub async fn stub_phase2() -> web::HttpResponse {
    web::HttpResponse::build(StatusCode::NOT_IMPLEMENTED).json(&json!({
        "error": "not_implemented",
        "detail": "migration plan/status/rollback endpoints are Phase 2"
    }))
}

fn bearer_token(req: &web::HttpRequest) -> Option<&str> {
    let header = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    zeroship_core::auth::extract_bearer(header)
}

async fn verify_apps_migrate(
    req: &web::HttpRequest,
    state: &Arc<MigrationServiceState>,
    app_id: Uuid,
) -> Result<crate::auth::VerifiedCaller, web::HttpResponse> {
    let Some(token) = bearer_token(req) else {
        return Err(web::HttpResponse::Unauthorized().json(&json!({"error": "unauthenticated"})));
    };
    state
        .authenticator
        .verify_action(token, app_id, Action::AppsDeploy)
        .await
        .map_err(auth_error_response)
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
    web::HttpResponse::build(status).json(&json!({
        "error": kind,
        "detail": err.to_string(),
        "migration_id": match &err {
            ApplyRequestError::ApprovalRequiredPending { migration_id, .. } => {
                Some(migration_id.to_string())
            }
            ApplyRequestError::ApprovalStaleCeiling { migration_id, .. }
            | ApplyRequestError::ApprovalPreflightChanged { migration_id, .. } => {
                Some(migration_id.to_string())
            }
            _ => None,
        },
        "gated_versions": match &err {
            ApplyRequestError::ApprovalRequiredPending { gated_versions, .. } => {
                Some(gated_versions)
            }
            ApplyRequestError::ApprovalPreflightChanged {
                current_gated_versions,
                ..
            } => Some(current_gated_versions),
            _ => None,
        },
    }))
}

fn policy_validation_error_response(err: ManagedPolicyError) -> web::HttpResponse {
    if err.is_creator_fault() {
        tracing::debug!(error = %err, "migrated: migration policy rejected");
        web::HttpResponse::UnprocessableEntity().json(&json!({
            "error": "migration_policy_invalid",
            "detail": err.to_string(),
        }))
    } else {
        tracing::error!(error = %err, "migrated: migration policy infrastructure error");
        web::HttpResponse::ServiceUnavailable().json(&json!({
            "error": "migration_policy_infrastructure",
            "detail": "migration policy service unavailable",
        }))
    }
}

fn policy_store_error_response(err: AppPolicyStoreError) -> web::HttpResponse {
    tracing::error!(error = %err, "migrated: app policy store failed");
    web::HttpResponse::ServiceUnavailable().json(&json!({
        "error": "policy_infrastructure",
        "detail": "migration policy service unavailable",
    }))
}

fn policy_not_found_response() -> web::HttpResponse {
    web::HttpResponse::NotFound().json(&json!({
        "error": "policy_not_found",
        "detail": "no migration policy version exists for this app",
    }))
}
