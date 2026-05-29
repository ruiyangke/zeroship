//! Platform-admin handlers for roles and operator policy overrides.

use std::str::FromStr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ntex::web;
use ntex::web::types::{Json, Path, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroship_auth::audit::{self as auth_audit, AuditEvent};
use zeroship_authz::{Action, EntityCache, PolicySet, Resource};

use crate::auth_audit as control_auth_audit;
use crate::authz_guard::AuthzGuard;
use crate::AppState;

const PLATFORM_ORG_ID: &str = "zeroship_platform";
const ROLE_ADMIN: &str = "admin";
const ROLE_SUPPORT: &str = "support";
const ROLE_BILLING: &str = "billing";
const ROLE_READONLY: &str = "readonly";

#[derive(Debug, Deserialize)]
pub struct RoleBody {
    role: String,
}

#[derive(Serialize)]
struct RoleResponse {
    role: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PlatformPolicyBody {
    cedar_source: String,
    enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct AuditLockBody {
    audit_locked: bool,
}

#[derive(Debug, Deserialize)]
pub struct SuspensionBody {
    suspended: bool,
}

#[derive(Serialize)]
struct PlatformPolicySummary {
    id: String,
    enabled: bool,
    updated_at: DateTime<Utc>,
    updated_by: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cedar_source: Option<String>,
}

#[derive(Serialize)]
struct AppAuditLockResponse {
    id: Uuid,
    audit_locked: bool,
}

#[derive(Serialize)]
struct AppSuspensionResponse {
    id: Uuid,
    suspended: bool,
}

pub async fn grant_platform_role(
    req: web::HttpRequest,
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
    user_id: Path<String>,
    body: Json<RoleBody>,
) -> web::HttpResponse {
    if let Err(resp) = require_platform_admin(&guard, &state).await {
        return resp;
    }

    let target = match parse_uuid(&user_id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let role = match validate_role(&body.role) {
        Some(role) => role,
        None => {
            return web::HttpResponse::BadRequest()
                .json(&json!({"error": "invalid platform role"}))
        }
    };

    match user_exists(&state, target).await {
        Ok(true) => {}
        Ok(false) => {
            return web::HttpResponse::NotFound().json(&json!({"error": "user not found"}))
        }
        Err(resp) => return resp,
    }

    if let Err(err) = state
        .auth_pg
        .execute(
            "INSERT INTO platform.roles (user_id, role, granted_by) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (user_id) DO UPDATE \
             SET role = $2, granted_by = $3, granted_at = NOW()",
            &[&target, &role, &guard.principal_id],
        )
        .await
    {
        tracing::error!(error = %err, "control: platform role grant failed");
        return db_error();
    }
    EntityCache::invalidate(target);

    if let Err(resp) = audit_event(
        &req,
        &state,
        &guard,
        "platform_role_granted",
        json!({
            "actor": guard.principal_id,
            "target": target,
            "role": role,
        }),
    )
    .await
    {
        return resp;
    }

    web::HttpResponse::NoContent().finish()
}

pub async fn revoke_platform_role(
    req: web::HttpRequest,
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
    user_id: Path<String>,
) -> web::HttpResponse {
    if let Err(resp) = require_platform_admin(&guard, &state).await {
        return resp;
    }

    let target = match parse_uuid(&user_id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    if let Err(err) = state
        .auth_pg
        .execute("DELETE FROM platform.roles WHERE user_id = $1", &[&target])
        .await
    {
        tracing::error!(error = %err, "control: platform role revoke failed");
        return db_error();
    }
    EntityCache::invalidate(target);

    if let Err(resp) = audit_event(
        &req,
        &state,
        &guard,
        "platform_role_revoked",
        json!({
            "actor": guard.principal_id,
            "target": target,
        }),
    )
    .await
    {
        return resp;
    }

    web::HttpResponse::NoContent().finish()
}

pub async fn get_platform_role(
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
    user_id: Path<String>,
) -> web::HttpResponse {
    if let Err(resp) = require_admin_or_support(&guard, &state).await {
        return resp;
    }

    let target = match parse_uuid(&user_id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    match user_exists(&state, target).await {
        Ok(true) => {}
        Ok(false) => {
            return web::HttpResponse::NotFound().json(&json!({"error": "user not found"}))
        }
        Err(resp) => return resp,
    }

    match platform_role(&state, target).await {
        Ok(role) => web::HttpResponse::Ok().json(&RoleResponse { role }),
        Err(resp) => resp,
    }
}

pub async fn set_app_audit_lock(
    req: web::HttpRequest,
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
    app_id: Path<String>,
    body: Json<AuditLockBody>,
) -> web::HttpResponse {
    if let Err(resp) = require_platform_admin(&guard, &state).await {
        return resp;
    }

    let app_id = match parse_app_uuid(&app_id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    let rows = match state
        .auth_pg
        .query(
            "UPDATE apps SET audit_locked = $1, updated_at = NOW() \
             WHERE id = $2 \
             RETURNING audit_locked",
            &[&body.audit_locked, &app_id],
        )
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(error = %err, app_id = %app_id, "control: app audit lock update failed");
            return db_error();
        }
    };
    let Some(row) = rows.first() else {
        return web::HttpResponse::NotFound().json(&json!({"error": "app not found"}));
    };
    EntityCache::invalidate_resource(&Resource::App {
        id: app_id.to_string(),
    });

    if let Err(resp) = audit_event(
        &req,
        &state,
        &guard,
        "app_audit_lock_updated",
        json!({
            "actor": guard.principal_id,
            "app_id": app_id,
            "audit_locked": body.audit_locked,
        }),
    )
    .await
    {
        return resp;
    }

    web::HttpResponse::Ok().json(&AppAuditLockResponse {
        id: app_id,
        audit_locked: row.get("audit_locked"),
    })
}

pub async fn set_app_suspension(
    req: web::HttpRequest,
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
    app_id: Path<String>,
    body: Json<SuspensionBody>,
) -> web::HttpResponse {
    if let Err(resp) = require_platform_admin(&guard, &state).await {
        return resp;
    }

    let app_id = match parse_app_uuid(&app_id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    let rows = match state
        .auth_pg
        .query(
            "UPDATE apps SET suspended = $1, updated_at = NOW() \
             WHERE id = $2 \
             RETURNING suspended",
            &[&body.suspended, &app_id],
        )
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(error = %err, app_id = %app_id, "control: app suspension update failed");
            return db_error();
        }
    };
    let Some(row) = rows.first() else {
        return web::HttpResponse::NotFound().json(&json!({"error": "app not found"}));
    };
    EntityCache::invalidate_resource(&Resource::App {
        id: app_id.to_string(),
    });

    if let Err(resp) = audit_event(
        &req,
        &state,
        &guard,
        "app_suspension_updated",
        json!({
            "actor": guard.principal_id,
            "app_id": app_id,
            "suspended": body.suspended,
        }),
    )
    .await
    {
        return resp;
    }

    web::HttpResponse::Ok().json(&AppSuspensionResponse {
        id: app_id,
        suspended: row.get("suspended"),
    })
}

pub async fn upsert_platform_policy(
    req: web::HttpRequest,
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
    id: Path<String>,
    body: Json<PlatformPolicyBody>,
) -> web::HttpResponse {
    if let Err(resp) = require_platform_admin(&guard, &state).await {
        return resp;
    }
    if let Err(err) = PolicySet::from_str(&body.cedar_source) {
        return web::HttpResponse::BadRequest()
            .json(&json!({"error": "invalid cedar policy", "detail": err.to_string()}));
    }

    let policy_id = id.into_inner();
    let previous = match load_platform_policy(&state, &policy_id).await {
        Ok(previous) => previous,
        Err(resp) => return resp,
    };

    if let Err(err) = state
        .auth_pg
        .execute(
            "INSERT INTO control.platform_policies (id, cedar_source, enabled, updated_by) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (id) DO UPDATE \
             SET cedar_source = $2, enabled = $3, updated_by = $4, updated_at = NOW()",
            &[&policy_id, &body.cedar_source, &body.enabled, &guard.principal_id],
        )
        .await
    {
        tracing::error!(error = %err, policy_id = %policy_id, "control: platform policy upsert failed");
        return db_error();
    }

    let diff = json!({
        "id": policy_id,
        "old_enabled": previous.as_ref().map(|p| p.enabled),
        "new_enabled": body.enabled,
        "old_cedar_sha256": previous.as_ref().map(|p| sha256_hex(&p.cedar_source)),
        "new_cedar_sha256": sha256_hex(&body.cedar_source),
    });
    if let Err(resp) = audit_event(
        &req,
        &state,
        &guard,
        "platform_policy_updated",
        json!({
            "actor": guard.principal_id,
            "diff": diff,
        }),
    )
    .await
    {
        return resp;
    }

    web::HttpResponse::NoContent().finish()
}

pub async fn delete_platform_policy(
    req: web::HttpRequest,
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
    id: Path<String>,
) -> web::HttpResponse {
    if let Err(resp) = require_platform_admin(&guard, &state).await {
        return resp;
    }

    let policy_id = id.into_inner();
    if let Err(err) = state
        .auth_pg
        .execute("DELETE FROM control.platform_policies WHERE id = $1", &[&policy_id])
        .await
    {
        tracing::error!(error = %err, policy_id = %policy_id, "control: platform policy delete failed");
        return db_error();
    }

    if let Err(resp) = audit_event(
        &req,
        &state,
        &guard,
        "platform_policy_deleted",
        json!({
            "actor": guard.principal_id,
            "id": policy_id,
        }),
    )
    .await
    {
        return resp;
    }

    web::HttpResponse::NoContent().finish()
}

pub async fn list_platform_policies(
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let caller_role = match require_admin_or_support(&guard, &state).await {
        Ok(role) => role,
        Err(resp) => return resp,
    };
    let include_source = caller_role == ROLE_ADMIN;

    let rows = match state
        .auth_pg
        .query(
            "SELECT id, cedar_source, enabled, updated_at, updated_by \
             FROM control.platform_policies \
             ORDER BY id ASC",
            &[],
        )
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(error = %err, "control: platform policy list failed");
            return db_error();
        }
    };

    let policies = rows
        .iter()
        .map(|row| PlatformPolicySummary {
            id: row.get("id"),
            enabled: row.get("enabled"),
            updated_at: row.get("updated_at"),
            updated_by: row.get("updated_by"),
            cedar_source: include_source.then(|| row.get("cedar_source")),
        })
        .collect::<Vec<_>>();

    web::HttpResponse::Ok().json(&policies)
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/admin/users/{user_id}/role")
            .route(web::post().to(grant_platform_role))
            .route(web::delete().to(revoke_platform_role))
            .route(web::get().to(get_platform_role)),
    )
    .service(
        web::resource("/admin/apps/{app_id}/audit-lock")
            .route(web::post().to(set_app_audit_lock)),
    )
    .service(
        web::resource("/admin/apps/{app_id}/suspend")
            .route(web::post().to(set_app_suspension)),
    )
    .service(
        web::resource("/admin/platform-policies")
            .route(web::get().to(list_platform_policies)),
    )
    .service(
        web::resource("/admin/platform-policies/{id}")
            .route(web::put().to(upsert_platform_policy))
            .route(web::delete().to(delete_platform_policy)),
    );
}

async fn require_platform_admin(
    guard: &AuthzGuard,
    state: &AppState,
) -> Result<(), web::HttpResponse> {
    guard
        .require(
            Action::TeamWrite,
            Resource::Org {
                id: PLATFORM_ORG_ID.to_owned(),
            },
            state,
        )
        .await
}

async fn require_admin_or_support(
    guard: &AuthzGuard,
    state: &AppState,
) -> Result<String, web::HttpResponse> {
    match platform_role(state, guard.principal_id).await? {
        Some(role) if role == ROLE_ADMIN || role == ROLE_SUPPORT => Ok(role),
        _ => Err(web::HttpResponse::Forbidden().json(&json!({"error": "forbidden"}))),
    }
}

fn parse_uuid(raw: &str) -> Result<Uuid, web::HttpResponse> {
    Uuid::parse_str(raw)
        .map_err(|_| web::HttpResponse::BadRequest().json(&json!({"error": "invalid user id"})))
}

fn parse_app_uuid(raw: &str) -> Result<Uuid, web::HttpResponse> {
    Uuid::parse_str(raw)
        .map_err(|_| web::HttpResponse::BadRequest().json(&json!({"error": "invalid app id"})))
}

fn validate_role(role: &str) -> Option<&'static str> {
    match role {
        ROLE_ADMIN => Some(ROLE_ADMIN),
        ROLE_SUPPORT => Some(ROLE_SUPPORT),
        ROLE_BILLING => Some(ROLE_BILLING),
        ROLE_READONLY => Some(ROLE_READONLY),
        _ => None,
    }
}

async fn user_exists(state: &AppState, user_id: Uuid) -> Result<bool, web::HttpResponse> {
    let rows = state
        .auth_pg
        .query("SELECT 1 FROM auth.users WHERE id = $1", &[&user_id])
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "control: user lookup failed");
            db_error()
        })?;
    Ok(!rows.is_empty())
}

async fn platform_role(
    state: &AppState,
    user_id: Uuid,
) -> Result<Option<String>, web::HttpResponse> {
    let rows = state
        .auth_pg
        .query("SELECT role FROM platform.roles WHERE user_id = $1", &[&user_id])
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "control: platform role lookup failed");
            db_error()
        })?;
    Ok(rows.first().map(|row| row.get("role")))
}

struct ExistingPlatformPolicy {
    cedar_source: String,
    enabled: bool,
}

async fn load_platform_policy(
    state: &AppState,
    id: &str,
) -> Result<Option<ExistingPlatformPolicy>, web::HttpResponse> {
    let rows = state
        .auth_pg
        .query(
            "SELECT cedar_source, enabled FROM control.platform_policies WHERE id = $1",
            &[&id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, policy_id = %id, "control: platform policy lookup failed");
            db_error()
        })?;
    Ok(rows.first().map(|row| ExistingPlatformPolicy {
        cedar_source: row.get("cedar_source"),
        enabled: row.get("enabled"),
    }))
}

async fn audit_event(
    req: &web::HttpRequest,
    state: &AppState,
    guard: &AuthzGuard,
    event_type: &str,
    detail: serde_json::Value,
) -> Result<(), web::HttpResponse> {
    let user_agent = req
        .headers()
        .get("user-agent")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let ev = AuditEvent {
        event_type,
        outcome: "success",
        user_id: Some(&guard.principal_id),
        request_id: Some(guard.request_id.clone()),
        ip: guard.request_ip,
        user_agent,
        auth_method: Some(control_auth_audit::auth_method(guard)),
        detail,
        ..Default::default()
    };

    if let Err(err) = auth_audit::emit_strict(state.auth_pg.as_ref(), &ev)
        .await
    {
        tracing::error!(error = %err, event_type, "control: platform admin audit insert failed");
        return Err(db_error());
    }

    Ok(())
}

fn sha256_hex(source: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    hex::encode(hasher.finalize())
}

fn db_error() -> web::HttpResponse {
    web::HttpResponse::InternalServerError().json(&json!({"error": "database error"}))
}
