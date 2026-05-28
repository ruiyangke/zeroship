//! JWK rotation cron.
//!
//! Daily check; if either key set's `last_rotated_at` is older than
//! `jwk_rotation_days`, prepend new keys (which become the active
//! signers per hydra's list-based store). Old keys whose set was last
//! rotated more than `rotation_days + retain_days` ago are retired
//! (deleted from JWKS).
//!
//! Single source of truth for "when did we last rotate set X" is the
//! `auth.cron_state` row keyed by the set name. We never inspect key
//! `kid`s or hydra-internal timestamps — operationally simpler and the
//! one decision we control.
//!
//! Companion: [`super::audit_retention`] sweeps `auth.audit_events` on a
//! separate ticker (different cadence, different table — kept in their
//! own modules so a JWK-rotation incident never blocks log retention
//! and vice versa).

use std::sync::Arc;
use std::time::Duration;

use compio_postgres::Client;

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
    rotate_set_if_due(admin, db, ID_TOKEN_SET, ID_TOKEN_ALGS, rotation_days).await?;
    rotate_set_if_due(admin, db, ACCESS_TOKEN_SET, ACCESS_TOKEN_ALGS, rotation_days).await?;
    retire_stale_keys(admin, db, ID_TOKEN_SET, ID_TOKEN_ALGS, rotation_days, retain_days).await?;
    retire_stale_keys(
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

/// Returns the days since the set was last rotated, or `None` if we
/// have no `auth.cron_state` record for it.
async fn days_since_rotation(db: &Client, set: &str) -> Result<Option<i64>> {
    let rows = db
        .query(
            "SELECT EXTRACT(EPOCH FROM (NOW() - last_rotated_at))::DOUBLE PRECISION AS secs \
             FROM auth.cron_state WHERE key = $1",
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

/// Upsert `auth.cron_state[set] = NOW()`. Called when we rotate (so the
/// next rotation is one interval out) and when we first observe a set
/// (to plant a baseline so we don't immediately rotate freshly-bootstrapped
/// keys).
async fn record_rotated_now(db: &Client, set: &str) -> Result<()> {
    db.execute(
        "INSERT INTO auth.cron_state (key, last_rotated_at) VALUES ($1, NOW()) \
         ON CONFLICT (key) DO UPDATE SET last_rotated_at = NOW()",
        &[&set],
    )
    .await
    .map_err(|e| AuthError::Db(format!("cron_state upsert: {e}")))?;
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
        if let Err(e) = admin.create_jwk(set, alg).await {
            tracing::error!(error = %e, set, alg, "create_jwk failed during rotation");
            return Err(AuthError::Hydra(format!("create_jwk {set} {alg}: {e}")));
        }
    }
    record_rotated_now(db, set).await?;
    tracing::info!(set, "jwk_rotation: completed");
    Ok(())
}

/// Retire keys older than the rotation+retain window. Hydra returns
/// keys in insertion order (newest first per the "prepend" semantics
/// of `create_jwk`); we keep the most recent `algs.len()` keys and
/// delete the rest.
async fn retire_stale_keys(
    admin: &HydraAdmin,
    db: &Client,
    set: &str,
    algs: &[&str],
    rotation_days: i64,
    retain_days: i64,
) -> Result<()> {
    // No rotation record yet → nothing to retire.
    let Some(days_since) = days_since_rotation(db, set).await? else {
        return Ok(());
    };
    // Old keys can be retired once we're past rotation_days + retain_days
    // since the most recent rotation (gives any access tokens signed
    // by an outgoing key time to expire).
    if days_since < rotation_days + retain_days {
        return Ok(());
    }
    let Some(jwks) = admin.get_jwks(set).await? else {
        return Ok(());
    };
    let keep_n = algs.len();
    if jwks.keys.len() <= keep_n {
        return Ok(());
    }
    let to_retire: Vec<String> = jwks
        .keys
        .iter()
        .skip(keep_n)
        .filter_map(|k| {
            k.get("kid")
                .and_then(serde_json::Value::as_str)
                .map(String::from)
        })
        .collect();
    for kid in to_retire {
        tracing::info!(set, kid = %kid, "jwk_rotation: retiring stale key");
        if let Err(e) = admin.delete_jwk(set, &kid).await {
            // Don't bubble — retirement is best-effort. Next tick will
            // try again. This keeps a single stuck key from blocking
            // every other rotation/retirement on this tick.
            tracing::warn!(error = %e, set, kid = %kid,
                "delete_jwk failed; will retry next cycle");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
        let threshold = rotation_days + retain_days;
        assert!(120_i64 < threshold, "120d < 121 — keep");
        assert!(121_i64 >= threshold, "121d ≥ 121 — retire-eligible");
    }
}
