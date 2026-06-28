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
use zeroship_core::net_policy::{
    normalize_frontable_suffixes, HostPort, FRONTABLE_WILDCARD_SUFFIXES,
};

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

#[derive(Debug, Deserialize)]
pub struct NetGrantBody {
    host: String,
    port: u16,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FrontableSuffixesBody {
    suffixes: Vec<String>,
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

#[derive(Serialize)]
struct AppNetGrantResponse {
    app_id: Uuid,
    host: String,
    port: u16,
    granted_by: String,
    granted_at: DateTime<Utc>,
    note: Option<String>,
}

#[derive(Clone, Serialize)]
struct AppNetRequestResponse {
    host: String,
    port: u16,
    reason: String,
}

#[derive(Serialize)]
struct AppNetGrantListResponse {
    app_id: Uuid,
    grants: Vec<AppNetGrantResponse>,
    requests: Vec<AppNetRequestResponse>,
    pending_requests: Vec<AppNetRequestResponse>,
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
        .control_pg
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
        .control_pg
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

pub async fn list_app_net_grants(
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
    app_id: Path<String>,
) -> web::HttpResponse {
    if let Err(resp) = require_admin_or_support(&guard, &state).await {
        return resp;
    }

    let app_id = match parse_app_uuid(&app_id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let manifest_json = match load_app_manifest_json(&state, app_id).await {
        Ok(Some(json)) => json,
        Ok(None) => {
            return web::HttpResponse::NotFound().json(&json!({"error": "app not found"}));
        }
        Err(resp) => return resp,
    };

    let rows = match state
        .control_pg
        .query(
            "SELECT app_id, host, port, granted_by, granted_at, note \
             FROM zeroship.app_net_grants \
             WHERE app_id = $1 \
             ORDER BY host ASC, port ASC",
            &[&app_id],
        )
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(error = %err, app_id = %app_id, "control: app net grants list failed");
            return db_error();
        }
    };
    let grants = rows.iter().map(row_to_net_grant).collect::<Vec<_>>();
    let requests = manifest_net_requests(manifest_json.as_deref(), app_id);
    let granted_keys = grants
        .iter()
        .map(|g| (normalize_host_text(&g.host), g.port))
        .collect::<std::collections::HashSet<_>>();
    let pending_requests = requests
        .iter()
        .filter(|r| !granted_keys.contains(&(normalize_host_text(&r.host), r.port)))
        .cloned()
        .collect::<Vec<_>>();

    web::HttpResponse::Ok().json(&AppNetGrantListResponse {
        app_id,
        grants,
        requests,
        pending_requests,
    })
}

pub async fn grant_app_net(
    req: web::HttpRequest,
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
    app_id: Path<String>,
    body: Json<NetGrantBody>,
) -> web::HttpResponse {
    if let Err(resp) = require_platform_admin(&guard, &state).await {
        return resp;
    }

    let app_id = match parse_app_uuid(&app_id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match app_exists(&state, app_id).await {
        Ok(true) => {}
        Ok(false) => {
            return web::HttpResponse::NotFound().json(&json!({"error": "app not found"}));
        }
        Err(resp) => return resp,
    }

    let catalog = match load_frontable_suffix_catalog(&state).await {
        Ok(catalog) => catalog,
        Err(resp) => return resp,
    };
    let reviewed = match HostPort::try_new_with_frontable_suffixes(
        body.host.clone(),
        body.port,
        &catalog.suffixes,
        catalog.available,
    ) {
        Ok(hp) => hp,
        Err(err) => {
            return web::HttpResponse::BadRequest().json(&json!({
                "error": "invalid net grant",
                "detail": err,
            }));
        }
    };
    let host = reviewed.host().to_string();
    let port = i32::from(reviewed.port());
    let granted_by = guard.principal_id.to_string();
    let note = body
        .note
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let rows = match state
        .control_pg
        .query(
            "INSERT INTO zeroship.app_net_grants \
                (app_id, host, port, granted_by, note) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (app_id, host, port) DO UPDATE SET \
                granted_by = EXCLUDED.granted_by, \
                granted_at = NOW(), \
                note = EXCLUDED.note \
             RETURNING app_id, host, port, granted_by, granted_at, note",
            &[&app_id, &host, &port, &granted_by, &note],
        )
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(error = %err, app_id = %app_id, host = %host, port, "control: app net grant upsert failed");
            return db_error();
        }
    };
    let Some(row) = rows.first() else {
        tracing::error!(app_id = %app_id, host = %host, port, "control: app net grant upsert returned no row");
        return db_error();
    };

    if let Err(resp) = audit_event(
        &req,
        &state,
        &guard,
        "app_net_grant_upserted",
        json!({
            "actor": guard.principal_id,
            "app_id": app_id,
            "host": host,
            "port": port,
        }),
    )
    .await
    {
        return resp;
    }

    web::HttpResponse::Ok().json(&row_to_net_grant(row))
}

pub async fn revoke_app_net(
    req: web::HttpRequest,
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
    app_id: Path<String>,
    body: Json<NetGrantBody>,
) -> web::HttpResponse {
    if let Err(resp) = require_platform_admin(&guard, &state).await {
        return resp;
    }

    let app_id = match parse_app_uuid(&app_id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let host = normalize_host_text(&body.host);
    if host.is_empty() || body.port == 0 {
        return web::HttpResponse::BadRequest().json(&json!({"error": "invalid net grant"}));
    }
    let port = i32::from(body.port);
    let n = match state
        .control_pg
        .execute(
            "DELETE FROM zeroship.app_net_grants \
             WHERE app_id = $1 AND host = $2 AND port = $3",
            &[&app_id, &host, &port],
        )
        .await
    {
        Ok(n) => n,
        Err(err) => {
            tracing::error!(error = %err, app_id = %app_id, host = %host, port, "control: app net grant revoke failed");
            return db_error();
        }
    };

    if let Err(resp) = audit_event(
        &req,
        &state,
        &guard,
        "app_net_grant_revoked",
        json!({
            "actor": guard.principal_id,
            "app_id": app_id,
            "host": host,
            "port": port,
            "deleted": n,
        }),
    )
    .await
    {
        return resp;
    }

    if n == 0 {
        web::HttpResponse::NotFound().json(&json!({"error": "net grant not found"}))
    } else {
        web::HttpResponse::NoContent().finish()
    }
}

pub async fn get_frontable_suffixes(
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Err(resp) = require_admin_or_support(&guard, &state).await {
        return resp;
    }
    match load_frontable_suffix_catalog(&state).await {
        Ok(catalog) => web::HttpResponse::Ok().json(&FrontableSuffixesResponse {
            suffixes: catalog.suffixes,
            catalog_available: catalog.available,
            backstop_suffixes: FRONTABLE_WILDCARD_SUFFIXES.to_vec(),
        }),
        Err(resp) => resp,
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
        .control_pg
        .execute(
            "INSERT INTO zeroship.platform_policies (id, cedar_source, enabled, updated_by) \
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
        .control_pg
        .execute("DELETE FROM zeroship.platform_policies WHERE id = $1", &[&policy_id])
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
        .control_pg
        .query(
            "SELECT id, cedar_source, enabled, updated_at, updated_by \
             FROM zeroship.platform_policies \
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

struct FrontableSuffixCatalog {
    suffixes: Vec<String>,
    available: bool,
}

async fn app_exists(state: &AppState, app_id: Uuid) -> Result<bool, web::HttpResponse> {
    state
        .control_pg
        .query("SELECT 1 FROM zeroship.apps WHERE id = $1", &[&app_id])
        .await
        .map(|rows| !rows.is_empty())
        .map_err(|err| {
            tracing::error!(error = %err, app_id = %app_id, "control: app lookup failed");
            db_error()
        })
}

async fn load_app_manifest_json(
    state: &AppState,
    app_id: Uuid,
) -> Result<Option<Option<String>>, web::HttpResponse> {
    let rows = state
        .control_pg
        .query("SELECT manifest_json FROM zeroship.apps WHERE id = $1", &[&app_id])
        .await
        .map_err(|err| {
            tracing::error!(error = %err, app_id = %app_id, "control: app manifest lookup failed");
            db_error()
        })?;
    Ok(rows.first().map(|row| row.get::<_, Option<String>>("manifest_json")))
}

fn row_to_net_grant(row: &compio_postgres::Row) -> AppNetGrantResponse {
    let port_i32: i32 = row.get("port");
    AppNetGrantResponse {
        app_id: row.get("app_id"),
        host: row.get("host"),
        port: u16::try_from(port_i32).unwrap_or(0),
        granted_by: row.get("granted_by"),
        granted_at: row.get("granted_at"),
        note: row.get("note"),
    }
}

fn manifest_net_requests(
    manifest_json: Option<&str>,
    app_id: Uuid,
) -> Vec<AppNetRequestResponse> {
    let Some(raw) = manifest_json else {
        return Vec::new();
    };
    let manifest = match serde_json::from_str::<zeroship_bundle::Manifest>(raw) {
        Ok(manifest) => manifest,
        Err(err) => {
            tracing::warn!(
                app_id = %app_id,
                error = %err,
                "control: app net grant list could not parse manifest requests"
            );
            return Vec::new();
        }
    };
    manifest
        .net
        .requests
        .into_iter()
        .map(|r| AppNetRequestResponse {
            host: normalize_host_text(&r.host),
            port: r.port,
            reason: r.reason,
        })
        .collect()
}

async fn load_frontable_suffix_catalog(
    state: &AppState,
) -> Result<FrontableSuffixCatalog, web::HttpResponse> {
    let rows = state
        .control_pg
        .query(
            "SELECT value_json FROM zeroship.net_policy_catalog \
             WHERE key = 'frontable_wildcard_suffixes'",
            &[],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "control: frontable suffix catalog lookup failed");
            db_error()
        })?;
    let Some(row) = rows.first() else {
        tracing::error!("control: frontable suffix catalog row missing; wildcard grants fail closed");
        return Ok(FrontableSuffixCatalog {
            suffixes: Vec::new(),
            available: false,
        });
    };
    let value: serde_json::Value = row.get("value_json");
    let raw = match serde_json::from_value::<Vec<String>>(value) {
        Ok(raw) => raw,
        Err(err) => {
            tracing::error!(
                error = %err,
                "control: frontable suffix catalog row invalid; wildcard grants fail closed"
            );
            return Ok(FrontableSuffixCatalog {
                suffixes: Vec::new(),
                available: false,
            });
        }
    };
    match normalize_frontable_suffixes(&raw) {
        Ok(suffixes) => Ok(FrontableSuffixCatalog {
            suffixes,
            available: true,
        }),
        Err(err) => {
            tracing::error!(
                error = %err,
                "control: frontable suffix catalog contains invalid suffix; wildcard grants fail closed"
            );
            Ok(FrontableSuffixCatalog {
                suffixes: Vec::new(),
                available: false,
            })
        }
    }
}

fn normalize_host_text(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
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
        web::resource("/admin/apps/{app_id}/net-grants")
            .route(web::get().to(list_app_net_grants))
            .route(web::post().to(grant_app_net)),
    )
    .service(
        web::resource("/admin/apps/{app_id}/net-grants/revoke")
            .route(web::post().to(revoke_app_net)),
    )
    .service(
        web::resource("/admin/net-policy/frontable-wildcard-suffixes")
            .route(web::get().to(get_frontable_suffixes))
            .route(web::put().to(put_frontable_suffixes)),
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

struct ExistingPlatformPolicy {
    cedar_source: String,
    enabled: bool,
}

async fn load_platform_policy(
    state: &AppState,
    id: &str,
) -> Result<Option<ExistingPlatformPolicy>, web::HttpResponse> {
    let rows = state
        .control_pg
        .query(
            "SELECT cedar_source, enabled FROM zeroship.platform_policies WHERE id = $1",
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

    if let Err(err) = auth_audit::emit_strict(state.control_pg.as_ref(), &ev)
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
