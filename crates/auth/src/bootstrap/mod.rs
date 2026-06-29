//! First-boot bootstrap orchestrator.
//!
//! Order on startup:
//!   1. `ensure_signing_keys` — only runs if `--bootstrap`.
//!   2. `reconcile_clients`    — runs always.
//!
//! Client reconciliation is upsert-style: declared clients are created or
//! updated; clients in hydra not in the config are LEFT ALONE (we don't
//! want a bootstrap loop to nuke a manually-registered third-party client).

pub mod clients_config;
pub mod keys;

use compio_postgres::Client;

use crate::advisory_lock::{with_advisory_lock, BOOTSTRAP_CLIENTS_LOCK};
use crate::error::{AuthError, Result};
use crate::hydra_client::HydraAdmin;
use clients_config::ClientsConfig;

/// Run the first-boot bootstrap sequence.
///
/// When `allow_bootstrap` is true, missing signing keys are generated.
/// When false, empty key sets are treated as a fatal misconfiguration —
/// silently re-generating keys on a fresh DB clone would invalidate every
/// issued token.
///
/// # Errors
///
/// - [`AuthError::Bootstrap`](crate::error::AuthError::Bootstrap) if
///   `allow_bootstrap` is false and either key set is empty, or if the
///   clients config cannot be read or parsed.
/// - [`AuthError::Hydra`](crate::error::AuthError::Hydra) for any admin
///   API failure during key generation or client reconciliation.
pub async fn run(
    admin: &HydraAdmin,
    db: &Client,
    allow_bootstrap: bool,
    clients_config_path: &str,
) -> Result<()> {
    if allow_bootstrap {
        keys::ensure_signing_keys(admin, db).await?;
    } else if keys_empty(admin).await? {
        return Err(crate::error::AuthError::Bootstrap(
            "hydra signing-key sets are empty; restart with --bootstrap to generate".into(),
        ));
    }

    reconcile_clients(admin, db, clients_config_path).await
}

async fn keys_empty(admin: &HydraAdmin) -> Result<bool> {
    let a = admin.get_jwks(keys::ID_TOKEN_SET).await?;
    let b = admin.get_jwks(keys::ACCESS_TOKEN_SET).await?;
    Ok(a.is_none_or(|j| j.keys.is_empty()) ||
       b.is_none_or(|j| j.keys.is_empty()))
}

async fn reconcile_clients(admin: &HydraAdmin, db: &Client, path: &str) -> Result<()> {
    let cfg = ClientsConfig::from_path(path)?;
    with_advisory_lock(db, BOOTSTRAP_CLIENTS_LOCK, || async {
        for entry in &cfg.clients {
            let desired = entry.to_oauth2_client();
            match admin.get_client(&entry.client_id).await? {
                None => {
                    tracing::info!(client_id = %entry.client_id, "registering OIDC client");
                    admin.create_client(&desired).await?;
                }
                Some(_existing) => {
                    tracing::info!(client_id = %entry.client_id, "updating OIDC client");
                    admin.update_client(&desired).await?;
                }
            }
            upsert_local_client(db, entry).await?;
        }
        Ok(())
    })
    .await
}

async fn upsert_local_client(
    db: &Client,
    entry: &clients_config::ClientEntry,
) -> Result<()> {
    let client_name = entry
        .client_name
        .clone()
        .unwrap_or_else(|| entry.client_id.clone());
    let scopes = entry
        .scope
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    db.execute(
        "INSERT INTO zeroship.oauth_clients \
             (client_id, client_name, redirect_uris, scopes, skip_consent, hydra_client_id) \
         VALUES ($1, $2, $3, $4, $5, $1) \
         ON CONFLICT (client_id) DO UPDATE \
         SET client_name = EXCLUDED.client_name, \
             redirect_uris = EXCLUDED.redirect_uris, \
             scopes = EXCLUDED.scopes, \
             skip_consent = EXCLUDED.skip_consent, \
             hydra_client_id = EXCLUDED.hydra_client_id",
        &[
            &entry.client_id,
            &client_name,
            &entry.redirect_uris,
            &scopes,
            &entry.first_party,
        ],
    )
    .await
    .map_err(|e| AuthError::Db(format!("upsert zeroship.oauth_clients: {e}")))?;
    Ok(())
}
