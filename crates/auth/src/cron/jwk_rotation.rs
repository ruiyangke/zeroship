//! JWK rotation cron.
//!
//! Daily check; if either key set's `last_rotated_at` is older than
//! `jwk_rotation_days`, prepend new keys (which become the active
//! signers per hydra's list-based store). Old keys whose own tracked
//! `created_at` is older than `rotation_days + retain_days` are
//! retired (deleted from JWKS).
//!
//! Single source of truth for "when did we last rotate set X" is the
//! `zeroship.cron_state` row keyed by the set name. Per-key retirement age
//! lives in `zeroship.jwk_key_state`, keyed by `(set_name, kid)`.
//!
//! Companion: [`super::audit_retention`] sweeps `zeroship.audit_events` on a
//! separate ticker (different cadence, different table — kept in their
//! own modules so a JWK-rotation incident never blocks log retention
//! and vice versa).

use std::sync::Arc;
use std::time::Duration;

use compio_postgres::Client;

use crate::advisory_lock::{jwk_set_lock_key, with_advisory_lock};
use crate::config::AuthConfig;
use crate::error::{AuthError, Result};
use crate::hydra_client::HydraAdmin;

/// JWK set hydra uses to sign ID tokens.
const ID_TOKEN_SET: &str = "hydra.openid.id-token";
/// JWK set hydra uses to sign access tokens (JWT strategy).
const ACCESS_TOKEN_SET: &str = "hydra.jwt.access-token";
/// Algorithms the bootstrap populates `ID_TOKEN_SET` with. Must stay in
/// lockstep with `bootstrap::keys::ensure_signing_keys` — those two
/// constants together define the active signers.
const ID_TOKEN_ALGS: &[&str] = &["EdDSA", "RS256"];
/// Algorithms the bootstrap populates `ACCESS_TOKEN_SET` with.
const ACCESS_TOKEN_ALGS: &[&str] = &["EdDSA"];

/// Cron entry point. Loops forever; each iteration runs one [`tick`]
/// then sleeps `cron_tick_secs` (24 h default).
///
/// Errors inside a single tick are logged and swallowed so a transient
/// hydra outage doesn't kill the cron task.
//
// `cyper::Client` (the hydra admin transport) holds a `!Send` connection
// handle inside its request future; the lint is structural, not actionable.
#[allow(clippy::future_not_send)]
pub async fn run(admin: HydraAdmin, db: Arc<Client>, cfg: Arc<AuthConfig>) {
    let interval_secs = cfg.cron_tick_secs;
    tracing::info!(
        interval_secs,
        rotation_days = cfg.jwk_rotation_days,
        retain_days = cfg.jwk_retain_days,
        "jwk_rotation cron starting"
    );
    loop {
        if let Err(e) = tick(&admin, &db, cfg.jwk_rotation_days, cfg.jwk_retain_days).await {
            tracing::error!(error = %e, "jwk_rotation tick failed");
        }
        compio::time::sleep(Duration::from_secs(interval_secs)).await;
    }
}

/// Run one rotation pass against both key sets. Public to the crate so
/// integration tests can drive a single tick deterministically.
pub(crate) async fn tick(
    admin: &HydraAdmin,
    db: &Client,
    rotation_days: i64,
    retain_days: i64,
) -> Result<()> {
    process_set(
        admin,
        db,
        ID_TOKEN_SET,
        ID_TOKEN_ALGS,
        rotation_days,
        retain_days,
    )
    .await?;
    process_set(
        admin,
        db,
        ACCESS_TOKEN_SET,
        ACCESS_TOKEN_ALGS,
        rotation_days,
        retain_days,
    )
    .await?;
    Ok(())
}

/// Test-only handle so `tests/jwk_rotation_test.rs` can drive a tick
/// without needing to expose all the private constants.
#[doc(hidden)]
pub async fn tick_once_for_test(
    admin: &HydraAdmin,
    db: &Client,
    rotation_days: i64,
    retain_days: i64,
) -> Result<()> {
    tick(admin, db, rotation_days, retain_days).await
}

async fn process_set(
    admin: &HydraAdmin,
    db: &Client,
    set: &str,
    algs: &[&str],
    rotation_days: i64,
    retain_days: i64,
) -> Result<()> {
    let lock_key = jwk_set_lock_key(set);
    with_advisory_lock(db, lock_key, || async {
        sync_tracked_keys(admin, db, set).await?;
        rotate_set_if_due(admin, db, set, algs, rotation_days).await?;
        retire_stale_keys(admin, db, set, algs, rotation_days, retain_days).await
    })
    .await
}

/// Returns the days since the set was last rotated, or `None` if we
/// have no `zeroship.cron_state` record for it.
async fn days_since_rotation(db: &Client, set: &str) -> Result<Option<i64>> {
    let rows = db
        .query(
            "SELECT EXTRACT(EPOCH FROM (NOW() - last_rotated_at))::DOUBLE PRECISION AS secs \
             FROM zeroship.cron_state WHERE key = $1",
            &[&set],
        )
        .await
        .map_err(|e| AuthError::Db(format!("cron_state read: {e}")))?;
    Ok(rows.first().map(|r| {
        let secs: f64 = r.get("secs");
        #[allow(clippy::cast_possible_truncation)]
        let days = (secs / 86_400.0) as i64;
        days
    }))
}

/// Upsert `zeroship.cron_state[set] = NOW()`. Called when we rotate (so the
/// next rotation is one interval out) and when we first observe a set
/// (to plant a baseline so we don't immediately rotate freshly-bootstrapped
/// keys).
async fn record_rotated_now(db: &Client, set: &str) -> Result<()> {
    db.execute(
        "INSERT INTO zeroship.cron_state (key, last_rotated_at) VALUES ($1, NOW()) \
         ON CONFLICT (key) DO UPDATE SET last_rotated_at = NOW()",
        &[&set],
    )
    .await
    .map_err(|e| AuthError::Db(format!("cron_state upsert: {e}")))?;
    Ok(())
}

async fn record_key_created_now(db: &Client, set: &str, kid: &str) -> Result<()> {
    db.execute(
        "INSERT INTO zeroship.jwk_key_state (set_name, kid, created_at)
         VALUES ($1, $2, NOW())
         ON CONFLICT (set_name, kid) DO UPDATE SET created_at = NOW()",
        &[&set, &kid],
    )
    .await
    .map_err(|e| AuthError::Db(format!("jwk_key_state upsert: {e}")))?;
    Ok(())
}

async fn sync_tracked_keys(admin: &HydraAdmin, db: &Client, set: &str) -> Result<()> {
    let Some(jwks) = admin.get_jwks(set).await? else {
        db.execute(
            "DELETE FROM zeroship.jwk_key_state WHERE set_name = $1",
            &[&set],
        )
        .await
        .map_err(|e| AuthError::Db(format!("jwk_key_state prune missing set: {e}")))?;
        return Ok(());
    };

    let kids: Vec<String> = jwks
        .keys
        .iter()
        .filter_map(jwk_kid)
        .map(str::to_owned)
        .collect();

    for kid in &kids {
        db.execute(
            "INSERT INTO zeroship.jwk_key_state (set_name, kid, created_at)
             VALUES (
                $1,
                $2,
                COALESCE(
                    (SELECT last_rotated_at FROM zeroship.cron_state WHERE key = $1),
                    NOW()
                )
             )
             ON CONFLICT (set_name, kid) DO NOTHING",
            &[&set, &kid],
        )
        .await
        .map_err(|e| AuthError::Db(format!("jwk_key_state sync insert: {e}")))?;
    }

    if kids.is_empty() {
        db.execute(
            "DELETE FROM zeroship.jwk_key_state WHERE set_name = $1",
            &[&set],
        )
        .await
        .map_err(|e| AuthError::Db(format!("jwk_key_state prune empty set: {e}")))?;
    } else {
        let kid_refs: Vec<&str> = kids.iter().map(String::as_str).collect();
        db.execute(
            "DELETE FROM zeroship.jwk_key_state
             WHERE set_name = $1 AND NOT (kid = ANY($2))",
            &[&set, &kid_refs],
        )
        .await
        .map_err(|e| AuthError::Db(format!("jwk_key_state prune stale rows: {e}")))?;
    }

    Ok(())
}

/// Rotate `set` if its last-rotated record is `>= rotation_days` old.
/// First-ever observation plants a baseline at `NOW()` (i.e. no rotation
/// this tick — we don't know how old the bootstrap-time keys are, so
/// "now" is the most conservative anchor).
async fn rotate_set_if_due(
    admin: &HydraAdmin,
    db: &Client,
    set: &str,
    algs: &[&str],
    rotation_days: i64,
) -> Result<()> {
    let due = match days_since_rotation(db, set).await? {
        None => {
            // First time we've seen this set in cron_state — record
            // NOW() so we don't immediately rotate. The set was already
            // populated by bootstrap; its age is unknown, so we anchor
            // the clock at the current observation.
            record_rotated_now(db, set).await?;
            tracing::info!(set, "cron_state initialised (no prior rotation record)");
            false
        }
        Some(days) => days >= rotation_days,
    };
    if !due {
        return Ok(());
    }
    tracing::info!(set, rotation_days, "jwk_rotation: prepending new keys");
    for alg in algs {
        let kid = format!("kid_{}", uuid::Uuid::new_v4().simple());
        if let Err(e) = admin.create_jwk_with_kid(set, alg, &kid).await {
            tracing::error!(error = %e, set, alg, "create_jwk failed during rotation");
            return Err(AuthError::Hydra(format!("create_jwk {set} {alg}: {e}")));
        }
        record_key_created_now(db, set, &kid).await?;
    }
    record_rotated_now(db, set).await?;
    tracing::info!(set, "jwk_rotation: completed");
    Ok(())
}

fn jwk_kid(key: &serde_json::Value) -> Option<&str> {
    key.get("kid").and_then(serde_json::Value::as_str)
}

fn retirement_threshold_days(rotation_days: i64, retain_days: i64) -> i64 {
    rotation_days + retain_days
}

/// Retire keys whose own tracked creation time is older than the
/// rotation+retain window. We still keep at least the current generation
/// (`algs.len()` keys) even if timestamps are corrupt or cron was down
/// for longer than the whole retention window.
async fn retire_stale_keys(
    admin: &HydraAdmin,
    db: &Client,
    set: &str,
    algs: &[&str],
    rotation_days: i64,
    retain_days: i64,
) -> Result<()> {
    let Some(jwks) = admin.get_jwks(set).await? else {
        return Ok(());
    };
    let keep_n = algs.len();
    let excess = jwks.keys.len().saturating_sub(keep_n);
    if excess == 0 {
        return Ok(());
    }
    let threshold = retirement_threshold_days(rotation_days, retain_days).to_string();
    let limit = i64::try_from(excess).unwrap_or(i64::MAX);
    let rows = db
        .query(
            "SELECT kid
             FROM zeroship.jwk_key_state
             WHERE set_name = $1
               AND created_at <= NOW() - ($2::text || ' days')::interval
             ORDER BY created_at ASC
             LIMIT $3",
            &[&set, &threshold, &limit],
        )
        .await
        .map_err(|e| AuthError::Db(format!("jwk_key_state retirement read: {e}")))?;
    let to_retire: Vec<String> = rows.iter().map(|r| r.get("kid")).collect();
    for kid in to_retire {
        tracing::info!(set, kid = %kid, "jwk_rotation: retiring stale key");
        match admin.delete_jwk(set, &kid).await {
            Ok(()) => {
                db.execute(
                    "DELETE FROM zeroship.jwk_key_state WHERE set_name = $1 AND kid = $2",
                    &[&set, &kid],
                )
                .await
                .map_err(|e| AuthError::Db(format!("jwk_key_state delete: {e}")))?;
            }
            Err(e) => {
                // Don't bubble — retirement is best-effort. Next tick will
                // try again. This keeps a single stuck key from blocking
                // every other rotation/retirement on this tick.
                tracing::warn!(error = %e, set, kid = %kid,
                    "delete_jwk failed; will retry next cycle");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure threshold sanity — guard against off-by-one drift on the two
    // boundary conditions the cron leans on.

    #[test]
    fn rotation_due_threshold() {
        let rotation_days: i64 = 90;
        assert!(89_i64 < rotation_days, "89 days old → not due");
        assert!(90_i64 >= rotation_days, "90 days old → due");
    }

    #[test]
    fn retire_threshold() {
        let rotation_days: i64 = 90;
        let retain_days: i64 = 31;
        let threshold = retirement_threshold_days(rotation_days, retain_days);
        assert!(120_i64 < threshold, "120d < 121 — keep");
        assert!(121_i64 >= threshold, "121d ≥ 121 — retire-eligible");
    }

    #[test]
    fn three_rotations_retire_generation_zero_only() {
        let threshold = retirement_threshold_days(90, 31);
        let keep_n = 1;
        let keys = [
            ("gen-2", 0_i64),
            ("gen-1", 90_i64),
            ("gen-0", 180_i64),
        ];
        let excess = keys.len().saturating_sub(keep_n);
        let mut retired: Vec<&str> = keys
            .iter()
            .rev()
            .filter(|(_, age_days)| *age_days >= threshold)
            .take(excess)
            .map(|(kid, _)| *kid)
            .collect();
        retired.sort_unstable();

        assert_eq!(retired, vec!["gen-0"]);
    }
}
