//! zeroship-auth — the `OIDC` `IdP` login UI + identity flows + hydra admin client.
//!
//! Companion process: `oryd/hydra` (OIDC kernel). See docs/proposals/auth-server.md.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::sync::Arc;

use clap::Parser;
use compio_postgres::{connect, NoTls};
use zeroship_core::config::{
    bootstrap_or_exit, validate_stash_key, CheckConfigReport, CheckFormat, CheckValue,
};
use zeroship_core::oidc_verify::JwksCache;

use zeroship_auth::bootstrap;
use zeroship_auth::config::AuthConfig;
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
    let boot = bootstrap_or_exit(
        cfg.config_path.as_deref(),
        !cfg.no_config,
        &cfg.obs,
        "info,zeroship_auth=debug",
        "auth",
    );
    let file = &boot.overlay.config;
    cfg.resolve(file.auth.clone());
    tracing::info!(addr = %cfg.addr, "starting zeroship-auth");

    // Loopback is the default; a non-loopback bind under --dev-insecure is
    // intentional (compose dev on a private network) but must shout (S1).
    if cfg.insecure_dev && !is_loopback_addr(&cfg.addr) {
        tracing::warn!(
            addr = %cfg.addr,
            "auth: binding a non-loopback address with --dev-insecure; admin/cookie guards are relaxed — NEVER in production"
        );
    }

    if let Err(message) = validate_stash_key(&cfg.stash_signing_key, cfg.insecure_dev) {
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
        let mut report = CheckConfigReport::new();
        report.field("addr", CheckValue::Plain(cfg.addr.clone()));
        report.field(
            "config_source",
            CheckValue::Plain(boot.overlay.source.to_string()),
        );
        report.field(
            "hydra_admin_url",
            CheckValue::Plain(cfg.hydra_admin_url().to_string()),
        );
        report.field(
            "hydra_public_url",
            CheckValue::Plain(cfg.hydra_public_url().to_string()),
        );
        report.field("log_filter", CheckValue::Plain(boot.log_filter.clone()));
        report.field(
            "log_format",
            CheckValue::Plain(
                boot.log_format
                    .map_or_else(|| "auto".to_string(), |f| f.to_string()),
            ),
        );
        report.field("insecure_dev", CheckValue::Flag(cfg.insecure_dev));
        report.field(
            "allow_remote_hydra_admin",
            CheckValue::Flag(cfg.allow_remote_hydra_admin),
        );
        report.field("bootstrap", CheckValue::Flag(cfg.bootstrap));
        report.field("public_url", CheckValue::Plain(cfg.public_url()));
        report.field("clients_config", CheckValue::Plain(cfg.clients_config.clone()));
        report.field("db_configured", CheckValue::Secret(!cfg.db_url.is_empty()));
        report.field("mailer", CheckValue::Plain(cfg.mailer.clone()));
        report.field(
            "google_oauth_configured",
            CheckValue::Flag(cfg.google_client_id.is_some()),
        );
        report.field(
            "github_oauth_configured",
            CheckValue::Flag(cfg.github_client_id.is_some()),
        );
        let fmt = if cfg.check_config_format == "json" {
            CheckFormat::Json
        } else {
            CheckFormat::Text
        };
        report.emit(fmt);
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

/// Return true when the bind address (`host:port`) is a literal loopback host
/// (`localhost` or a loopback IP). Mirrors the literal-only policy in
/// `zeroship_core::config::is_loopback_url`; no DNS resolution. Used only to
/// decide whether to shout about a non-loopback `--dev-insecure` bind.
fn is_loopback_addr(addr: &str) -> bool {
    let host = match addr.rsplit_once(':') {
        // Strip the IPv6 brackets from `[::1]:9092` style addresses.
        Some((host, _)) => host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host),
        None => addr,
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::is_loopback_addr;

    #[test]
    fn loopback_addr_recognises_literal_loopback_only() {
        assert!(is_loopback_addr("127.0.0.1:9092"));
        assert!(is_loopback_addr("localhost:9092"));
        assert!(is_loopback_addr("[::1]:9092"));
        // non-loopback binds (the ones the dev-insecure warn fires on)
        assert!(!is_loopback_addr("0.0.0.0:9092"));
        assert!(!is_loopback_addr("10.0.0.5:9092"));
    }
}
