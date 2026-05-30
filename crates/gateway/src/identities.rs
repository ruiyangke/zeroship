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
    conn: &Client,
    app_client_id: &str,
    global_user_id: Uuid,
) -> Result<Option<String>> {
    let rows = conn
        .query(
            "SELECT relay_email FROM auth.app_user_identities \
             WHERE app_client_id = $1 AND global_user_id = $2 \
               AND relay_email IS NOT NULL \
               AND revoked_at IS NULL",
            &[&app_client_id, &global_user_id],
        )
        .await
        .map_err(|e| GatewayError::Db(format!("app_user_identities relay lookup: {e}")))?;
    Ok(rows.first().and_then(|row| row.get("relay_email")))
}

/// Read the ACTIVE relay alias (`relay_email`) for `(app_client_id,
/// pairwise_sub)`, or `None` when no live alias exists. The pairwise-keyed
/// twin of [`lookup_relay_email`], used by the wrapper fast-paths (Batch A
/// fix 5): a gateway-issued wrapper carries the per-app `pws_…` subject, NOT
/// the global UUID, so the live re-resolution keys on `pairwise_sub` instead
/// of `global_user_id`. The `(app_client_id, pairwise_sub)` pair is unique —
/// `pairwise_sub` is a deterministic projection of one `(app, global_user)`
/// — so this returns the same row [`lookup_relay_email`] would.
///
/// Same fail-closed gate (`relay_email IS NOT NULL AND revoked_at IS NULL`):
/// a revoked grant's alias reads as `None`, so the caller emits an EMPTY email
/// rather than the stale alias the wrapper still carries — the whole point of
/// re-resolving live instead of trusting the (TTL-stale) wrapper claim.
///
/// # Errors
/// [`GatewayError::Db`] on PG failure.
pub async fn lookup_relay_email_by_pairwise(
    conn: &Client,
    app_client_id: &str,
    pairwise_sub: &str,
) -> Result<Option<String>> {
    let rows = conn
        .query(
            "SELECT relay_email FROM auth.app_user_identities \
             WHERE app_client_id = $1 AND pairwise_sub = $2 \
               AND relay_email IS NOT NULL \
               AND revoked_at IS NULL",
            &[&app_client_id, &pairwise_sub],
        )
        .await
        .map_err(|e| GatewayError::Db(format!("app_user_identities relay lookup by pairwise: {e}")))?;
    Ok(rows.first().and_then(|row| row.get("relay_email")))
}
