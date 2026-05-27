//! zeroship-auth — the `OIDC` `IdP` login UI + identity flows + hydra admin client.
//!
//! Companion process: `oryd/hydra` (OIDC kernel). See docs/proposals/auth-server.md.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::sync::Arc;

use clap::Parser;
use compio_postgres::{connect, NoTls};
use zeroship_core::oidc_verify::JwksCache;

use zeroship_auth::bootstrap;
use zeroship_auth::config::AuthConfig;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::identity::oauth::google;
use zeroship_auth::server;
use zeroship_auth::store;

#[ntex::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    zeroship_core::observability::init_tracing("info,zeroship_auth=debug");

    let cfg = AuthConfig::parse();
    tracing::info!(addr = %cfg.addr, "starting zeroship-auth");

    // OAuth provider credentials are optional. We log a warning per disabled
    // provider so it's obvious during boot which federation arms aren't wired
    // up. Actual route gating happens in U2.2 (Google) + U3.2 (GitHub).
    if cfg.google_client_id.is_none() {
        tracing::warn!(
            "Google OAuth disabled — set AUTH_GOOGLE_CLIENT_ID + AUTH_GOOGLE_CLIENT_SECRET to enable"
        );
    }
    if cfg.github_client_id.is_none() {
        tracing::warn!(
            "GitHub OAuth disabled — set AUTH_GITHUB_CLIENT_ID + AUTH_GITHUB_CLIENT_SECRET to enable"
        );
    }

    // Loud warning when the stash signing key still has its dev default —
    // production deployments MUST override AUTH_STASH_SIGNING_KEY.
    if cfg.stash_signing_key.starts_with("dev-only-") {
        tracing::warn!(
            "AUTH_STASH_SIGNING_KEY is using the dev default — set a strong (≥32-byte) value before serving real traffic"
        );
    }

    // 1. Open PG.
    let (client, connection) = connect(&cfg.db_url, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            tracing::error!(error = %e, "auth/pg connection error");
        }
    })
    .detach();

    // 2. Run migrations.
    store::migrations::migrate(&client).await?;
    tracing::info!("auth.* migrations applied");

    // 3. Bootstrap: keys + client reconciliation.
    let admin = HydraAdmin::new(&cfg.hydra_admin);
    bootstrap::run(&admin, cfg.bootstrap, &cfg.clients_config).await?;
    tracing::info!("bootstrap complete");

    // 4. Build the Google JWKS cache. Only constructed when Google OAuth
    //    is wired up — the cache eagerly does nothing (lazy refresh on
    //    first verify), so we don't burn a startup roundtrip on Google.
    let google_jwks = if cfg.google_client_id.is_some() {
        Some(Arc::new(JwksCache::new(google::GOOGLE_JWKS_URL)))
    } else {
        None
    };

    // 5. Serve. `server::run` takes ownership of the PG client (it wraps
    //    it in `Arc` internally) so the spawned connection task stays
    //    live for the entire server lifetime — `Arc` keeps the client
    //    alive across worker tasks; on shutdown the last `Arc` drop
    //    unblocks the background connection driver.
    server::run(cfg, admin, client, google_jwks).await?;
    Ok(())
}
