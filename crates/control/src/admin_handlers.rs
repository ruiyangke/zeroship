//! Platform-admin handlers for the platform staff role table.

use std::sync::Arc;

use ntex::web;
use ntex::web::types::{Json, Path, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;
use zeroship_auth::audit::{self as auth_audit, AuditEvent};
use zeroship_authz::{Action, EntityCache, Resource};

use crate::auth_audit as control_auth_audit;
use crate::authz_guard::AuthzGuard;
use crate::AppState;

pub(crate) const PLATFORM_ORG_ID: &str = "zeroship_platform";
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
        .control_pg
        .execute(
            "INSERT INTO zeroship.platform_admin_roles (user_id, role, granted_by) \
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
        .control_pg
        .execute("DELETE FROM zeroship.platform_admin_roles WHERE user_id = $1", &[&target])
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

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/admin/users/{user_id}/role")
            .route(web::post().to(grant_platform_role))
            .route(web::delete().to(revoke_platform_role))
            .route(web::get().to(get_platform_role)),
    );
}

/// The platform-operator gate every `/admin/*` route shares: `team:write` on
/// the synthetic platform org, which only `admin.cedar`'s universal allow
/// grants (no creator policy names an `Org` resource at all).
///
/// `pub(crate)` because the operator OAuth-client routes in `oauth_handlers`
/// use it too. They used to require `Action::PlatformPoliciesWrite`, which had
/// no entry in the OAuth scope vocabulary, so once personal access tokens were
/// removed no bearer could carry it and those three routes were unreachable.
/// That action is deleted; this gate is what every other operator route uses.
pub(crate) async fn require_platform_admin(
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
        .control_pg
        .query("SELECT 1 FROM zeroship.users WHERE id = $1", &[&user_id])
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
        .control_pg
        .query("SELECT role FROM zeroship.platform_admin_roles WHERE user_id = $1", &[&user_id])
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "control: platform role lookup failed");
            db_error()
        })?;
    Ok(rows.first().map(|row| row.get("role")))
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

    if let Err(err) = auth_audit::emit_strict(state.control_pg.as_ref(), &ev)
        .await
    {
        tracing::error!(error = %err, event_type, "control: platform admin audit insert failed");
        return Err(db_error());
    }

    Ok(())
}

fn db_error() -> web::HttpResponse {
    web::HttpResponse::InternalServerError().json(&json!({"error": "database error"}))
}
