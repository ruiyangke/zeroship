//! zeroship-auth — the OIDC IdP login UI + identity flows + hydra admin client.
//!
//! Companion process: `oryd/hydra` (OIDC kernel). See docs/proposals/auth-server.md.

use clap::Parser;

use zeroship_auth::config::AuthConfig;

#[compio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    zeroship_core::observability::init_tracing("info,zeroship_auth=debug");

    let cfg = AuthConfig::parse();
    tracing::info!(addr = %cfg.addr, "starting zeroship-auth");

    // Server start lives in Task 3.
    let _ = cfg;
    Ok(())
}
