//! First-boot JWK generation. Called only when `--bootstrap` is set
//! and the relevant hydra key sets are empty.

use crate::error::Result;
use crate::hydra_client::HydraAdmin;

pub const ID_TOKEN_SET: &str = "hydra.openid.id-token";
pub const ACCESS_TOKEN_SET: &str = "hydra.jwt.access-token";

/// Ensures hydra has at least one signing key in each set. Idempotent:
/// returns immediately if the set is non-empty.
///
/// # Errors
///
/// Propagates any [`AuthError::Hydra`](crate::error::AuthError::Hydra) error
/// from the admin API (network failures, non-2xx responses, decode errors).
pub async fn ensure_signing_keys(admin: &HydraAdmin) -> Result<()> {
    ensure_set(admin, ID_TOKEN_SET, &["EdDSA", "RS256"]).await?;
    ensure_set(admin, ACCESS_TOKEN_SET, &["EdDSA"]).await?;
    Ok(())
}

async fn ensure_set(admin: &HydraAdmin, set: &str, algs: &[&str]) -> Result<()> {
    let existing = admin.get_jwks(set).await?;
    let count = existing.as_ref().map_or(0, |j| j.keys.len());
    if count > 0 {
        tracing::info!(set, count, "hydra key set already populated; skipping");
        return Ok(());
    }
    for alg in algs {
        tracing::info!(set, alg, "creating hydra JWK");
        admin.create_jwk(set, alg).await?;
    }
    Ok(())
}
