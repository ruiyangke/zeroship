//! zeroship-auth — the `OIDC` `IdP` login UI + identity flows + hydra admin client.
//!
//! Companion process: `oryd/hydra` (OIDC kernel). See docs/proposals/auth-server.md.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::sync::Arc;

use clap::Parser;
use compio_postgres::{connect, NoTls};
use zeroship_core::config::{
    bootstrap_or_exit, resolve_secret_or_exit, validate_secret_ref_or_exit, validate_stash_key,
    CheckConfigReport, CheckFormat, CheckValue,
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

    // Resolve secret-reference inputs (urn:zeroship:env|file|vault, arn:…) before
    // any guard or use. On real boot we resolve to the literal value (env/file
    // read); under --check-config we only validate the reference FORMAT and keep
    // the raw ref string so no side effects fire (mirrors bootstrap_or_exit). A
    // plain literal passes through byte-identically in both modes. Pure file-PATH
    // fields (none in auth today) are excluded; these are the exact fields the
    // redacting Debug impl prints as "<redacted>" minus the OAuth client *IDs*.
    resolve_auth_secrets(&mut cfg);

    tracing::info!(addr = %cfg.addr, "starting zeroship-auth");

    // Loopback is the default; a non-loopback bind under --dev-insecure is
    // intentional (compose dev on a private network) but must shout (S1).
    if cfg.insecure_dev && !is_loopback_addr(&cfg.addr) {
        tracing::warn!(
            addr = %cfg.addr,
            "auth: binding a non-loopback address with --dev-insecure; admin/cookie guards are relaxed — NEVER in production"
        );
    }

    // Strength guard runs on the RESOLVED value at real boot (cfg.stash_signing_key
    // is already the literal there). During --check-config a secret REFERENCE is
    // still the raw `urn:`/`arn:` string — running a strength check on it would
    // wrongly fail, so skip it for a reference in that mode only (format was
    // already validated by resolve_auth_secrets).
    if !cfg.check_config || !zeroship_core::config::is_secret_ref(&cfg.stash_signing_key) {
        if let Err(message) = validate_stash_key(&cfg.stash_signing_key, cfg.insecure_dev) {
            tracing::error!("{message}");
            std::process::exit(1);
        }
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

/// Resolve every secret-reference-bearing input in place.
///
/// Each field here is one the redacting [`AuthConfig`] `Debug` impl prints as
/// `<redacted>` (DSN, stash signing key, OAuth client *secrets*, SMTP/Resend
/// keys, webhook password) — never the cleartext OAuth client *IDs*. On real
/// boot the value is resolved to its literal (env/file read via
/// [`resolve_secret_or_exit`]); under `--check-config` only the reference FORMAT
/// is validated ([`validate_secret_ref_or_exit`]) and the raw ref is kept so no
/// side effects fire. A plain literal is byte-identical in both modes.
///
/// `Option<String>` secrets are resolved only when present and non-empty; an
/// absent OAuth/mailer credential stays `None` (its provider arm is disabled).
fn resolve_auth_secrets(cfg: &mut AuthConfig) {
    let check = cfg.check_config;

    // Required-string secrets (always present on the CLI struct).
    resolve_required(check, "AUTH_DB_URL / --db-url", &mut cfg.db_url);
    resolve_required(
        check,
        "AUTH_STASH_SIGNING_KEY / --stash-signing-key",
        &mut cfg.stash_signing_key,
    );

    // Optional secrets — resolve in place only when set & non-empty.
    resolve_optional(
        check,
        "AUTH_GOOGLE_CLIENT_SECRET / --google-client-secret",
        &mut cfg.google_client_secret,
    );
    resolve_optional(
        check,
        "AUTH_GITHUB_CLIENT_SECRET / --github-client-secret",
        &mut cfg.github_client_secret,
    );
    resolve_optional(
        check,
        "AUTH_SMTP_PASSWORD / --smtp-password",
        &mut cfg.smtp_password,
    );
    resolve_optional(
        check,
        "AUTH_RESEND_API_KEY / --resend-api-key",
        &mut cfg.resend_api_key,
    );
    resolve_optional(
        check,
        "AUTH_POSTMARK_WEBHOOK_PASSWORD / --postmark-webhook-password",
        &mut cfg.postmark_webhook_password,
    );
}

/// Resolve one required-string secret in place (see [`resolve_auth_secrets`]).
fn resolve_required(check_config: bool, label: &str, field: &mut String) {
    if check_config {
        validate_secret_ref_or_exit(label, field);
    } else {
        *field = resolve_secret_or_exit(label, field);
    }
}

/// Resolve one optional secret in place. Absent/empty values are left untouched
/// (the corresponding provider arm stays disabled); see [`resolve_auth_secrets`].
fn resolve_optional(check_config: bool, label: &str, field: &mut Option<String>) {
    let Some(raw) = field.as_deref() else { return };
    if raw.is_empty() {
        return;
    }
    if check_config {
        validate_secret_ref_or_exit(label, raw);
    } else {
        *field = Some(resolve_secret_or_exit(label, raw));
    }
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
    use zeroship_core::config::{
        is_secret_ref, resolve_secret, validate_secret_ref, validate_stash_key,
    };

    #[test]
    fn loopback_addr_recognises_literal_loopback_only() {
        assert!(is_loopback_addr("127.0.0.1:9092"));
        assert!(is_loopback_addr("localhost:9092"));
        assert!(is_loopback_addr("[::1]:9092"));
        // non-loopback binds (the ones the dev-insecure warn fires on)
        assert!(!is_loopback_addr("0.0.0.0:9092"));
        assert!(!is_loopback_addr("10.0.0.5:9092"));
    }

    // (a) A literal secret passes through the resolver byte-identically — the
    // wiring (resolve_required/resolve_optional → resolve_secret_or_exit) must
    // not mangle a plain value. Asserts via the public resolver the *_or_exit
    // helpers delegate to.
    #[test]
    fn literal_secret_resolves_to_itself() {
        let literal = "0123456789abcdef0123456789abcdef"; // strong, ≥32 bytes
        assert_eq!(
            resolve_secret(literal).expect("literal resolves"),
            literal,
            "a literal secret must pass through unchanged"
        );
        // A DSN literal (the db_url shape) is also a literal, not a ref.
        let dsn = "postgres://u:p@h/db";
        assert_eq!(resolve_secret(dsn).expect("dsn resolves"), dsn);
        assert!(!is_secret_ref(dsn), "a DSN literal is not a reference");
    }

    // (b) The is_secret_ref-gated guard logic. The guard at boot is:
    //     if !check_config || !is_secret_ref(&stash_signing_key) { validate_stash_key(..) }
    // A ref'd stash key under --check-config must SKIP the strength guard
    // (the local is the raw `urn:…` ref, which would wrongly fail length/format
    // checks); a literal must STILL be guarded even under --check-config.
    fn stash_guard_runs(check_config: bool, stash_key: &str) -> bool {
        !check_config || !is_secret_ref(stash_key)
    }

    #[test]
    fn refd_stash_key_skips_strength_guard_in_check_config() {
        // A short env ref: valid reference FORMAT, but only 18 bytes — so the
        // strength guard (length ≥ 32) would WRONGLY reject it if it ran on the
        // raw ref string. That is exactly why the guard is skipped for a
        // reference under --check-config.
        let reference = "urn:zeroship:env:K";
        assert!(is_secret_ref(reference), "the test fixture must be a reference");
        assert!(
            validate_secret_ref(reference).is_ok(),
            "the ref FORMAT itself is valid (only the strength guard would reject it)"
        );
        assert!(
            validate_stash_key(reference, false).is_err(),
            "raw ref string must fail the strength guard if (wrongly) checked"
        );

        // check-config + reference ⇒ guard is skipped.
        assert!(
            !stash_guard_runs(true, reference),
            "a ref'd stash key under --check-config must skip the strength guard"
        );
        // check-config + literal ⇒ guard still runs (a weak literal must fail).
        assert!(
            stash_guard_runs(true, "weak"),
            "a literal stash key must still be guarded under --check-config"
        );
        // Real boot (not check-config) ⇒ guard ALWAYS runs, even on a reference
        // (the local is the resolved literal there, so this is correct).
        assert!(
            stash_guard_runs(false, reference),
            "outside --check-config the guard always runs (value is resolved)"
        );
        assert!(stash_guard_runs(false, "literal"));
    }

    // (c) A malformed reference is rejected by the format validator the
    // check-config path uses (validate_secret_ref_or_exit delegates to this).
    #[test]
    fn malformed_secret_ref_is_rejected() {
        // Reserved prefix but unrecognized scheme ⇒ malformed.
        assert!(validate_secret_ref("urn:zeroship:nope:x").is_err());
        // Recognized scheme with an empty body ⇒ malformed.
        assert!(validate_secret_ref("urn:zeroship:env:").is_err());
        // A bare reserved prefix ⇒ malformed.
        assert!(validate_secret_ref("urn:bogus:x").is_err());
        // A well-formed env reference is NOT malformed (format-only check).
        assert!(validate_secret_ref("urn:zeroship:env:MY_VAR").is_ok());
    }
}
