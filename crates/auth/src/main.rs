//! zeroship-auth — the `OIDC` `IdP` login UI + identity flows + hydra admin client.
//!
//! Companion process: `oryd/hydra` (OIDC kernel). See docs/archive/auth-server.md.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::sync::Arc;

use clap::Parser;
use compio_postgres::{connect, NoTls};
use zeroship_core::config::{
    bootstrap_or_exit, is_secret_ref, obtain_secret, validate_master_key_material,
    validate_stash_key, CheckConfigReport, CheckFormat, CheckValue, SecretSection,
};
use zeroship_core::oidc_verify::JwksCache;

use zeroship_auth::bootstrap;
use zeroship_auth::config::AuthConfig;
use zeroship_auth::cron;
use zeroship_auth::error::AuthError;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::server;
use zeroship_mailer::{
    Mailer, RelayForwardMailer, ResendConfig, ResendMailer, SmtpConfig, SmtpMailer, StdoutMailer,
};
use zeroship_auth::startup_validation::validate_hydra_admin_url;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut cfg = AuthConfig::parse();
    let boot = bootstrap_or_exit(
        cfg.config_path.as_deref(),
        !cfg.no_config,
        &cfg.obs,
        "info,zeroship_auth=debug",
        "auth",
    );
    // Snapshot the [secrets] file overlay so each secret can consult its file-tier
    // reference. `file` stays a shared borrow of the overlay and `cfg.resolve` below
    // gets a clone of the `[auth]` section, so nothing is moved out of the overlay;
    // this `.clone()` of `.secrets` is a small defensive snapshot for clarity.
    let file_secrets = boot.overlay.config.secrets.clone();
    let file = &boot.overlay.config;
    if let Err(message) = cfg.try_resolve(file.auth.clone()) {
        tracing::error!("{message}");
        std::process::exit(1);
    }

    // Resolve secret-reference inputs (urn:zeroship:env|file|vault, arn:…) before
    // any guard or use. On real boot we resolve to the literal value (env/file
    // read); under --check-config we only validate the reference FORMAT and keep
    // the raw ref string so no side effects fire (mirrors bootstrap_or_exit). A
    // plain literal passes through byte-identically in both modes. Pure file-PATH
    // fields (none in auth today) are excluded; these are the exact fields the
    // redacting Debug impl prints as "<redacted>" minus the OAuth client *IDs*.
    resolve_auth_secrets(&mut cfg, &file_secrets);

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
    if !cfg.check_config || !is_secret_ref(&cfg.stash_signing_key) {
        if let Err(message) = validate_stash_key(&cfg.stash_signing_key, cfg.insecure_dev) {
            tracing::error!("{message}");
            std::process::exit(1);
        }
    }
    // TOTP at-rest key (ISS-11) — same is_secret_ref/check-config gate as the
    // stash key: skip the strength check for a raw `urn:`/`arn:` reference under
    // --check-config (the format was already validated in resolve_auth_secrets),
    // but always validate the resolved literal on real boot. Decodes (hex or
    // base64url) to ≥32 bytes, identical to the bundle/master key posture.
    if !cfg.check_config || !is_secret_ref(&cfg.totp_enc_key) {
        if let Err(message) = validate_master_key_material(
            "AUTH_TOTP_ENC_KEY / --totp-enc-key",
            &cfg.totp_enc_key,
            cfg.insecure_dev,
        ) {
            tracing::error!("{message}");
            std::process::exit(1);
        }
    }
    if let Err(message) = validate_hydra_admin_url(&cfg) {
        tracing::error!("{message}");
        std::process::exit(1);
    }

    // --check-config is a read-only DRY-RUN: resolve + report the config and
    // exit BEFORE any runtime-only validation (mailer/SMTP construction), exactly
    // like control / gateway / worker. Building the mailers enforces the
    // `AUTH_SMTP_HOST` / `AUTH_RELAY_SMTP_HOST` requirements, which are real-boot
    // concerns — they must NOT gate a config dry-run (ISS-62). The report below
    // references only `cfg.*` (e.g. `cfg.mailer`, a plain string), never the
    // constructed drivers, so it stands alone ahead of mailer construction.
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
        report.field(
            "auth_provider",
            CheckValue::Plain(cfg.auth_provider().as_str().to_string()),
        );
        report.field(
            "supabase_url",
            CheckValue::Plain(cfg.supabase_url().unwrap_or("").to_string()),
        );
        report.field(
            "supabase_anon_key_configured",
            CheckValue::Secret(cfg.supabase_anon_key().is_some()),
        );
        report.field(
            "gotrue_email_hook_secret_configured",
            CheckValue::Secret(cfg.gotrue_email_hook_secret.is_some()),
        );
        report.field(
            "control_url",
            CheckValue::Plain(cfg.control_url().to_string()),
        );
        report.field(
            "control_key_configured",
            CheckValue::Secret(!cfg.control_key.is_empty()),
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
        report.field("op_issuer_url", CheckValue::Plain(cfg.op_issuer_url()));
        report.field(
            "auth_signing_key_file_configured",
            CheckValue::Secret(cfg.auth_signing_key_file.is_some()),
        );
        report.field(
            "auth_pairwise_salt_file_configured",
            CheckValue::Secret(cfg.auth_pairwise_salt_file.is_some()),
        );
        report.field(
            "auth_broker_secret_file_configured",
            CheckValue::Secret(cfg.auth_broker_secret_file.is_some()),
        );
        report.field(
            "auth_broker_secret_previous_file_configured",
            CheckValue::Secret(cfg.auth_broker_secret_previous_file.is_some()),
        );
        report.field(
            "refresh_hash_key_file_configured",
            CheckValue::Secret(cfg.refresh_hash_key_file.is_some()),
        );
        report.field(
            "refresh_idem_key_file_configured",
            CheckValue::Secret(cfg.refresh_idem_key_file.is_some()),
        );
        report.field(
            "refresh_pool_size",
            CheckValue::Plain(cfg.refresh_pool_size.to_string()),
        );
        report.field(
            "frame_ancestor_origins",
            CheckValue::Plain(cfg.frame_ancestor_origins.join(",")),
        );
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

    // Real boot only (past the --check-config dry-run early-return above). Mailer
    // config validation is cheap and should fail before any DB/Hydra work, with a
    // named env var, so a misconfigured SMTP block (transactional or relay-forward)
    // fails fast. The constructed drivers are threaded into `server::run` below.
    let mailer: Arc<dyn Mailer> = build_mailer(&cfg)?;
    tracing::info!(driver = %cfg.mailer, "mailer ready");

    // The SECOND, dedicated relay-forward mailer (sub-spec §5.2a). Forced to
    // SMTP/stdout — Resend can't pin envelope-from (§3.2).
    let relay_forward_mailer: RelayForwardMailer = build_relay_forward_mailer(&cfg)?;
    tracing::info!(driver = %cfg.relay_forward_mailer, "relay-forward mailer ready");

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

    // Schema is owned by zeroship-migrate (db/migrations, applied by the
    // compose `migrate` service / `ops/db-migrate.sh`) out of band before this
    // service boots — not here.

    let auth_signing_key_file = cfg.auth_signing_key_file.as_deref().ok_or_else(|| {
        AuthError::Config(
            "AUTH_SIGNING_KEY_FILE / --auth-signing-key-file is required".into(),
        )
    })?;
    let auth_pairwise_salt_file = cfg.auth_pairwise_salt_file.as_deref().ok_or_else(|| {
        AuthError::Config(
            "AUTH_PAIRWISE_SALT_FILE / --auth-pairwise-salt-file is required".into(),
        )
    })?;
    let auth_broker_secret_file = cfg.auth_broker_secret_file.as_deref().ok_or_else(|| {
        AuthError::Config(
            "AUTH_BROKER_SECRET_FILE / --auth-broker-secret-file is required".into(),
        )
    })?;
    cfg.refresh_hash_key_file.as_deref().ok_or_else(|| {
        AuthError::Config(
            "REFRESH_HASH_KEY_FILE / --refresh-hash-key-file is required".into(),
        )
    })?;
    cfg.refresh_idem_key_file.as_deref().ok_or_else(|| {
        AuthError::Config(
            "REFRESH_IDEM_KEY_FILE / --refresh-idem-key-file is required".into(),
        )
    })?;
    let op_issuer = zeroship_auth::oidc::Issuer::from_files(
        auth_signing_key_file,
        auth_pairwise_salt_file,
        cfg.op_issuer_url(),
    )?
    .with_broker_secrets(zeroship_auth::oidc::BrokerSecrets::from_files(
        auth_broker_secret_file,
        cfg.auth_broker_secret_previous_file.as_deref(),
    )?);
    op_issuer.publish_active_key(&client).await?;
    tracing::info!(
        kid = %op_issuer.kid(),
        issuer = %op_issuer.issuer(),
        "platform OP signing key published"
    );
    let op_issuer = Arc::new(op_issuer);

    // 2. Bootstrap: keys + client reconciliation.
    let admin = HydraAdmin::new(cfg.hydra_admin_url());
    bootstrap::run(&admin, &client, cfg.bootstrap, &cfg.clients_config).await?;
    tracing::info!("bootstrap complete");

    // 3. Build the Google JWKS cache. Only constructed when Google OAuth
    //    is wired up — the cache eagerly does nothing (lazy refresh on
    //    first verify), so we don't burn a startup roundtrip on Google.
    let google_jwks = if cfg.google_client_id.is_some() {
        Some(Arc::new(JwksCache::new(&cfg.google_jwks_url)))
    } else {
        None
    };

    let refresh_pool = zeroship_auth::oidc::refresh::RefreshSessionPool::new(
        cfg.db_url.clone(),
        cfg.refresh_pool_size,
    );
    tracing::info!(
        pool_size = refresh_pool.pool_size(),
        "refresh dedicated session pool configured"
    );

    // 4. Spawn in-process cron tasks. Detached on
    //    the compio runtime — survives across server worker restarts.
    //    Spawned BEFORE `server::run` so the loop is live as soon as
    //    the listener is bound. `Arc<Client>` is shared for autocommit
    //    cron work; refresh-family sweeps check out bounded dedicated
    //    sessions for their advisory-locked transactions.
    let cfg = Arc::new(cfg);
    let db = Arc::new(client);
    cron::spawn_all(admin.clone(), db.clone(), cfg.clone(), refresh_pool.clone());
    tracing::info!("cron tasks spawned");

    // 5. Serve. `Arc`s keep the PG client + config alive across the
    //    server worker tasks AND the detached cron tasks; on shutdown
    //    the last `Arc` drop unblocks the background connection driver.
    //    OP refresh-token rotations/revokes/root issuance do not run
    //    multi-statement transactions on this shared handle; they check
    //    out bounded dedicated compio-postgres sessions from refresh_pool.
    server::run(
        cfg,
        admin,
        db,
        google_jwks,
        mailer,
        relay_forward_mailer,
        op_issuer,
        refresh_pool,
    )
    .await?;
    Ok::<(), Box<dyn std::error::Error>>(())
        })
}

/// Resolve every secret-reference-bearing input in place.
///
/// Each field here is one the redacting [`AuthConfig`] `Debug` impl prints as
/// `<redacted>` (DSN, stash signing key, OAuth client *secrets*, SMTP/Resend
/// keys, webhook password) — never the cleartext OAuth client *IDs*. Each secret
/// is obtained via [`obtain_secret`] with precedence CLI/env > `[secrets]` file
/// (reference-only) > default. On real boot the value is resolved to its literal
/// (env/file read); under `--check-config` only the reference FORMAT is validated
/// and the raw ref is kept so no side effects fire. A plain literal CLI/env value
/// is byte-identical in both modes.
///
/// `Option<String>` secrets fall back to the `[secrets]` file tier only when the
/// CLI/env value is absent or empty; when neither tier supplies a value the field
/// stays `None` (its provider arm is disabled).
fn resolve_auth_secrets(cfg: &mut AuthConfig, file_secrets: &SecretSection) {
    let check = cfg.check_config;

    // Required-string secrets (always present on the CLI struct).
    cfg.db_url = obtain_secret(
        "AUTH_DB_URL / --db-url",
        &cfg.db_url,
        file_secrets.auth_db_url.as_deref(),
        check,
    );
    cfg.stash_signing_key = obtain_secret(
        "AUTH_STASH_SIGNING_KEY / --stash-signing-key",
        &cfg.stash_signing_key,
        file_secrets.stash_signing_key.as_deref(),
        check,
    );
    cfg.control_key = obtain_secret(
        "CONTROL_KEY / --control-key",
        &cfg.control_key,
        file_secrets.control_key.as_deref(),
        check,
    );
    cfg.totp_enc_key = obtain_secret(
        "AUTH_TOTP_ENC_KEY / --totp-enc-key",
        &cfg.totp_enc_key,
        file_secrets.totp_enc_key.as_deref(),
        check,
    );

    // Optional secrets — obtain only when the CLI/env or file tier supplies a
    // value; an all-empty result leaves the provider arm disabled (`None`).
    cfg.google_client_secret = resolve_optional(
        check,
        "AUTH_GOOGLE_CLIENT_SECRET / --google-client-secret",
        cfg.google_client_secret.as_deref(),
        file_secrets.google_client_secret.as_deref(),
    );
    cfg.github_client_secret = resolve_optional(
        check,
        "AUTH_GITHUB_CLIENT_SECRET / --github-client-secret",
        cfg.github_client_secret.as_deref(),
        file_secrets.github_client_secret.as_deref(),
    );
    cfg.smtp_password = resolve_optional(
        check,
        "AUTH_SMTP_PASSWORD / --smtp-password",
        cfg.smtp_password.as_deref(),
        file_secrets.smtp_password.as_deref(),
    );
    cfg.resend_api_key = resolve_optional(
        check,
        "AUTH_RESEND_API_KEY / --resend-api-key",
        cfg.resend_api_key.as_deref(),
        file_secrets.resend_api_key.as_deref(),
    );
    cfg.gotrue_email_hook_secret = resolve_optional(
        check,
        "AUTH_GOTRUE_EMAIL_HOOK_SECRET / --gotrue-email-hook-secret",
        cfg.gotrue_email_hook_secret.as_deref(),
        None,
    );
    cfg.postmark_webhook_password = resolve_optional(
        check,
        "AUTH_POSTMARK_WEBHOOK_PASSWORD / --postmark-webhook-password",
        cfg.postmark_webhook_password.as_deref(),
        file_secrets.postmark_webhook_password.as_deref(),
    );
    // Relay secrets (Slice 5). No dedicated [secrets] file slot yet, so the
    // file tier is `None` — CLI/env resolution + reference-format validation
    // still apply, exactly like the optional SMTP/Resend secrets.
    cfg.relay_inbound_password = resolve_optional(
        check,
        "AUTH_RELAY_INBOUND_PASSWORD / --relay-inbound-password",
        cfg.relay_inbound_password.as_deref(),
        None,
    );
    cfg.relay_smtp_password = resolve_optional(
        check,
        "AUTH_RELAY_SMTP_PASSWORD / --relay-smtp-password",
        cfg.relay_smtp_password.as_deref(),
        None,
    );
}

/// Obtain one optional secret across the CLI/env and `[secrets]` file tiers.
///
/// Preserves the original optional behaviour byte-for-byte when no file tier is
/// involved: an absent field stays `None`, and a present-but-empty CLI/env value
/// stays `Some("")` (untouched). The `[secrets]` file tier is consulted only when
/// the CLI/env value is empty AND a file reference exists; in that case the file
/// reference is resolved via [`obtain_secret`]. A non-empty CLI/env value always
/// wins over a file reference (see [`obtain_secret`]).
fn resolve_optional(
    check_config: bool,
    label: &str,
    cli: Option<&str>,
    file: Option<&str>,
) -> Option<String> {
    match cli {
        // Present, non-empty ⇒ CLI/env wins (file is ignored by obtain_secret).
        Some(raw) if !raw.is_empty() => Some(obtain_secret(label, raw, file, check_config)),
        // Empty/absent CLI with a file reference ⇒ obtain from the file tier.
        _ if file.is_some() => Some(obtain_secret(label, cli.unwrap_or(""), file, check_config)),
        // No file tier ⇒ leave the field exactly as it was (None stays None,
        // Some("") stays Some("")), matching pre-[secrets] behaviour.
        other => other.map(str::to_string),
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
                tls: cfg.smtp_tls,
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

/// Build the SECOND, dedicated relay-forward mailer (sub-spec §5.2a). Keyed on
/// `--relay-forward-mailer` (default `smtp`) and reads the SEPARATE
/// `AUTH_RELAY_SMTP_*` config block so the relay sending identity/credentials
/// are independent of the transactional `AUTH_SMTP_*`. Rejects `resend` for
/// this role: the relay forward path must pin the SMTP envelope-from to the
/// relay bounce mailbox, which Resend's HTTP API cannot do (§3.2).
fn build_relay_forward_mailer(cfg: &AuthConfig) -> Result<RelayForwardMailer, AuthError> {
    match cfg.relay_forward_mailer.as_str() {
        "smtp" => {
            let host = cfg.relay_smtp_host.clone().ok_or_else(|| {
                AuthError::Config(
                    "AUTH_RELAY_SMTP_HOST is required when --relay-forward-mailer=smtp".into(),
                )
            })?;
            let driver = SmtpMailer::new(&SmtpConfig {
                host,
                port: cfg.relay_smtp_port,
                username: cfg.relay_smtp_username.clone(),
                password: cfg.relay_smtp_password.clone(),
                tls: cfg.relay_smtp_tls,
            })
            .map_err(|e| AuthError::Config(format!("relay smtp mailer: {e}")))?;
            Ok(RelayForwardMailer(Arc::new(driver)))
        }
        // Dev: forward → terminal (the e2e prefers smtp so it can assert the
        // rendered envelope, but stdout is valid for eyeballing the surgery).
        "stdout" => Ok(RelayForwardMailer(Arc::new(StdoutMailer))),
        other => Err(AuthError::Config(format!(
            "AUTH_RELAY_FORWARD_MAILER={other:?} unsupported; relay forward needs \
             envelope-from control — use smtp (or stdout in dev). resend cannot pin \
             envelope-from (sub-spec §3.2)."
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
    use super::{is_loopback_addr, resolve_optional};
    use zeroship_core::config::{
        is_secret_ref, obtain_secret, resolve_secret, validate_secret_ref, validate_stash_key,
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

    // [secrets] file tier (d): a required secret falls back to its [secrets]
    // file reference when the CLI/env value is empty. This is the exact wiring
    // resolve_auth_secrets uses for db_url / stash_signing_key. Asserted through
    // the public obtain_secret so the test does not depend on env-var visibility
    // of the binary's own AUTH_* names.
    #[test]
    fn secrets_file_ref_used_when_cli_empty() {
        // Unique per-process var name; edition 2021: set_var is safe (no unsafe
        // block; workspace denies unsafe_code).
        let var = format!("ZEROSHIP_AUTH_TEST_FILE_TIER_{}", std::process::id());
        std::env::set_var(&var, "from-secrets-file");
        let reference = format!("urn:zeroship:env:{var}");

        // Empty CLI/env => the [secrets] file reference is resolved.
        let out = obtain_secret("AUTH_DB_URL / --db-url", "", Some(&reference), false);
        assert_eq!(out, "from-secrets-file");
        std::env::remove_var(&var);
    }

    // [secrets] file tier precedence: a non-empty CLI/env value WINS over the
    // [secrets] file reference (CLI/env > file). The file ref is never resolved.
    #[test]
    fn cli_value_wins_over_secrets_file_ref() {
        // A literal CLI value beats the file reference; the env ref is ignored
        // (never read), so an unset env var must not cause a failure.
        let reference = "urn:zeroship:env:ZEROSHIP_AUTH_UNSET_PROVES_CLI_WINS";
        let out = obtain_secret(
            "AUTH_STASH_SIGNING_KEY / --stash-signing-key",
            "literal-cli-secret",
            Some(reference),
            false,
        );
        assert_eq!(out, "literal-cli-secret");
    }

    // Optional secret across the file tier: an empty CLI value with a [secrets]
    // file reference resolves the file tier; an empty CLI with NO file leaves the
    // field exactly as it was (None stays None, Some("") stays Some("")) —
    // matching pre-[secrets] optional behaviour byte-for-byte.
    #[test]
    fn resolve_optional_uses_file_tier_and_preserves_empty() {
        let var = format!("ZEROSHIP_AUTH_TEST_OPT_TIER_{}", std::process::id());
        std::env::set_var(&var, "opt-from-file");
        let reference = format!("urn:zeroship:env:{var}");

        // Empty/absent CLI + file ref => obtained from the file tier.
        assert_eq!(
            resolve_optional(false, "AUTH_RESEND_API_KEY", None, Some(&reference)),
            Some("opt-from-file".to_string())
        );
        // Non-empty CLI wins over the file ref.
        assert_eq!(
            resolve_optional(false, "AUTH_RESEND_API_KEY", Some("cli-wins"), Some(&reference)),
            Some("cli-wins".to_string())
        );
        // No CLI, no file => stays disabled (None), unchanged from before.
        assert_eq!(resolve_optional(false, "AUTH_RESEND_API_KEY", None, None), None);
        // Present-but-empty CLI, no file => stays Some("") (untouched), matching
        // the original resolve_optional empty-passthrough behaviour.
        assert_eq!(
            resolve_optional(false, "AUTH_RESEND_API_KEY", Some(""), None),
            Some(String::new())
        );
        std::env::remove_var(&var);
    }
}
