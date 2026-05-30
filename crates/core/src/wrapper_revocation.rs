//! Shared wrapper-token subject revocation helpers.
//!
//! Two revocation primitives live here:
//!
//! 1. **`wrapper_revoked_subjects`** — the legacy GLOBAL (all-apps) denylist
//!    keyed on the bare UUID subject. Back-channel logout's shared-`gateway`
//!    path and the DPoP arm key on this. A wrapper/access token is rejected
//!    when its `iat` is at or before the recorded revocation time.
//!
//! 2. **`token_revocations`** — the spec §8.5 cross-node PER-APP family
//!    marker keyed on `(client_id, sub)` with `sub` stored as TEXT, so it
//!    holds BOTH the wrapper's `pws_…` pairwise subject AND the raw-Hydra
//!    global UUID (whichever the token carries). The Bearer arm (§1.3
//!    c-wrap / c-hydra) rejects a token when a row exists for its
//!    `(client_id, sub)` with `revoked_after > token.iat`. Per-app scoping
//!    means revoking a user on app A leaves their tokens on app B valid.

use compio_postgres::{Client, Error};
use uuid::Uuid;

pub const WRAPPER_REVOCATION_RETENTION_HOURS: i32 = 24;

#[must_use]
pub fn subject_uuid(sub: &str) -> Option<Uuid> {
    Uuid::parse_str(sub).ok()
}

pub async fn revoke_subject(db: &Client, subject: Uuid) -> Result<u64, Error> {
    db.execute(
        "INSERT INTO auth.wrapper_revoked_subjects (subject, revoked_at) \
         VALUES ($1, NOW()) \
         ON CONFLICT (subject) DO UPDATE SET revoked_at = EXCLUDED.revoked_at",
        &[&subject],
    )
    .await
}

pub async fn is_subject_revoked_since(
    db: &Client,
    subject: Uuid,
    iat: i64,
) -> Result<bool, Error> {
    // `to_timestamp(...)` takes `double precision`; the explicit
    // `$2::double precision` cast makes Postgres report the bind param as
    // `Float8`, so the value must be encoded as `f64` — binding an `i64`
    // here fails with a "serializing parameter" / `WrongType` error.
    let iat = iat as f64;
    let row = db
        .query_one(
            "SELECT EXISTS ( \
                SELECT 1 FROM auth.wrapper_revoked_subjects \
                WHERE subject = $1 \
                  AND revoked_at >= to_timestamp($2::double precision) \
             ) AS revoked",
            &[&subject, &iat],
        )
        .await?;
    Ok(row.get("revoked"))
}

pub async fn sweep_expired_subjects(db: &Client) -> Result<u64, Error> {
    db.execute(
        "DELETE FROM auth.wrapper_revoked_subjects \
         WHERE revoked_at < NOW() - make_interval(hours => $1)",
        &[&WRAPPER_REVOCATION_RETENTION_HOURS],
    )
    .await
}

// ─── Per-app family marker (spec §8.5 `auth.token_revocations`) ──────────
//
// The PRIMARY cross-node revocation mechanism. Keyed on `(client_id, sub)`
// — one row per token family per app — so signout (which cannot enumerate
// the live `jti`s) can reject every already-minted token for that family
// on every node. `sub` is TEXT so it accepts the wrapper's `pws_…` pairwise
// subject as well as the raw-Hydra global UUID; this is the gap the
// UUID-only `wrapper_revoked_subjects` denylist could not cover for the
// browser wrapper path.

/// Upsert the per-app family marker for `(client_id, sub)`, stamping
/// `revoked_after = NOW()`. Any token in this family with `iat < NOW()` is
/// rejected from here on. Per-app: a marker for app A's `client_id` does
/// NOT affect app B.
pub async fn revoke_family(db: &Client, client_id: &str, sub: &str) -> Result<u64, Error> {
    db.execute(
        "INSERT INTO auth.token_revocations (client_id, sub, revoked_after) \
         VALUES ($1, $2, NOW()) \
         ON CONFLICT (client_id, sub) DO UPDATE SET revoked_after = EXCLUDED.revoked_after",
        &[&client_id, &sub],
    )
    .await
}

/// Whether the `(client_id, sub)` family was revoked AFTER `iat` — i.e. a
/// row exists with `revoked_after > to_timestamp(iat)`. A token whose `iat`
/// predates the marker is rejected; one minted after the marker is fine.
pub async fn is_family_revoked_since(
    db: &Client,
    client_id: &str,
    sub: &str,
    iat: i64,
) -> Result<bool, Error> {
    // `to_timestamp(...)` takes `double precision`; the explicit
    // `$3::double precision` cast makes Postgres report the bind param as
    // `Float8`, so the value must be encoded as `f64` (binding an `i64`
    // fails with a `WrongType` error). Matches `is_subject_revoked_since`.
    let iat = iat as f64;
    let row = db
        .query_one(
            "SELECT EXISTS ( \
                SELECT 1 FROM auth.token_revocations \
                WHERE client_id = $1 \
                  AND sub = $2 \
                  AND revoked_after > to_timestamp($3::double precision) \
             ) AS revoked",
            &[&client_id, &sub, &iat],
        )
        .await?;
    Ok(row.get("revoked"))
}

/// Sweep family markers older than the retention window. Markers only need
/// to outlive the longest-lived token whose `iat` could predate them; the
/// same 24 h retention as the subject denylist is comfortably beyond the
/// 10-min wrapper / 1 h raw-Hydra TTLs.
pub async fn sweep_expired_families(db: &Client) -> Result<u64, Error> {
    db.execute(
        "DELETE FROM auth.token_revocations \
         WHERE revoked_after < NOW() - make_interval(hours => $1)",
        &[&WRAPPER_REVOCATION_RETENTION_HOURS],
    )
    .await
}
