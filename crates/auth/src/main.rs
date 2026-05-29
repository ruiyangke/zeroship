//! zeroship-auth — the `OIDC` `IdP` login UI + identity flows + hydra admin client.
//!
//! Companion process: `oryd/hydra` (OIDC kernel). See docs/proposals/auth-server.md.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::sync::Arc;

use clap::Parser;
use compio_postgres::{connect, NoTls};
use zeroship_core::config::{load_overlay_or_exit, resolve_observability};
use zeroship_core::oidc_verify::JwksCache;

use zeroship_auth::bootstrap;
use zeroship_auth::config::{validate_stash_key, AuthConfig};
use zeroship_auth::cron;
use zeroship_auth::error::AuthError;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::mailer::{
    Mailer, ResendConfig, ResendMailer, SmtpConfig, SmtpMailer, StdoutMailer,
};
use zeroship_auth::server;
use zeroship_auth::startup_validation::validate_hydra_admin_url;
use zeroship_auth::store;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut cfg = AuthConfig::parse();
    let file = load_overlay_or_exit(cfg.config_path.as_deref(), "auth");
    let (filter, format) =
        resolve_observability(&cfg.obs, &file.observability, "info,zeroship_auth=debug");
    zeroship_core::observability::init_tracing_with(&filter, format.as_deref());

    cfg.resolve_file_overlay(file.auth);
    tracing::info!(addr = %cfg.addr, "starting zeroship-auth");

    if let Err(message) = validate_stash_key(&cfg) {
        tracing::error!("{message}");
        std::process::exit(1);
    }
    if let Err(message) = validate_hydra_admin_url(&cfg) {
        tracing::error!("{message}");
        std::process::exit(1);
    }

    // Mailer config validation is cheap and should fail before any DB/Hydra
    // work. The constructed driver is reused below on normal startup.
    let mailer: Arc<dyn Mailer> = build_mailer(&cfg)?;
    tracing::info!(driver = %cfg.mailer, "mailer ready");

    if cfg.check_config {
        println!("check-config: addr = {}", cfg.addr);
        println!("check-config: hydra_admin_url = {}", cfg.hydra_admin_url());
        println!("check-config: hydra_public_url = {}", cfg.hydra_public_url());
        println!("check-config: log_filter = {filter}");
        println!(
            "check-config: log_format = {}",
            format.as_deref().unwrap_or("auto")
        );
        println!("check-config: insecure_dev = {}", cfg.insecure_dev);
        println!(
            "check-config: allow_remote_hydra_admin = {}",
            cfg.allow_remote_hydra_admin
        );
        println!("check-config: bootstrap = {}", cfg.bootstrap);
        println!("check-config: public_url = {}", cfg.public_url());
        println!("check-config: clients_config = {}", cfg.clients_config);
        println!("check-config: db_configured = {}", !cfg.db_url.is_empty());
        println!("check-config: mailer = {}", cfg.mailer);
        println!(
            "check-config: google_oauth_configured = {}",
            cfg.google_client_id.is_some()
        );
        println!(
            "check-config: github_oauth_configured = {}",
            cfg.github_client_id.is_some()
        );
        return Ok(());
    }

    ntex::rt::System::build()
        .name("zeroship-auth")
        .build(ntex::rt::DefaultRuntime)
        .block_on(async move {
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
    let admin = HydraAdmin::new(cfg.hydra_admin_url());
    bootstrap::run(&admin, &client, cfg.bootstrap, &cfg.clients_config).await?;
    tracing::info!("bootstrap complete");

    // 4. Build the Google JWKS cache. Only constructed when Google OAuth
    //    is wired up — the cache eagerly does nothing (lazy refresh on
    //    first verify), so we don't burn a startup roundtrip on Google.
    let google_jwks = if cfg.google_client_id.is_some() {
        Some(Arc::new(JwksCache::new(&cfg.google_jwks_url)))
    } else {
        None
    };

    // 5. Spawn in-process cron tasks. Detached on
    //    the compio runtime — survives across server worker restarts.
    //    Spawned BEFORE `server::run` so the loop is live as soon as
    //    the listener is bound. `Arc<Client>` is shared with the server
    //    so both drive I/O through the single compio-postgres connection.
    let cfg = Arc::new(cfg);
    let db = Arc::new(client);
    cron::spawn_all(admin.clone(), db.clone(), cfg.clone());
    tracing::info!("cron tasks spawned");

    // 6. Serve. `Arc`s keep the PG client + config alive across the
    //    server worker tasks AND the detached cron tasks; on shutdown
    //    the last `Arc` drop unblocks the background connection driver.
    server::run(cfg, admin, db, google_jwks, mailer).await?;
    Ok::<(), Box<dyn std::error::Error>>(())
        })
}

/// Translate `--mailer` + per-driver flags into a concrete
/// `Arc<dyn Mailer>`. Returns [`AuthError::Config`] when the selected
/// driver's required credentials aren't set, so the startup error
/// names exactly which env var is missing.
fn build_mailer(cfg: &AuthConfig) -> Result<Arc<dyn Mailer>, AuthError> {
    match cfg.mailer.as_str() {
        "stdout" => Ok(Arc::new(StdoutMailer)),
        "smtp" => {
            let host = cfg.smtp_host.clone().ok_or_else(|| {
                AuthError::Config(
                    "AUTH_SMTP_HOST is required when --mailer=smtp".into(),
                )
            })?;
            let driver = SmtpMailer::new(&SmtpConfig {
                host,
                port: cfg.smtp_port,
                username: cfg.smtp_username.clone(),
                password: cfg.smtp_password.clone(),
                use_starttls: cfg.smtp_starttls,
            })
            .map_err(|e| AuthError::Config(format!("smtp mailer: {e}")))?;
            Ok(Arc::new(driver))
        }
        "resend" => {
            let api_key = cfg.resend_api_key.clone().ok_or_else(|| {
                AuthError::Config(
                    "AUTH_RESEND_API_KEY is required when --mailer=resend".into(),
                )
            })?;
            Ok(Arc::new(ResendMailer::new(ResendConfig { api_key })))
        }
        other => Err(AuthError::Config(format!(
            "unknown mailer: {other:?}; use stdout|smtp|resend"
        ))),
    }
}
