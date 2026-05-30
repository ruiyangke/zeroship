//! `auth.app_user_identities` — the per-app pairwise + relay identity
//! mapping (auth-sdk Slice 4, spec §6.2/§6.3/§8.1).
//!
//! The gateway derives the per-app pairwise subject
//! `pws_ = derive_pairwise(pairwise_salt, global_user_id,
//! route.sector_identifier)` at the `ZeroShip-User` header boundary
//! (F4-B) and [`upsert`]s this row whenever it projects a `pws_` for an
//! `(app_client_id, global_user_id)` — so the mapping exists for support
//! tooling, the relay handler (Slice 5), and revocation, which all need
//! to reverse `pws_ → (app, global_user)`.
//!
//! The row is keyed on `(app_client_id, global_user_id)` where
//! `app_client_id` is the per-app OAuth client_id (`oac_<base62>`). The
//! `pairwise_sub` is the deterministic `pws_…` projection (a column, not
//! the PK: re-login / re-grant re-derives the SAME value and UPSERTS the
//! one row). `relay_email` stays `NULL` until Slice 5 populates it on
//! first email-scope consent.
//!
//! This module performs ONLY the idempotent mapping write. The pairwise
//! derivation itself ([`zeroship_core::auth::derive_pairwise`]) is a pure
//! function — the gateway derives the `pws_` without a DB round-trip and
//! writes it here so the reverse-lookup is available. A failed mapping
//! write is therefore NON-fatal to the auth decision (the projected
//! header is already correct); callers log-and-continue.

use compio_postgres::Client;
use uuid::Uuid;

use crate::error::{GatewayError, Result};

/// Idempotently record the pairwise mapping for `(app_client_id,
/// global_user_id)`.
///
/// `INSERT … ON CONFLICT (app_client_id, global_user_id) DO UPDATE`:
///   - `pairwise_sub` is re-asserted to the (deterministic) projected
///     value — a no-op in the steady state, but it keeps the row correct
///     if the salt/sector ever rotated.
///   - `revoked_at` is cleared, so a revoke→re-grant reuses the SAME row
///     (the deterministic `pws_` row is never duplicated, §6.4).
///   - `relay_email` is LEFT UNTOUCHED (Slice 5 owns it; the gateway
///     projection never clobbers an alias minted at consent time).
///
/// `app_client_id` is the per-app OAuth client_id (`oac_<base62>`,
/// `route.oauth_client_id`); `pairwise_sub` is the derived `pws_…`.
///
/// # Errors
/// [`GatewayError::Db`] on PG failure. Callers treat this as non-fatal
/// (the projected `ZeroShip-User` header is already correct) and
/// log-and-continue — the mapping is a reverse-lookup cache, not part of
/// the per-request trust decision.
pub async fn upsert(
    conn: &Client,
    app_client_id: &str,
    global_user_id: Uuid,
    pairwise_sub: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO auth.app_user_identities \
            (app_client_id, global_user_id, pairwise_sub) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (app_client_id, global_user_id) DO UPDATE SET \
            pairwise_sub = EXCLUDED.pairwise_sub, \
            revoked_at = NULL",
        &[&app_client_id, &global_user_id, &pairwise_sub],
    )
    .await
    .map_err(|e| GatewayError::Db(format!("app_user_identities upsert: {e}")))?;
    Ok(())
}

/// Read the persisted `pairwise_sub` for `(app_client_id,
/// global_user_id)`, or `None` when no (live) row exists. Used by tests
/// and support tooling to confirm the mapping the gateway wrote.
///
/// # Errors
/// [`GatewayError::Db`] on PG failure.
pub async fn lookup_pairwise_sub(
    conn: &Client,
    app_client_id: &str,
    global_user_id: Uuid,
) -> Result<Option<String>> {
    let rows = conn
        .query(
            "SELECT pairwise_sub FROM auth.app_user_identities \
             WHERE app_client_id = $1 AND global_user_id = $2",
            &[&app_client_id, &global_user_id],
        )
        .await
        .map_err(|e| GatewayError::Db(format!("app_user_identities lookup: {e}")))?;
    Ok(rows.first().map(|row| row.get("pairwise_sub")))
}
