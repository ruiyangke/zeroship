//! Personal Access Token handlers for the creator console.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Duration, Utc};
use ntex::web;
use ntex::web::types::{Json, Path as WebPath, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;
use zeroship_authz::{
    self as authz, AuthzContext, AuthzDecision, Condition, Effect, EntityCache, Policy, Resource,
};

use crate::auth_audit;
use crate::authz_guard::AuthzGuard;
use crate::AppState;

const MAX_EXPIRES_IN_DAYS: u16 = 365;
const DEFAULT_EXPIRES_IN_DAYS: u16 = 365;
const MAX_GRANT_PAIRS: usize = 10_000;
const MAX_PAT_NAME_CHARS: usize = 200;

#[derive(Debug, Deserialize)]
pub struct CreateTokenBody {
    name: String,
    policies: Policy,
    expires_in_days: Option<u16>,
}

#[derive(Debug, Serialize)]
struct CreateTokenResponse {
    id: String,
    token: String,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct TokenListEntry {
    id: String,
    name: String,
    created_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    last_used_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
struct DeleteTokenResponse {
    id: String,
    revoked_at: DateTime<Utc>,
}

fn valid_pat_name(name: &str) -> bool {
    !name.trim().is_empty() && name.chars().count() <= MAX_PAT_NAME_CHARS
}

pub async fn create_token(
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
    body: Json<CreateTokenBody>,
) -> web::HttpResponse {
    // A PAT may NOT mint another PAT (no token-chaining): the acting principal
    // must be an interactive OAuth/BFF session bearer (`token_id == None`,
    // `token_policy == Some`), not a PAT (`token_id == Some`). Under the R5
    // cutover the bespoke console-session principal is gone; the non-PAT
    // principal is now the OAuth access token the BFF session carries.
    if guard.token_id.is_some() {
        return web::HttpResponse::Unauthorized().json(&json!({
            "error": "interactive_session_required",
            "message": "PATs can only be minted from an interactive session bearer, not from another PAT",
        }));
    }

    let expires_in_days = body.expires_in_days.unwrap_or(DEFAULT_EXPIRES_IN_DAYS);
    if expires_in_days == 0 || expires_in_days > MAX_EXPIRES_IN_DAYS {
        return web::HttpResponse::BadRequest().json(&json!({
            "error": "invalid_expires_in_days",
            "message": "expires_in_days must be between 1 and 365",
        }));
    }

    let name = body.name.trim();
    if !valid_pat_name(name) {
        return web::HttpResponse::BadRequest().json(&json!({
            "error": "invalid_token_name",
            "message": "name must be 1-200 characters",
        }));
    }

    if let Err(resp) = validate_grant_subset(&guard, &state, &body.policies).await {
        return resp;
    }

    let token_id = Uuid::new_v4();
    let policies_json = body.policies.to_json_value();
    let policy_hash = authz::policy_hash(&policies_json);
    let expires_at = Utc::now() + Duration::days(i64::from(expires_in_days));
    let token = match state.pat_issuer.issue(
        token_id,
        guard.principal_id,
        policy_hash.clone(),
        expires_at,
    ) {
        Ok(token) => token,
        Err(err) => {
            tracing::error!(error = %err, "control: PAT issue failed");
            return web::HttpResponse::InternalServerError()
                .json(&json!({"error": "pat_issue_failed"}));
        }
    };

    match state
        .control_pg
        .execute(
            "INSERT INTO zeroship.permission_tokens \
                (id, owner_id, kind, name, policies, policy_hash, expires_at) \
             VALUES ($1, $2, 'pat', $3, $4, $5, $6)",
            &[
                &token_id,
                &guard.principal_id,
                &name,
                &policies_json,
                &policy_hash,
                &expires_at,
            ],
        )
        .await
    {
        Ok(_) => {
            if let Err(resp) = auth_audit::emit_guard_event(
                &state,
                &guard,
                "pat_mint",
                None,
                json!({
                    "token_id": token_id.to_string(),
                    "name": name,
                    "expires_at": expires_at.to_rfc3339(),
                    "policy_hash": policy_hash,
                }),
            )
            .await
            {
                return resp;
            }

            web::HttpResponse::Ok().json(&CreateTokenResponse {
                id: token_id.to_string(),
                token,
                expires_at,
            })
        }
        Err(err) => {
            tracing::error!(error = %err, "control: PAT insert failed");
            web::HttpResponse::InternalServerError().json(&json!({"error": "pat_insert_failed"}))
        }
    }
}

pub async fn list_tokens(
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let rows = match state
        .control_pg
        .query(
            "SELECT id, name, created_at, expires_at, last_used_at, revoked_at \
             FROM zeroship.permission_tokens \
             WHERE owner_id = $1 AND kind = 'pat' \
             ORDER BY created_at DESC, id DESC",
            &[&guard.principal_id],
        )
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(error = %err, "control: PAT list failed");
            return web::HttpResponse::InternalServerError()
                .json(&json!({"error": "pat_list_failed"}));
        }
    };

    let tokens = rows
        .into_iter()
        .map(|row| TokenListEntry {
            id: row.get::<_, Uuid>("id").to_string(),
            name: row.get("name"),
            created_at: row.get("created_at"),
            expires_at: row.get("expires_at"),
            last_used_at: row.get("last_used_at"),
            revoked_at: row.get("revoked_at"),
        })
        .collect::<Vec<_>>();

    web::HttpResponse::Ok().json(&tokens)
}

pub async fn delete_token(
    guard: AuthzGuard,
    state: State<Arc<AppState>>,
    id: WebPath<String>,
) -> web::HttpResponse {
    let token_id = match id.parse::<Uuid>() {
        Ok(id) => id,
        Err(_) => {
            return web::HttpResponse::BadRequest().json(&json!({"error": "invalid_token_id"}));
        }
    };

    let rows = match state
        .control_pg
        .query(
            "UPDATE zeroship.permission_tokens \
             SET revoked_at = COALESCE(revoked_at, NOW()) \
             WHERE id = $1 AND owner_id = $2 AND kind = 'pat' \
             RETURNING revoked_at",
            &[&token_id, &guard.principal_id],
        )
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(error = %err, "control: PAT revoke failed");
            return web::HttpResponse::InternalServerError()
                .json(&json!({"error": "pat_revoke_failed"}));
        }
    };

    let Some(row) = rows.first() else {
        return web::HttpResponse::NotFound().json(&json!({"error": "token_not_found"}));
    };
    EntityCache::invalidate(guard.principal_id);
    let revoked_at: DateTime<Utc> = row.get("revoked_at");
    if let Err(resp) = auth_audit::emit_guard_event(
        &state,
        &guard,
        "pat_revoke",
        None,
        json!({
            "token_id": token_id.to_string(),
            "revoked_at": revoked_at.to_rfc3339(),
        }),
    )
    .await
    {
        return resp;
    }

    web::HttpResponse::Ok().json(&DeleteTokenResponse {
        id: token_id.to_string(),
        revoked_at,
    })
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/me/tokens")
            .route(web::post().to(create_token))
            .route(web::get().to(list_tokens)),
    )
    .service(web::resource("/me/tokens/{id}").route(web::delete().to(delete_token)));
}

async fn validate_grant_subset(
    guard: &AuthzGuard,
    state: &AppState,
    policy: &Policy,
) -> Result<(), web::HttpResponse> {
    validate_policy_shape(policy)?;

    let now = now_unix().map_err(|err| {
        tracing::error!(error = %err, "control: PAT grant validation clock failed");
        web::HttpResponse::InternalServerError().json(&json!({"error": "authz_error"}))
    })?;

    let mut pairs = 0usize;
    for statement in &policy.statements {
        // Deny statements only narrow the wrapper policy. The subset check
        // validates every Allow pair because those are the only statements
        // that can grant authority beyond what the principal already has.
        if statement.effect != Effect::Allow {
            continue;
        }
        for action in &statement.actions {
            for resource in &statement.resources {
                pairs += 1;
                if pairs > MAX_GRANT_PAIRS {
                    return Err(web::HttpResponse::BadRequest().json(&json!({
                        "error": "too_many_policy_pairs",
                        "message": "policy expands beyond 10000 action/resource pairs",
                    })));
                }

                let ctx = AuthzContext {
                    principal_id: guard.principal_id,
                    token_id: None,
                    token_policy: None,
                    action: *action,
                    resource: resource.clone(),
                    now,
                    request_ip: guard.request_ip,
                    mfa_verified: guard.mfa_verified,
                    mfa_age_seconds: guard.mfa_age_seconds,
                    request_id: Some(guard.request_id.as_str()),
                };

                let authorized = if matches!(resource, Resource::Any) {
                    authz::is_authorized_anywhere(&state.control_pg, &state.static_policies, &ctx)
                        .await
                } else {
                    authz::enforce(&state.control_pg, &state.static_policies, &ctx)
                        .await
                        .map(|decision| decision == AuthzDecision::Allow)
                };

                match authorized {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(web::HttpResponse::BadRequest().json(&json!({
                            "error": "excess_permissions",
                            "message": "you can't grant a permission you don't have",
                            "action": action.cedar_id(),
                            "resource": resource,
                        })));
                    }
                    Err(err) => {
                        tracing::error!(error = %err, "control: PAT grant validation failed");
                        return Err(web::HttpResponse::InternalServerError()
                            .json(&json!({"error": "authz_error"})));
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_policy_shape(policy: &Policy) -> Result<(), web::HttpResponse> {
    for statement in &policy.statements {
        if statement.actions.is_empty() {
            return Err(web::HttpResponse::BadRequest().json(&json!({
                "error": "empty_policy_statement",
                "message": "policy statements must include at least one action",
            })));
        }
        if statement.resources.is_empty() {
            return Err(web::HttpResponse::BadRequest().json(&json!({
                "error": "empty_policy_statement",
                "message": "policy statements must include at least one resource",
            })));
        }
        for condition in &statement.conditions {
            if matches!(condition, Condition::RequireMfa | Condition::MfaWithin { .. }) {
                return Err(web::HttpResponse::BadRequest().json(&json!({
                    "error": "unsupported_policy_condition",
                    "message": "MFA policy conditions are not yet enforced by control-plane sessions",
                })));
            }
        }
        for resource in &statement.resources {
            if let Err(message) = resource.validate_ids() {
                return Err(web::HttpResponse::BadRequest().json(&json!({
                    "error": "invalid_resource_id",
                    "message": message,
                    "resource": resource,
                })));
            }
        }
    }
    Ok(())
}

fn now_unix() -> Result<i64, String> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| format!("clock: {err}"))?
            .as_secs(),
    )
    .map_err(|err| format!("clock overflow: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pat_name_is_bounded() {
        assert!(valid_pat_name("CI deploy"));
        assert!(valid_pat_name(&"A".repeat(200)));
        assert!(!valid_pat_name(""));
        assert!(!valid_pat_name("   "));
        assert!(!valid_pat_name(&"A".repeat(201)));
    }
}
