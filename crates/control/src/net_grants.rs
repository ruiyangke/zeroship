//! Creator self-service for an app's raw-TCP egress hosts.
//!
//! `zeroship.app_net_grants` is the authoritative allowlist the registry
//! projects into every runtime (`Registry::get_versions`). This module is the
//! only writer of that table, and the creator who owns the app is the author:
//! `GET`/`POST`/`DELETE /api/apps/{app_id}/net-grants`, authorized as
//! `env:read`/`env:write` on `Resource::App` through the same `app_members`
//! path as vars and secrets.
//!
//! Three things bound what a creator can write, and none of them is a human
//! reviewer:
//!
//! 1. **Deny by default.** An app with no rows gets `NetPolicy::Denied` and
//!    cannot resolve `node:net` at all. Nothing here changes that.
//! 2. **Shape.** Every host goes through
//!    [`HostPort::try_new_with_frontable_suffixes`] with the operator's
//!    frontable-suffix catalog, which rejects bare `*`, malformed wildcards,
//!    registry-level suffixes, and wildcards fronting shared infrastructure.
//!    A missing or invalid catalog row fails CLOSED: wildcards are refused,
//!    never permitted.
//! 3. **Plan caps.** `max_grants` bounds the row count; `max_sockets` and
//!    `egress_ceiling_bytes` bound concurrency and volume. All three come from
//!    the plan catalog and a creator can never raise them.
//!
//! What this deliberately does NOT claim: it is not a boundary against a
//! malicious creator. `fetch` reaches any public host with no allowlist at
//! all, so raw-TCP narrowness is a compromised-dependency blast-radius
//! control (`zeroship_core::net_policy`), and the malicious-creator controls
//! are attribution, spend enforcement, and egress ceilings.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use ntex::web::{
    self,
    types::{Json, Path, State},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use compio_postgres::Client;
use uuid::Uuid;
use zeroship_authz::{Action as AuthzAction, Resource};
use zeroship_core::net_policy::{normalize_frontable_suffixes, HostPort};
use zeroship_core::types::{AppNetPolicyLimits, FREE_TIER_NET_POLICY_LIMITS};

use crate::audit::{self, Action as AuditAction, AuditEntry};
use crate::authz_guard::AuthzGuard;
use crate::env_handlers::admin_rate_limit;
use crate::http_util;
use crate::AppState;

/// A net-grant body is a host, a port and a short note. 8 KiB is far more than
/// that and far less than anything worth streaming.
pub const NET_GRANT_PAYLOAD_BYTES: usize = 8 * 1024;

const MAX_NOTE_CHARS: usize = 200;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct NetGrantBody {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct NetGrant {
    pub app_id: Uuid,
    pub host: String,
    pub port: u16,
    pub granted_by: String,
    pub granted_at: DateTime<Utc>,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct NetRequest {
    pub host: String,
    pub port: u16,
    pub reason: String,
}

/// The plan ceiling, echoed on every list so a creator can see what they are
/// working against without reading the plan catalog.
#[derive(Debug, Serialize)]
pub struct NetGrantLimits {
    pub max_grants: u32,
    pub used_grants: u32,
    pub max_sockets: u32,
    pub egress_ceiling_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct NetGrantList {
    pub app_id: Uuid,
    pub grants: Vec<NetGrant>,
    /// The manifest's `net.requests` hints, inert until a grant exists.
    pub requests: Vec<NetRequest>,
    /// Hints with no matching grant row — what the creator has yet to allow.
    pub pending_requests: Vec<NetRequest>,
    pub limits: NetGrantLimits,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum NetGrantError {
    AppNotFound,
    /// The host/port failed shape validation; carries the validator's message.
    Invalid(String),
    /// The app already holds `max_grants` rows and this call would add another.
    CapExceeded { max: u32, current: u32 },
    /// Revoke targeted a row that does not exist.
    GrantNotFound,
    Db,
}

impl NetGrantError {
    pub fn into_response(self) -> web::HttpResponse {
        match self {
            Self::AppNotFound => {
                web::HttpResponse::NotFound().json(&json!({"error": "app not found"}))
            }
            Self::Invalid(detail) => web::HttpResponse::BadRequest()
                .json(&json!({"error": "invalid net grant", "detail": detail})),
            Self::CapExceeded { max, current } => web::HttpResponse::Conflict().json(&json!({
                "error": "net grant limit reached",
                "detail": format!(
                    "plan allows {max} egress host grants; this app already holds {current}"
                ),
                "max_grants": max,
                "used_grants": current,
            })),
            Self::GrantNotFound => {
                web::HttpResponse::NotFound().json(&json!({"error": "net grant not found"}))
            }
            Self::Db => web::HttpResponse::InternalServerError()
                .json(&json!({"error": "database error"})),
        }
    }
}

// ---------------------------------------------------------------------------
// Frontable-suffix catalog
// ---------------------------------------------------------------------------

/// The operator's wildcard-suffix catalog, plus whether it could be read.
///
/// `available: false` is the fail-closed state: every wildcard grant is
/// refused while the catalog is missing or unparsable. Exact hosts do not
/// consult it and are unaffected.
#[derive(Debug, Clone)]
pub struct FrontableSuffixCatalog {
    pub suffixes: Vec<String>,
    pub available: bool,
}

impl FrontableSuffixCatalog {
    const fn unavailable() -> Self {
        Self {
            suffixes: Vec::new(),
            available: false,
        }
    }
}

/// Load the operator-editable frontable-suffix catalog.
///
/// Every failure mode - the row missing, the JSON not being a string array, a
/// suffix failing normalization - resolves to `unavailable()`, which refuses
/// wildcards. A `Db` error is only returned when the QUERY itself fails, so a
/// transport fault surfaces as a 500 rather than as a silent narrowing.
pub async fn load_frontable_suffix_catalog(
    pg: &Client,
) -> Result<FrontableSuffixCatalog, NetGrantError> {
    let rows = pg
        .query(
            "SELECT value_json FROM zeroship.net_policy_catalog \
             WHERE key = 'frontable_wildcard_suffixes'",
            &[],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "control: frontable suffix catalog lookup failed");
            NetGrantError::Db
        })?;
    let Some(row) = rows.first() else {
        tracing::error!("control: frontable suffix catalog row missing; wildcard grants fail closed");
        return Ok(FrontableSuffixCatalog::unavailable());
    };
    let value: serde_json::Value = row.get("value_json");
    let raw = match serde_json::from_value::<Vec<String>>(value) {
        Ok(raw) => raw,
        Err(err) => {
            tracing::error!(
                error = %err,
                "control: frontable suffix catalog row invalid; wildcard grants fail closed"
            );
            return Ok(FrontableSuffixCatalog::unavailable());
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
            Ok(FrontableSuffixCatalog::unavailable())
        }
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// The app's plan-derived net caps. A missing app is `AppNotFound`; a missing
/// or corrupt plan row falls back to the free tier, never to "unbounded".
async fn plan_net_limits(
    pg: &Client,
    app_id: Uuid,
) -> Result<AppNetPolicyLimits, NetGrantError> {
    let rows = pg
        .query(
            "SELECT p.net_policy_limits_json \
             FROM zeroship.apps a \
             LEFT JOIN zeroship.plans p ON p.id = a.plan_id \
             WHERE a.id = $1",
            &[&app_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, app_id = %app_id, "control: app plan lookup failed");
            NetGrantError::Db
        })?;
    let Some(row) = rows.first() else {
        return Err(NetGrantError::AppNotFound);
    };
    let json: Option<serde_json::Value> = row.get("net_policy_limits_json");
    Ok(json
        .and_then(|j| {
            serde_json::from_value::<AppNetPolicyLimits>(j)
                .map_err(|err| {
                    tracing::warn!(
                        app_id = %app_id,
                        error = %err,
                        "control: plan net_policy_limits_json parse failure - using free-tier caps"
                    );
                })
                .ok()
        })
        .unwrap_or(FREE_TIER_NET_POLICY_LIMITS))
}

async fn count_grants(pg: &Client, app_id: Uuid) -> Result<u32, NetGrantError> {
    let rows = pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.app_net_grants WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, app_id = %app_id, "control: net grant count failed");
            NetGrantError::Db
        })?;
    let n: i64 = rows.first().map_or(0, |row| row.get("n"));
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

/// List an app's grants, its manifest hints, and the plan ceiling.
pub async fn list_grants(pg: &Client, app_id: Uuid) -> Result<NetGrantList, NetGrantError> {
    let manifest_rows = pg
        .query(
            "SELECT manifest_json FROM zeroship.apps WHERE id = $1",
            &[&app_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, app_id = %app_id, "control: app manifest lookup failed");
            NetGrantError::Db
        })?;
    let Some(manifest_row) = manifest_rows.first() else {
        return Err(NetGrantError::AppNotFound);
    };
    let manifest_json: Option<String> = manifest_row.get("manifest_json");

    let rows = pg
        .query(
            "SELECT app_id, host, port, granted_by, granted_at, note \
             FROM zeroship.app_net_grants \
             WHERE app_id = $1 \
             ORDER BY host ASC, port ASC",
            &[&app_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, app_id = %app_id, "control: app net grants list failed");
            NetGrantError::Db
        })?;
    let grants: Vec<NetGrant> = rows.iter().map(row_to_grant).collect();
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

    let caps = plan_net_limits(pg, app_id).await?;
    let used_grants = u32::try_from(grants.len()).unwrap_or(u32::MAX);
    Ok(NetGrantList {
        app_id,
        grants,
        requests,
        pending_requests,
        limits: NetGrantLimits {
            max_grants: caps.max_grants,
            used_grants,
            max_sockets: caps.max_sockets,
            egress_ceiling_bytes: caps.egress_ceiling_bytes,
        },
    })
}

/// Validate and write one grant, re-noting an existing `(host, port)` in
/// place. The single writer of `zeroship.app_net_grants`.
pub async fn upsert_grant(
    pg: &Client,
    app_id: Uuid,
    body: &NetGrantBody,
    granted_by: &str,
) -> Result<NetGrant, NetGrantError> {
    let caps = plan_net_limits(pg, app_id).await?;
    let catalog = load_frontable_suffix_catalog(pg).await?;
    let reviewed = HostPort::try_new_with_frontable_suffixes(
        body.host.clone(),
        body.port,
        &catalog.suffixes,
        catalog.available,
    )
    .map_err(NetGrantError::Invalid)?;
    let host = reviewed.host().to_string();
    let port = i32::from(reviewed.port());
    let note = match body.note.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(note) if note.chars().count() > MAX_NOTE_CHARS => {
            return Err(NetGrantError::Invalid(format!(
                "note must be at most {MAX_NOTE_CHARS} characters"
            )));
        }
        Some(note) => Some(note.to_string()),
        None => None,
    };
    let cap = i64::from(caps.max_grants);

    // The cap is a WHERE on the insert rather than a read-then-write, so a
    // second call cannot slip between the check and the row. It is not a hard
    // serialization barrier: two concurrent inserts under READ COMMITTED can
    // each see a pre-insert count, so the ceiling can overshoot by at most the
    // number of in-flight calls. Bounding a creator's own host list does not
    // warrant a table lock on the registry's read path.
    let rows = pg
        .query(
            "INSERT INTO zeroship.app_net_grants \
                (app_id, host, port, granted_by, note) \
             SELECT $1, $2, $3, $4, $5 \
             WHERE (SELECT COUNT(*) FROM zeroship.app_net_grants \
                    WHERE app_id = $1 AND NOT (host = $2 AND port = $3)) < $6 \
             ON CONFLICT (app_id, host, port) DO UPDATE SET \
                granted_by = EXCLUDED.granted_by, \
                granted_at = NOW(), \
                note = EXCLUDED.note \
             RETURNING app_id, host, port, granted_by, granted_at, note",
            &[&app_id, &host, &port, &granted_by, &note, &cap],
        )
        .await
        .map_err(|err| {
            tracing::error!(
                error = %err, app_id = %app_id, host = %host, port,
                "control: app net grant upsert failed"
            );
            NetGrantError::Db
        })?;

    let Some(row) = rows.first() else {
        // No row means the WHERE refused it: the app is already at its cap.
        return Err(NetGrantError::CapExceeded {
            max: caps.max_grants,
            current: count_grants(pg, app_id).await?,
        });
    };
    Ok(row_to_grant(row))
}

/// Delete one grant. Revoking is not shape-validated - the host is only a key
/// into rows this app already holds - but it IS normalized the same way, so
/// `SMTP.Example.COM.` revokes the row `smtp.example.com`.
pub async fn revoke_grant(
    pg: &Client,
    app_id: Uuid,
    host: &str,
    port: u16,
) -> Result<(), NetGrantError> {
    let host = normalize_host_text(host);
    if host.is_empty() || port == 0 {
        return Err(NetGrantError::Invalid(
            "host must not be empty and port must be between 1 and 65535".to_string(),
        ));
    }
    let port = i32::from(port);
    let n = pg
        .execute(
            "DELETE FROM zeroship.app_net_grants \
             WHERE app_id = $1 AND host = $2 AND port = $3",
            &[&app_id, &host, &port],
        )
        .await
        .map_err(|err| {
            tracing::error!(
                error = %err, app_id = %app_id, host = %host, port,
                "control: app net grant revoke failed"
            );
            NetGrantError::Db
        })?;
    if n == 0 {
        return Err(NetGrantError::GrantNotFound);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn row_to_grant(row: &compio_postgres::Row) -> NetGrant {
    let port_i32: i32 = row.get("port");
    NetGrant {
        app_id: row.get("app_id"),
        host: row.get("host"),
        port: u16::try_from(port_i32).unwrap_or(0),
        granted_by: row.get("granted_by"),
        granted_at: row.get("granted_at"),
        note: row.get("note"),
    }
}

fn manifest_net_requests(manifest_json: Option<&str>, app_id: Uuid) -> Vec<NetRequest> {
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
        .map(|r| NetRequest {
            host: normalize_host_text(&r.host),
            port: r.port,
            reason: r.reason,
        })
        .collect()
}

/// DNS-name normalization matching `zeroship_core::net_policy`'s, so a lookup
/// key built here matches a host that module produced.
pub(crate) fn normalize_host_text(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn bad_uuid() -> web::HttpResponse {
    web::HttpResponse::BadRequest().json(&json!({"error": "bad app_id"}))
}

// ---------------------------------------------------------------------------
// Creator HTTP surface
// ---------------------------------------------------------------------------

pub async fn list(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let Ok(app_id) = Uuid::parse_str(&path) else {
        return bad_uuid();
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::EnvRead,
            Resource::App {
                id: app_id.to_string(),
            },
            &state,
        )
        .await
    {
        return resp;
    }
    match list_grants(state.control_pg.as_ref(), app_id).await {
        Ok(list) => web::HttpResponse::Ok().json(&list),
        Err(e) => e.into_response(),
    }
}

pub async fn create(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: Json<NetGrantBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let Ok(app_id) = Uuid::parse_str(&path) else {
        return bad_uuid();
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::EnvWrite,
            Resource::App {
                id: app_id.to_string(),
            },
            &state,
        )
        .await
    {
        return resp;
    }
    match upsert_grant(
        state.control_pg.as_ref(),
        app_id,
        &body,
        &authz.principal_id.to_string(),
    )
    .await
    {
        Ok(grant) => {
            let resource = format!("{}:{}", grant.host, grant.port);
            log_grant_audit(&req, &state, &authz, app_id, AuditAction::GrantAppNet, &resource).await;
            web::HttpResponse::Ok().json(&grant)
        }
        Err(e) => e.into_response(),
    }
}

pub async fn delete(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: Json<NetGrantBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let Ok(app_id) = Uuid::parse_str(&path) else {
        return bad_uuid();
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::EnvWrite,
            Resource::App {
                id: app_id.to_string(),
            },
            &state,
        )
        .await
    {
        return resp;
    }
    match revoke_grant(state.control_pg.as_ref(), app_id, &body.host, body.port).await {
        Ok(()) => {
            let resource = format!("{}:{}", normalize_host_text(&body.host), body.port);
            log_grant_audit(&req, &state, &authz, app_id, AuditAction::RevokeAppNet, &resource)
                .await;
            web::HttpResponse::NoContent().finish()
        }
        Err(e) => e.into_response(),
    }
}

async fn log_grant_audit(
    req: &web::HttpRequest,
    state: &AppState,
    authz: &AuthzGuard,
    app_id: Uuid,
    action: AuditAction,
    resource: &str,
) {
    let ip = http_util::source_ip(req, state.trust_proxy);
    audit::log(
        &state.registry,
        AuditEntry {
            app_id: Some(app_id),
            creator_id: None,
            actor_user_id: Some(authz.principal_id),
            action,
            resource: Some(resource),
            source_ip: ip.as_deref(),
        },
    )
    .await;
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/api/apps/{id}/net-grants")
            .state(web::types::PayloadConfig::new(NET_GRANT_PAYLOAD_BYTES))
            .route(web::get().to(list))
            .route(web::post().to(create))
            .route(web::delete().to(delete)),
    );
}
