//! `zeroship.app_user_identities` — the per-app pairwise + relay identity
//! mapping.
//!
//! The auth issuer records this row before signing an app access token. Gateway
//! session minters reassert the same deterministic pairwise subject before
//! signing a cookie. The mapping supports relay handling and revocation, which
//! need to reverse `pws_` to `(app, global_user)`.
//!
//! The row is keyed on `(app_client_id, global_user_id)` where
//! `app_client_id` is the per-app OAuth client_id (`oac_<base62>`). The
//! `pairwise_sub` is the deterministic `pws_…` projection (a column, not
//! the PK: re-login / re-grant re-derives the SAME value and UPSERTS the
//! one row). `relay_email` stays `NULL` until the consent flow populates it
//! on first email-scope consent.
//!
//! This module performs only the idempotent mapping write. The pairwise
//! derivation itself ([`zeroship_core::auth::derive_pairwise`]) is pure. Cookie
//! minters require this write before signing because durable account teardown
//! enumerates these rows to revoke access-only and cookie token families.

use compio_postgres::Client;
use uuid::Uuid;

use crate::error::{GatewayError, Result};
use crate::rls;

const fn identity_upsert_sql() -> &'static str {
    "INSERT INTO zeroship.app_user_identities \
        (app_client_id, global_user_id, pairwise_sub) \
     VALUES ($1, $2, $3) \
     ON CONFLICT (app_client_id, global_user_id) DO UPDATE SET \
        pairwise_sub = EXCLUDED.pairwise_sub, \
        revoked_at = NULL \
     WHERE zeroship.app_user_identities.pairwise_sub = EXCLUDED.pairwise_sub"
}

/// Idempotently record the pairwise mapping for `(app_client_id,
/// global_user_id)`.
///
/// `INSERT … ON CONFLICT (app_client_id, global_user_id) DO UPDATE`:
///   - `pairwise_sub` must match its immutable stored value. Configuration
///     drift fails closed instead of replacing the subject used for recall.
///   - `revoked_at` is cleared, so a revoke→re-grant reuses the SAME row
///     (the deterministic `pws_` row is never duplicated).
///   - `relay_email` is LEFT UNTOUCHED, so the gateway projection never
///     clobbers an alias minted at consent time.
///
/// `app_client_id` is the per-app OAuth client_id (`oac_<base62>`,
/// `route.oauth_client_id`); `pairwise_sub` is the derived `pws_…`.
///
/// # Errors
/// [`GatewayError::Db`] on PG failure. Cookie minters fail closed because a
/// cookie without this row could not be recalled durably.
pub async fn upsert(
    conn: &mut Client,
    app_client_id: &str,
    global_user_id: Uuid,
    pairwise_sub: &str,
) -> Result<()> {
    let tx = conn
        .transaction()
        .await
        .map_err(|e| GatewayError::Db(format!("app_user_identities upsert begin: {e}")))?;
    rls::set_tenant_client(&tx, app_client_id).await?;
    let mapped = tx
        .execute(
        identity_upsert_sql(),
        &[&app_client_id, &global_user_id, &pairwise_sub],
    )
    .await
    .map_err(|e| GatewayError::Db(format!("app_user_identities upsert: {e}")))?;
    if mapped != 1 {
        let stored_pairwise_sub: Option<String> = tx
            .query(
                "SELECT pairwise_sub FROM zeroship.app_user_identities \
                 WHERE app_client_id = $1 AND global_user_id = $2",
                &[&app_client_id, &global_user_id],
            )
            .await
            .ok()
            .and_then(|rows| rows.first().map(|row| row.get("pairwise_sub")));
        tracing::error!(
            app_client_id = %app_client_id,
            global_user_id = %global_user_id,
            stored_pairwise_sub = ?stored_pairwise_sub,
            recomputed_pairwise_sub = %pairwise_sub,
            recomputed_origin = "gateway caller supplied pairwise projection",
            "app_user_identities immutable pairwise binding mismatch"
        );
        return Err(GatewayError::Db(
            "app_user_identities pairwise binding changed".to_string(),
        ));
    }
    tx.commit()
        .await
        .map_err(|e| GatewayError::Db(format!("app_user_identities upsert commit: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::identity_upsert_sql;

    #[test]
    fn mapping_upsert_refuses_pairwise_subject_rebinding() {
        assert!(identity_upsert_sql().contains(
            "WHERE zeroship.app_user_identities.pairwise_sub = EXCLUDED.pairwise_sub"
        ));
    }
}

/// Read the persisted `pairwise_sub` for `(app_client_id,
/// global_user_id)`, or `None` when no (live) row exists. Used by tests
/// and support tooling to confirm the mapping the gateway wrote.
///
/// # Errors
/// [`GatewayError::Db`] on PG failure.
pub async fn lookup_pairwise_sub(
    conn: &mut Client,
    app_client_id: &str,
    global_user_id: Uuid,
) -> Result<Option<String>> {
    let tx = conn
        .transaction()
        .await
        .map_err(|e| GatewayError::Db(format!("app_user_identities lookup begin: {e}")))?;
    rls::set_tenant_client(&tx, app_client_id).await?;
    let rows = tx
        .query(
            "SELECT pairwise_sub FROM zeroship.app_user_identities \
             WHERE app_client_id = $1 AND global_user_id = $2",
            &[&app_client_id, &global_user_id],
        )
        .await
        .map_err(|e| GatewayError::Db(format!("app_user_identities lookup: {e}")))?;
    let sub = rows.first().map(|row| row.get("pairwise_sub"));
    tx.commit()
        .await
        .map_err(|e| GatewayError::Db(format!("app_user_identities lookup commit: {e}")))?;
    Ok(sub)
}

/// Read the ACTIVE relay alias (`relay_email`) for `(app_client_id,
/// global_user_id)`, or `None` when no live alias exists.
///
/// This is the email-claim swap source (relay sub-spec §7): the gateway
/// projects this alias as the `email` claim on EVERY auth arm so the app
/// NEVER sees the user's real address. The `revoked_at IS NULL` gate means a
/// revoked grant's alias is treated as absent — the caller then fails closed
/// (no real-email leak) rather than emit a dead alias or the real email.
///
/// Returns `None` for: no identity row, no minted alias yet (`relay_email`
/// NULL — e.g. consent ran before the gateway's first projection), or a
/// revoked row. The caller distinguishes "no alias" from "real email" — it
/// must NEVER fall back to the real email.
///
/// # Errors
/// [`GatewayError::Db`] on PG failure.
pub async fn lookup_relay_email(
    conn: &mut Client,
    app_client_id: &str,
    global_user_id: Uuid,
) -> Result<Option<String>> {
    let tx = conn
        .transaction()
        .await
        .map_err(|e| GatewayError::Db(format!("app_user_identities relay lookup begin: {e}")))?;
    rls::set_tenant_client(&tx, app_client_id).await?;
    let rows = tx
        .query(
            "SELECT relay_email FROM zeroship.app_user_identities \
             WHERE app_client_id = $1 AND global_user_id = $2 \
               AND relay_email IS NOT NULL \
               AND revoked_at IS NULL",
            &[&app_client_id, &global_user_id],
        )
        .await
        .map_err(|e| GatewayError::Db(format!("app_user_identities relay lookup: {e}")))?;
    let email = rows.first().and_then(|row| row.get("relay_email"));
    tx.commit()
        .await
        .map_err(|e| GatewayError::Db(format!("app_user_identities relay lookup commit: {e}")))?;
    Ok(email)
}
