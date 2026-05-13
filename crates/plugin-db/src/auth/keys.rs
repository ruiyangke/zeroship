//! Key rotation primitives.
//!
//! The platform's HMAC secret is rotated periodically — daily by
//! default, per the proposal — by inserting a fresh `gen_random_bytes(32)`
//! row into `__zeroship_admin.hmac_keys` and marking the previous
//! `current` row as `retired_at = NOW()`. Within a 24-hour grace
//! window, `verify_signature` accepts either key; that's enough for
//! any in-flight tokens (TTL = 5 min) to drain.
//!
//! ## What's shipped here (Stage 4 — partial)
//!
//! - [`rotate_session_keys`]: idempotent rotation primitive that calls
//!   the SECURITY DEFINER function installed by `bootstrap.rs`.
//! - [`current_key_id`] / [`previous_key_id`]: inspection helpers for
//!   tests + operational tooling.
//!
//! ## What's deferred
//!
//! - The maintenance cron that drives daily rotation. The control
//!   plane owns scheduling; once the maintenance scheduler exists
//!   (today there's no first-class `crates/control` cron primitive
//!   for this), it adds a `rotate_session_keys` job at cluster
//!   bootstrap. Until then operators invoke rotation via:
//!     `psql -c "SELECT __zeroship_admin.rotate_session_keys();"`
//!   or — equivalently — `auth::keys::rotate_session_keys(pool)` from
//!   the runtime.
//! - The control-plane REST endpoint that exposes emergency rotation.

use compio_postgres::Pool;

use super::ADMIN_SCHEMA;

/// Result of a rotation. `previous_key_id` is the key that was retired
/// (or `None` if there was no previous current — initial bootstrap).
#[derive(Debug, Clone)]
pub struct RotationOutcome {
    pub new_key_id: i64,
    pub previous_key_id: Option<i64>,
}

impl RotationOutcome {
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "newKeyId":      self.new_key_id,
            "previousKeyId": self.previous_key_id,
        })
        .to_string()
    }
}

/// Run a key rotation. Idempotent in the sense that repeated calls
/// produce distinct keys but never leave the cluster in an
/// unverifiable state — each call's outcome stands alone.
pub async fn rotate_session_keys(pool: &Pool) -> Result<RotationOutcome, String> {
    // Capture the current key id before rotation so we can return it
    // as `previous_key_id`.
    let prev = current_key_id(pool).await?;

    // Cast to text so the binary-format result decoder doesn't need
    // a typed int8 decoder.
    let rows = pool
        .query_text_params(
            &format!("SELECT \"{ADMIN_SCHEMA}\".rotate_session_keys()::text AS new_id"),
            &[],
        )
        .await
        .map_err(|e| format!("auth/keys: rotate_session_keys: {e}"))?;

    let new_id_str: String = rows
        .first()
        .and_then(|r| r.try_get::<_, String>("new_id").ok())
        .ok_or_else(|| "auth/keys: rotate_session_keys returned no row".to_string())?;
    let new_id: i64 = new_id_str
        .parse()
        .map_err(|e| format!("auth/keys: parse new_id {new_id_str:?}: {e}"))?;

    Ok(RotationOutcome {
        new_key_id: new_id,
        previous_key_id: prev,
    })
}

/// The currently active key id — the one new tokens will be signed
/// with. `None` if no active key exists (only possible before
/// bootstrap or in a misconfigured cluster).
pub async fn current_key_id(pool: &Pool) -> Result<Option<i64>, String> {
    let rows = pool
        .query_text_params(
            &format!(
                "SELECT key_id::text AS id
                 FROM \"{ADMIN_SCHEMA}\".hmac_keys
                 WHERE retired_at IS NULL
                 ORDER BY created_at DESC LIMIT 1"
            ),
            &[],
        )
        .await
        .map_err(|e| format!("auth/keys: current_key_id: {e}"))?;
    let row = match rows.first() {
        Some(r) => r,
        None => return Ok(None),
    };
    let s: String = row
        .try_get::<_, String>("id")
        .map_err(|e| format!("auth/keys: parse current id: {e}"))?;
    s.parse::<i64>()
        .map(Some)
        .map_err(|e| format!("auth/keys: parse current id: {e}"))
}

/// The most-recently-retired key id (still inside the grace window).
/// `None` if no retired key exists.
pub async fn previous_key_id(pool: &Pool) -> Result<Option<i64>, String> {
    let rows = pool
        .query_text_params(
            &format!(
                "SELECT key_id::text AS id
                 FROM \"{ADMIN_SCHEMA}\".hmac_keys
                 WHERE retired_at IS NOT NULL
                   AND retired_at > NOW() - INTERVAL '24 hours'
                 ORDER BY retired_at DESC LIMIT 1"
            ),
            &[],
        )
        .await
        .map_err(|e| format!("auth/keys: previous_key_id: {e}"))?;
    let row = match rows.first() {
        Some(r) => r,
        None => return Ok(None),
    };
    let s: String = row
        .try_get::<_, String>("id")
        .map_err(|e| format!("auth/keys: parse previous id: {e}"))?;
    s.parse::<i64>()
        .map(Some)
        .map_err(|e| format!("auth/keys: parse previous id: {e}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_json_shape() {
        let r = RotationOutcome {
            new_key_id: 7,
            previous_key_id: Some(6),
        };
        let v: serde_json::Value = serde_json::from_str(&r.to_json()).unwrap();
        assert_eq!(v["newKeyId"], 7);
        assert_eq!(v["previousKeyId"], 6);
    }

    #[test]
    fn outcome_json_no_previous() {
        let r = RotationOutcome {
            new_key_id: 1,
            previous_key_id: None,
        };
        let v: serde_json::Value = serde_json::from_str(&r.to_json()).unwrap();
        assert_eq!(v["newKeyId"], 1);
        assert!(v["previousKeyId"].is_null());
    }
}
