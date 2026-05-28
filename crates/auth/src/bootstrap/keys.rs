//! First-boot JWK generation. Called only when `--bootstrap` is set.
//!
//! Per-algorithm idempotency: for each requested algorithm in a keyset,
//! we add a key only if no key with that `alg` is already present. This
//! lets a re-boot heal a partially-populated keyset (e.g. one that has
//! `RS256` only, and now needs `EdDSA` added) without disturbing existing
//! keys.

use std::collections::HashSet;

use compio_postgres::Client;

use crate::advisory_lock::{with_advisory_lock, BOOTSTRAP_SIGNING_KEYS_LOCK};
use crate::error::Result;
use crate::hydra_client::HydraAdmin;

pub const ID_TOKEN_SET: &str = "hydra.openid.id-token";
pub const ACCESS_TOKEN_SET: &str = "hydra.jwt.access-token";

/// Ensures hydra has a signing key for each requested algorithm in each set.
/// Idempotent: per-algorithm presence check, so re-running is safe and will
/// only add keys for algorithms that are missing.
///
/// # Errors
///
/// Propagates any [`AuthError::Hydra`](crate::error::AuthError::Hydra) error
/// from the admin API (network failures, non-2xx responses, decode errors).
pub async fn ensure_signing_keys(admin: &HydraAdmin, db: &Client) -> Result<()> {
    with_advisory_lock(db, BOOTSTRAP_SIGNING_KEYS_LOCK, || async {
        ensure_set(admin, ID_TOKEN_SET, &["EdDSA", "RS256"]).await?;
        ensure_set(admin, ACCESS_TOKEN_SET, &["EdDSA"]).await?;
        Ok(())
    })
    .await
}

async fn ensure_set(admin: &HydraAdmin, set: &str, algs: &[&str]) -> Result<()> {
    let existing = admin.get_jwks(set).await?;
    let existing_algs: HashSet<String> = match existing {
        Some(j) => j
            .keys
            .iter()
            .filter_map(|k| {
                k.get("alg")
                    .and_then(serde_json::Value::as_str)
                    .map(std::string::ToString::to_string)
            })
            .collect(),
        None => HashSet::new(),
    };

    for alg in algs {
        if existing_algs.contains(*alg) {
            tracing::info!(set, alg, "hydra key already present; skipping");
            continue;
        }
        tracing::info!(set, alg, "creating hydra JWK");
        admin.create_jwk(set, alg).await?;
    }
    Ok(())
}
