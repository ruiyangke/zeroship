//! User-facing connected-app OAuth grant handlers.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use ntex::web;
use ntex::web::types::{Path, State};
use serde::Serialize;
use serde_json::json;
use zeroship_authz::{Action, EntityCache, Resource};

use crate::auth_audit;
use crate::authz_guard::AuthzGuard;
use crate::AppState;

const MAX_CLIENT_ID_PATH_BYTES: usize = 128;

#[derive(Debug, Serialize)]
pub struct OauthGrantSummary {
    pub client_id: String,
    pub client_name: String,
    pub client_uri: Option<String>,
    pub logo_uri: Option<String>,
    pub granted_scopes: Vec<String>,
    pub granted_at: String,
    pub last_used_at: Option<String>,
}

/// GET /me/oauth-grants - list active grants for the authenticated user.
pub async fn list_grants(
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Err(resp) = authz
        .require(Action::AccountRead, Resource::Any, &state)
        .await
    {
        return resp;
    }

    let rows = match state
        .control_pg
        .query(
            "SELECT g.client_id, c.client_name, c.client_uri, c.logo_uri, \
                    g.granted_scopes, g.granted_at, g.last_used_at \
             FROM zeroship.oauth_grants g \
             JOIN zeroship.oauth_clients c ON c.client_id = g.client_id \
             WHERE g.user_id = $1 \
             ORDER BY g.granted_at DESC",
            &[&authz.principal_id],
        )
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(error = %err, "control: oauth grant list failed");
            return web::HttpResponse::InternalServerError()
                .json(&json!({"error": "oauth_grant_list_failed"}));
        }
    };

    let grants = rows
        .into_iter()
        .map(|row| {
            let granted_at: DateTime<Utc> = row.get("granted_at");
            let last_used_at: Option<DateTime<Utc>> = row.get("last_used_at");
            OauthGrantSummary {
                client_id: row.get("client_id"),
                client_name: row.get("client_name"),
                client_uri: row.get("client_uri"),
                logo_uri: row.get("logo_uri"),
                granted_scopes: row.get("granted_scopes"),
                granted_at: granted_at.to_rfc3339(),
                last_used_at: last_used_at.map(|value| value.to_rfc3339()),
            }
        })
        .collect::<Vec<_>>();

    web::HttpResponse::Ok().json(&grants)
}

/// DELETE /me/oauth-grants/{client_id} - revoke grant and active tokens.
#[allow(clippy::future_not_send)]
pub async fn revoke_grant(
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
    client_id: Path<String>,
) -> web::HttpResponse {
    if let Err(resp) = authz
        .require(Action::AccountWrite, Resource::Any, &state)
        .await
    {
        return resp;
    }

    let client_id = client_id.into_inner();
    if !valid_client_id_path_segment(&client_id) {
        return web::HttpResponse::BadRequest().json(&json!({"error": "invalid_client_id"}));
    }

    // Atomic revocation cascade (relay sub-spec §6, B4): DELETE the grant AND
    // revoke the relay alias for THIS (app, user) in ONE transaction. This is
    // the EXPLICIT single-grant revoke path — NOT app delete — so the FK
    // cascades do NOT cover it: deleting a child `oauth_grants` row cascades to
    // nothing, and the user/app both still exist. We therefore still flip the
    // alias's `revoked_at` and write the per-app token-family marker by hand,
    // all in one transaction.
    //
    // It runs on a DEDICATED owned connection (`registry.conn()` → a fresh
    // mutable `Client` on the single `zeroship` DB) — NOT the shared
    // `control_pg`. Driving a multi-statement `BEGIN…COMMIT` on the shared
    // pipelined handle would let a bystander handler's statement interleave
    // inside this window. The owned client gives us the `&mut self` the RAII
    // `transaction()` needs and isolates snapshot/locks/abort-state. After
    // commit, inbound to that alias bounces (5b `revoked_at IS NULL` gate +
    // `resolve_active_alias`'s structural `EXISTS(oauth_grants)` read gate).
    let deleted = match revoke_grant_cascade(
        &state,
        &authz.principal_id,
        &client_id,
    )
    .await
    {
        Ok(deleted) => deleted,
        Err(err) => {
            tracing::error!(
                error = %err,
                client_id = %client_id,
                "control: oauth grant revoke cascade failed"
            );
            return web::HttpResponse::InternalServerError()
                .json(&json!({"error": "oauth_grant_delete_failed"}));
        }
    };

    if deleted == 0 {
        return web::HttpResponse::NotFound().json(&json!({"error": "oauth_grant_not_found"}));
    }
    EntityCache::invalidate(authz.principal_id);

    if let Err(resp) = auth_audit::emit_guard_event(
        &state,
        &authz,
        "oauth_grant_revoke",
        Some(&client_id),
        json!({
            "client_id": &client_id,
        }),
    )
    .await
    {
        return resp;
    }

    web::HttpResponse::NoContent().finish()
}

/// Atomically DELETE the `zeroship.oauth_grants` row for `(user, client_id)`
/// AND revoke the matching relay alias + write the per-app token-family marker,
/// in ONE transaction on a DEDICATED owned connection (never the shared
/// `control_pg`). Returns the number of grant rows deleted (0 ⇒ the caller
/// answers 404; the alias UPDATE is a no-op inside the same txn, nothing is
/// half-applied).
///
/// This is the explicit single-grant revoke. The new app-delete FK cascades do
/// NOT subsume it (those fire on app/oauth_clients deletion; here both still
/// exist), so the alias revoke + family marker are written by hand. (This logic
/// was previously its own module; it is now inlined here onto `registry.conn()`
/// — same single `zeroship` DB.)
async fn revoke_grant_cascade(
    state: &AppState,
    user_id: &uuid::Uuid,
    client_id: &str,
) -> Result<u64, crate::registry::RegistryError> {
    let mut conn = state.registry.conn().await?;
    let tx = conn.transaction().await?;
    // DELETE the grant FIRST, in the SAME transaction as the alias UPDATE, so we
    // can never commit one without the other. This does NOT serialize against
    // auth's re-consent un-revoke (no shared advisory lock); the cross-service
    // race is closed STRUCTURALLY on the read side — `resolve_active_alias`
    // (auth `store/relay.rs`) forwards only when a live `oauth_grants` row still
    // EXISTS, so DELETEing the grant here silences the alias regardless of which
    // writer last touched `revoked_at`.
    let deleted = tx
        .execute(
            "DELETE FROM zeroship.oauth_grants WHERE user_id = $1 AND client_id = $2",
            &[user_id, &client_id],
        )
        .await?;
    // SAME txn — revoke the relay alias for THIS (app, user). Keyed DIRECTLY on
    // app_client_id = client_id (oac_…, §6.2): no join, exact match. Only an
    // ACTIVE alias is touched so a re-revoke is idempotent.
    tx.execute(
        "UPDATE zeroship.app_user_identities \
            SET revoked_at = now() \
          WHERE app_client_id = $2 \
            AND global_user_id = $1 \
            AND revoked_at IS NULL",
        &[user_id, &client_id],
    )
    .await?;
    // SAME txn — write the per-app token-family marker so the live ACCESS token
    // dies too (Batch A fix 4), not just the relay alias. We derive the SAME
    // `pws_` the gateway projects: `derive_pairwise(salt, global_user_id,
    // sector)` under the route's apex `sector_identifier` (read from
    // `zeroship.app_oauth_clients` by client_id). When the sector row is missing
    // (client never provisioned) there is no live wrapper to revoke, so the
    // marker write is skipped — the alias revoke above still applies.
    let sector: Option<String> = tx
        .query_opt(
            "SELECT sector_identifier FROM zeroship.app_oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await?
        .map(|row| row.get("sector_identifier"));
    if let Some(sector) = sector {
        let pws = zeroship_core::auth::derive_pairwise(
            &state.pairwise_salt,
            &user_id.to_string(),
            &sector,
        );
        tx.execute(
            "INSERT INTO zeroship.token_revocations (client_id, sub, revoked_after) \
             VALUES ($1, $2, NOW()) \
             ON CONFLICT (client_id, sub) DO UPDATE SET revoked_after = EXCLUDED.revoked_after",
            &[&client_id, &pws],
        )
        .await?;
    }
    tx.commit().await?;
    Ok(deleted)
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/me/oauth-grants").route(web::get().to(list_grants)))
        .service(
            web::resource("/me/oauth-grants/{client_id}").route(web::delete().to(revoke_grant)),
        );
}

fn valid_client_id_path_segment(client_id: &str) -> bool {
    !client_id.is_empty() && client_id.len() <= MAX_CLIENT_ID_PATH_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_path_segment_is_bounded() {
        assert!(valid_client_id_path_segment("client_123"));
        assert!(valid_client_id_path_segment(&"a".repeat(128)));
        assert!(!valid_client_id_path_segment(""));
        assert!(!valid_client_id_path_segment(&"a".repeat(129)));
    }
}
