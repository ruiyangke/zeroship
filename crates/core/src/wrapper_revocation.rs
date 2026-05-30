//! Shared wrapper-token revocation: the spec §8.5 cross-node PER-APP family
//! marker.
//!
//! **`token_revocations`** is the SOLE wrapper-token revocation primitive
//! keyed on `(client_id, sub)` with `sub` stored as TEXT, so it holds the
//! wrapper's `pws_…` pairwise subject (and, on the raw-Hydra arm, the per-app
//! `pws_` derived from the global UUID). The wrapper / Bearer / DPoP arms
//! (§1.3 c-wrap / c-hydra) reject a token when a row exists for its
//! `(client_id, pws_)` with `revoked_after > token.iat`. Per-app scoping means
//! revoking a user on app A leaves their tokens on app B valid.
//!
//! There is no longer a global UUID-keyed subject denylist: every wrapper /
//! raw-Hydra access token is minted under a per-app `oac_…` client and carries
//! (or projects to) a per-app `pws_`, so the per-app family marker is the only
//! key any reader uses. The previous `wrapper_revoked_subjects` denylist was
//! write-only dead code after the per-app cutover (Batch A M2) and is removed —
//! pre-launch, no back-compat (AGENTS.md), so the table and its helpers are
//! deleted rather than left orphaned.

use compio_postgres::{Client, Error};

pub const WRAPPER_REVOCATION_RETENTION_HOURS: i32 = 24;

// ─── Per-app family marker (spec §8.5 `auth.token_revocations`) ──────────
//
// The SOLE cross-node revocation mechanism. Keyed on `(client_id, sub)`
// — one row per token family per app — so signout (which cannot enumerate
// the live `jti`s) can reject every already-minted token for that family
// on every node. `sub` is TEXT and holds the per-app `pws_…` pairwise
// subject every wrapper / raw-Hydra access token carries (or projects to);
// the per-app scoping is exactly what a global UUID-keyed denylist could
// never express for the browser wrapper path.

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
    // fails with a `WrongType` error).
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
/// 24 h retention is comfortably beyond the 10-min wrapper / 1 h raw-Hydra
/// TTLs.
pub async fn sweep_expired_families(db: &Client) -> Result<u64, Error> {
    db.execute(
        "DELETE FROM auth.token_revocations \
         WHERE revoked_after < NOW() - make_interval(hours => $1)",
        &[&WRAPPER_REVOCATION_RETENTION_HOURS],
    )
    .await
}
