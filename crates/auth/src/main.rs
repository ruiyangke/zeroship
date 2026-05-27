//! zeroship-auth — the OIDC IdP login UI + identity flows + hydra admin client.
//!
//! Companion process: `oryd/hydra` (OIDC kernel). See docs/proposals/auth-server.md.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use clap::Parser;
use compio_postgres::{connect, NoTls};

use zeroship_auth::bootstrap;
use zeroship_auth::config::AuthConfig;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::server;
use zeroship_auth::store;

#[ntex::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    zeroship_core::observability::init_tracing("info,zeroship_auth=debug");

    let cfg = AuthConfig::parse();
    tracing::info!(addr = %cfg.addr, "starting zeroship-auth");

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

    // 4. Serve. `server::run` takes ownership of the PG client (it wraps
    // it in `Arc` internally) so the spawned connection task stays live
    // for the entire server lifetime — `Arc` keeps the client alive
    // across worker tasks; on shutdown the last `Arc` drop unblocks the
    // background connection driver.
    server::run(cfg, admin, client).await?;
    Ok(())
}
