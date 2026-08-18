//! Platform-admin handlers for roles and operator net policy.

use std::sync::Arc;

use ntex::web;
use ntex::web::types::{Json, Path, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;
use zeroship_auth::audit::{self as auth_audit, AuditEvent};
use zeroship_authz::{Action, EntityCache, Resource};
use zeroship_core::net_policy::{normalize_frontable_suffixes, FRONTABLE_WILDCARD_SUFFIXES};

use crate::auth_audit as control_auth_audit;
use crate::authz_guard::AuthzGuard;
use crate::net_grants;
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

#[derive(Debug, Deserialize)]
pub struct FrontableSuffixesBody {
    suffixes: Vec<String>,
}

#[derive(Serialize)]
struct FrontableSuffixesResponse {
    suffixes: Vec<String>,
    catalog_available: bool,
    backstop_suffixes: Vec<&'static str>,
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

pub async fn get_frontable_suffixes(
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Err(resp) = require_admin_or_support(&guard, &state).await {
        return resp;
    }
    match net_grants::load_frontable_suffix_catalog(state.control_pg.as_ref()).await {
        Ok(catalog) => web::HttpResponse::Ok().json(&FrontableSuffixesResponse {
            suffixes: catalog.suffixes,
            catalog_available: catalog.available,
            backstop_suffixes: FRONTABLE_WILDCARD_SUFFIXES.to_vec(),
        }),
        Err(err) => err.into_response(),
    }
}

pub async fn put_frontable_suffixes(
    req: web::HttpRequest,
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
    body: Json<FrontableSuffixesBody>,
) -> web::HttpResponse {
    if let Err(resp) = require_platform_admin(&guard, &state).await {
        return resp;
    }

    let suffixes = match normalize_frontable_suffixes(&body.suffixes) {
        Ok(suffixes) => suffixes,
        Err(err) => {
            return web::HttpResponse::BadRequest()
                .json(&json!({"error": "invalid suffix catalog", "detail": err}));
        }
    };
    let value = serde_json::to_value(&suffixes).expect("suffix Vec serializes");
    let updated_by = guard.principal_id.to_string();
    if let Err(err) = state
        .control_pg
        .execute(
            "INSERT INTO zeroship.net_policy_catalog (key, value_json, updated_by, updated_at) \
             VALUES ('frontable_wildcard_suffixes', $1, $2, NOW()) \
             ON CONFLICT (key) DO UPDATE SET \
                value_json = EXCLUDED.value_json, \
                updated_by = EXCLUDED.updated_by, \
                updated_at = NOW()",
            &[&value, &updated_by],
        )
        .await
    {
        tracing::error!(error = %err, "control: frontable suffix catalog update failed");
        return db_error();
    }

    if let Err(resp) = audit_event(
        &req,
        &state,
        &guard,
        "net_policy_frontable_suffixes_updated",
        json!({
            "actor": guard.principal_id,
            "suffix_count": suffixes.len(),
        }),
    )
    .await
    {
        return resp;
    }

    web::HttpResponse::Ok().json(&FrontableSuffixesResponse {
        suffixes,
        catalog_available: true,
        backstop_suffixes: FRONTABLE_WILDCARD_SUFFIXES.to_vec(),
    })
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/admin/users/{user_id}/role")
            .route(web::post().to(grant_platform_role))
            .route(web::delete().to(revoke_platform_role))
            .route(web::get().to(get_platform_role)),
    )
    .service(
        web::resource("/admin/net-policy/frontable-wildcard-suffixes")
            .route(web::get().to(get_frontable_suffixes))
            .route(web::put().to(put_frontable_suffixes)),
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
