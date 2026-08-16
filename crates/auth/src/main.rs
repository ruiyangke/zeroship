//! zeroship-auth — the `OIDC` `IdP` login UI + identity flows.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::sync::Arc;

use clap::Parser;
use compio_postgres::{connect, NoTls};
use zeroship_core::config::{
    bootstrap_or_exit, require_nonempty, validate_master_key_material, validate_secret_material,
    validate_stash_key, CheckConfigReport, CheckValue,
};
use zeroship_core::oidc_verify::JwksCache;

/// Operator-facing spelling of auth's stash signing key.
///
/// The gateway reads a DIFFERENT variable behind the same validator
/// (`gateway.stash_signing_key`), which is why the validator takes the name as
/// a parameter rather than spelling one itself. Derived from the declaration at
/// `crates/auth/src/config.rs` (`#[config(name = "auth.stash_signing_key")]`).
const STASH_SIGNING_KEY_LABEL: &str = "ZEROSHIP_AUTH_STASH_SIGNING_KEY / --stash-signing-key-file";
const PLATFORM_MINT_KEY_LABEL: &str = "ZEROSHIP_AUTH_PLATFORM_MINT_KEY / --platform-mint-key-file";

use zeroship_auth::config::{AuthCli, AuthConfig, AuthSettings};
use zeroship_auth::cron;
use zeroship_auth::error::AuthError;
use zeroship_auth::server;
use zeroship_mailer::{
    Mailer, RelayForwardMailer, ResendConfig, ResendMailer, SmtpConfig, SmtpMailer, StdoutMailer,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = AuthCli::parse();
    let (settings, boot) = bootstrap_or_exit::<AuthSettings>(
        cli.settings.clone(),
        zeroship_auth::config::DEFAULT_LOG_FILTER,
        "auth",
    );
    let check_config = *settings.check_config.get();
    // `bootstrap_or_exit` already applied CLI > env > overlay > default to every
    // value, secrets included: the generated resolver read each secret's `-file`
    // flag, canonical environment name and canonical overlay path itself, and
    // under --check-config it established each one's SOURCE without opening a
    // file. This is auth's own resolve-time work: the fail-closed frame-ancestor
    // filter and the Supabase completeness guard.
    let cfg = match AuthConfig::from_resolved(settings) {
        Ok(cfg) => cfg,
        Err(message) => {
            tracing::error!("{message}");
            std::process::exit(1);
        }
    };

    tracing::info!(addr = %cfg.settings.addr.get(), "starting zeroship-auth");

    if let Err(message) = validate_startup_secrets(&cfg) {
        tracing::error!("{message}");
        std::process::exit(1);
    }
    // --check-config is a read-only DRY-RUN: resolve + report the config and
    // exit BEFORE any runtime-only validation (mailer/SMTP construction), exactly
    // like control / gateway / worker. Building the mailers enforces the
    // `ZEROSHIP_AUTH_SMTP_HOST` / `ZEROSHIP_AUTH_RELAY_SMTP_HOST` requirements, which are real-boot
    // concerns — they must NOT gate a config dry-run (ISS-62). The report below
    // references only `cfg.*` (e.g. `cfg.mailer`, a plain string), never the
    // constructed drivers, so it stands alone ahead of mailer construction.
    if check_config {
        let mut report = CheckConfigReport::new();
        report.field("addr", CheckValue::Plain(cfg.settings.addr.get().clone()));
        report.field(
            "config_source",
            CheckValue::Plain(boot.overlay.source.to_string()),
        );
        report.field(
            "auth_provider",
            CheckValue::Plain(cfg.auth_provider().as_str().to_string()),
        );
        report.field(
            "supabase_url",
            CheckValue::Plain(cfg.supabase_url().unwrap_or("").to_string()),
        );
        // The anon key is OPERATIONAL by classification (Supabase publishes it
        // to every browser), so it is reported as a plain value like any other
        // operational setting rather than as a presence bit that would imply a
        // protection it does not have.
        report.field(
            "supabase_anon_key",
            CheckValue::Plain(cfg.supabase_anon_key().unwrap_or("").to_string()),
        );
        report.field(
            "gotrue_email_hook_secret_configured",
            CheckValue::Secret(cfg.settings.gotrue_email_hook_secret.is_configured()),
        );
        report.field(
            "control_url",
            CheckValue::Plain(cfg.control_url().to_string()),
        );
        report.field(
            "platform_mint_key_configured",
            CheckValue::Secret(cfg.settings.platform_mint_key.is_configured()),
        );
        report.field("log_filter", CheckValue::Plain(boot.log_filter.clone()));
        report.field(
            "log_format",
            CheckValue::Plain(
                boot.log_format.to_string(),
            ),
        );
        report.field("public_url", CheckValue::Plain(cfg.public_url()));
        report.field("op_issuer_url", CheckValue::Plain(cfg.op_issuer_url()));
        // Key FILES. These are operator-chosen PATHS, not secrets, so a report
        // could legitimately print them - but presence is all an operator needs
        // to answer "is this deployment wired up", and a path can name a mount
        // an operator would rather not see echoed into a log.
        report.field(
            "signing_key_file_configured",
            CheckValue::Secret(cfg.signing_key_file().is_some()),
        );
        report.field(
            "pairwise_salt_file_configured",
            CheckValue::Secret(cfg.pairwise_salt_file().is_some()),
        );
        report.field(
            "broker_secret_file_configured",
            CheckValue::Secret(cfg.broker_secret_file().is_some()),
        );
        report.field(
            "broker_secret_previous_file_configured",
            CheckValue::Secret(cfg.broker_secret_previous_file().is_some()),
        );
        report.field(
            "refresh_hash_key_file_configured",
            CheckValue::Secret(cfg.refresh_hash_key_file().is_some()),
        );
        report.field(
            "refresh_idem_key_file_configured",
            CheckValue::Secret(cfg.refresh_idem_key_file().is_some()),
        );
        report.field(
            "refresh_pool_size",
            CheckValue::Plain(cfg.refresh_pool_size().to_string()),
        );
        report.field(
            "frame_ancestor_origins",
            CheckValue::Plain(cfg.frame_ancestor_origins().join(",")),
        );
        report.field(
            "db_configured",
            CheckValue::Secret(cfg.settings.database_url.is_configured()),
        );
        report.field("mailer", CheckValue::Plain(cfg.settings.mailer.get().clone()));
        report.field(
            "google_oauth_configured",
            CheckValue::Flag(cfg.google_client_id().is_some()),
        );
        report.field(
            "github_oauth_configured",
            CheckValue::Flag(cfg.github_client_id().is_some()),
        );
        report.emit(*cfg.settings.check_config_format.get());
        return Ok(());
    }

    // Real boot only (past the --check-config dry-run early-return above). Mailer
    // config validation is cheap and should fail before any DB work, with a
    // named env var, so a misconfigured SMTP block (transactional or relay-forward)
    // fails fast. The constructed drivers are threaded into `server::run` below.
    let mailer: Arc<dyn Mailer> = build_mailer(&cfg)?;
    tracing::info!(driver = %cfg.settings.mailer.get(), "mailer ready");

    // The SECOND, dedicated relay-forward mailer (sub-spec §5.2a). Forced to
    // SMTP/stdout — Resend can't pin envelope-from (§3.2).
    let relay_forward_mailer: RelayForwardMailer = build_relay_forward_mailer(&cfg)?;
    tracing::info!(driver = %cfg.settings.relay_forward_mailer.get(), "relay-forward mailer ready");

    ntex::rt::System::build()
        .name("zeroship-auth")
        .build(ntex::rt::DefaultRuntime)
        .block_on(async move {
    // OAuth provider credentials are optional. We log a warning per disabled
    // provider so it's obvious during boot which federation arms aren't wired
    // up. Actual route gating happens in U2.2 (Google) + U3.2 (GitHub).
    if cfg.google_client_id().is_none() {
        tracing::warn!(
            "Google OAuth disabled - set ZEROSHIP_AUTH_GOOGLE_CLIENT_ID + ZEROSHIP_AUTH_GOOGLE_CLIENT_SECRET to enable"
        );
    }
    if cfg.github_client_id().is_none() {
        tracing::warn!(
            "GitHub OAuth disabled - set ZEROSHIP_AUTH_GITHUB_CLIENT_ID + ZEROSHIP_AUTH_GITHUB_CLIENT_SECRET to enable"
        );
    }

    // 1. Open PG.
    let (client, connection) = connect(cfg.settings.database_url.expose_str(), NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            tracing::error!(error = %e, "auth/pg connection error");
        }
    })
    .detach();

    // Schema is owned by zeroship-migrate (db/migrations-ts, applied by the
    // compose `migrate` service / `deploy/ops/db-migrate.sh`) out of band before this
    // service boots — not here.

    let signing_key_file = cfg.signing_key_file().ok_or_else(|| {
        AuthError::Config(
            "ZEROSHIP_AUTH_SIGNING_KEY_FILE / --signing-key-file is required".into(),
        )
    })?;
    let pairwise_salt_file = cfg.pairwise_salt_file().ok_or_else(|| {
        AuthError::Config(
            "ZEROSHIP_AUTH_PAIRWISE_SALT_FILE / --pairwise-salt-file is required".into(),
        )
    })?;
    let broker_secret_file = cfg.broker_secret_file().ok_or_else(|| {
        AuthError::Config(
            "ZEROSHIP_AUTH_BROKER_SECRET_FILE / --broker-secret-file is required".into(),
        )
    })?;
    cfg.refresh_hash_key_file().ok_or_else(|| {
        AuthError::Config(
            "ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE / --refresh-hash-key-file is required".into(),
        )
    })?;
    cfg.refresh_idem_key_file().ok_or_else(|| {
        AuthError::Config(
            "ZEROSHIP_AUTH_REFRESH_IDEM_KEY_FILE / --refresh-idem-key-file is required".into(),
        )
    })?;
    let op_issuer = zeroship_auth::oidc::Issuer::from_files(
        signing_key_file,
        pairwise_salt_file,
        cfg.op_issuer_url(),
    )?
    .with_broker_secrets(zeroship_auth::oidc::BrokerSecrets::from_files(
        broker_secret_file,
        cfg.broker_secret_previous_file(),
    )?);
    op_issuer.publish_active_key(&client).await?;
    tracing::info!(
        kid = %op_issuer.kid(),
        issuer = %op_issuer.issuer(),
        "platform OP signing key published"
    );
    let op_issuer = Arc::new(op_issuer);

    // 2. Build the Google JWKS cache. Only constructed when Google OAuth
    //    is wired up — the cache eagerly does nothing (lazy refresh on
    //    first verify), so we don't burn a startup roundtrip on Google.
    let google_jwks = if cfg.google_client_id().is_some() {
        Some(Arc::new(JwksCache::new(cfg.settings.google_jwks_url.get())))
    } else {
        None
    };

    let refresh_pool = zeroship_auth::oidc::refresh::RefreshSessionPool::new(
        cfg.settings.database_url.expose_str().to_owned(),
        cfg.refresh_pool_size(),
    );
    tracing::info!(
        pool_size = refresh_pool.pool_size(),
        "refresh dedicated session pool configured"
    );

    // 3. Spawn in-process cron tasks. Detached on
    //    the compio runtime — survives across server worker restarts.
    //    Spawned BEFORE `server::run` so the loop is live as soon as
    //    the listener is bound. `Arc<Client>` is shared for autocommit
    //    cron work; refresh-family sweeps check out bounded dedicated
    //    sessions for their advisory-locked transactions.
    let cfg = Arc::new(cfg);
    let db = Arc::new(client);
    cron::spawn_all(db.clone(), cfg.clone(), refresh_pool.clone());
    tracing::info!("cron tasks spawned");

    // 4. Serve. `Arc`s keep the PG client + config alive across the
    //    server worker tasks AND the detached cron tasks; on shutdown
    //    the last `Arc` drop unblocks the background connection driver.
    //    OP refresh-token rotations/revokes/root issuance do not run
    //    multi-statement transactions on this shared handle; they check
    //    out bounded dedicated compio-postgres sessions from refresh_pool.
    server::run(
        cfg,
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

/// Run every startup strength guard over the RESOLVED secrets.
///
/// A function rather than two inline blocks in `main` so the guards can be
/// driven by a test; `main` only adds "log it and exit 1".
///
/// `validate_secret_material` is the one bridge from a resolved
/// [`zeroship_core::config::Secret`] to a `&str` validator. It runs the
/// validator on the material whenever there IS material - every real boot, and
/// a `--check-config` run whose secret is an in-memory literal - and it runs the
/// validator on `""` when nothing supplied the secret, which is how each
/// validator still produces its own "X is required" message rather than a
/// generic one. The single case it skips is configured-but-deliberately-unread,
/// which only a dry run over a file source can reach: there is nothing to judge
/// there, and judging the reference TEXT instead is what the deleted
/// `is_secret_ref` dance did.
///
/// # Errors
///
/// Propagates the first validator's message unchanged.
fn validate_startup_secrets(cfg: &AuthConfig) -> Result<(), String> {
    validate_secret_material(&cfg.settings.platform_mint_key, |value| {
        require_nonempty(PLATFORM_MINT_KEY_LABEL, value.trim())
    })?;
    validate_secret_material(&cfg.settings.stash_signing_key, |value| {
        validate_stash_key(STASH_SIGNING_KEY_LABEL, value)
    })?;
    // TOTP at-rest key (ISS-11). Decodes (hex or base64url) to >=32 bytes,
    // identical to the bundle/master key posture.
    validate_secret_material(&cfg.settings.totp_enc_key, |material| {
        validate_master_key_material("ZEROSHIP_AUTH_TOTP_ENC_KEY / --totp-enc-key-file", material)
    })
}

/// Translate `--mailer` + per-driver flags into a concrete
/// `Arc<dyn Mailer>`. Returns [`AuthError::Config`] when the selected
/// driver's required credentials aren't set, so the startup error
/// names exactly which env var is missing.
fn build_mailer(cfg: &AuthConfig) -> Result<Arc<dyn Mailer>, AuthError> {
    match cfg.settings.mailer.get().as_str() {
        "stdout" => Ok(Arc::new(StdoutMailer)),
        "smtp" => {
            let host = cfg.smtp_host().map(str::to_owned).ok_or_else(|| {
                AuthError::Config(
                    "ZEROSHIP_AUTH_SMTP_HOST is required when --mailer=smtp".into(),
                )
            })?;
            let driver = SmtpMailer::new(&SmtpConfig {
                host,
                port: *cfg.settings.smtp_port.get(),
                username: cfg.smtp_username().map(str::to_owned),
                password: cfg.settings.smtp_password.expose_secret().cloned(),
                tls: *cfg.settings.smtp_tls.get(),
            })
            .map_err(|e| AuthError::Config(format!("smtp mailer: {e}")))?;
            Ok(Arc::new(driver))
        }
        "resend" => {
            let api_key = cfg
                .settings
                .resend_api_key
                .expose_secret()
                .cloned()
                .ok_or_else(|| {
                    AuthError::Config(
                        "ZEROSHIP_AUTH_RESEND_API_KEY is required when --mailer=resend".into(),
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
    match cfg.settings.relay_forward_mailer.get().as_str() {
        "smtp" => {
            let host = cfg.relay_smtp_host().map(str::to_owned).ok_or_else(|| {
                AuthError::Config(
                    "ZEROSHIP_AUTH_RELAY_SMTP_HOST is required when --relay-forward-mailer=smtp".into(),
                )
            })?;
            let driver = SmtpMailer::new(&SmtpConfig {
                host,
                port: *cfg.settings.relay_smtp_port.get(),
                username: cfg.relay_smtp_username().map(str::to_owned),
                password: cfg.settings.relay_smtp_password.expose_secret().cloned(),
                tls: *cfg.settings.relay_smtp_tls.get(),
            })
            .map_err(|e| AuthError::Config(format!("relay smtp mailer: {e}")))?;
            Ok(RelayForwardMailer(Arc::new(driver)))
        }
        // Dev: forward → terminal (the e2e prefers smtp so it can assert the
        // rendered envelope, but stdout is valid for eyeballing the surgery).
        "stdout" => Ok(RelayForwardMailer(Arc::new(StdoutMailer))),
        other => Err(AuthError::Config(format!(
            "ZEROSHIP_AUTH_RELAY_FORWARD_MAILER={other:?} unsupported; relay forward needs \
             envelope-from control — use smtp (or stdout in dev). resend cannot pin \
             envelope-from (sub-spec §3.2)."
        ))),
    }
}


#[cfg(test)]
mod tests {
    use zeroship_auth::config::AuthConfig;
    use zeroship_core::config::{Secret, SourceKind};

    use super::validate_startup_secrets;

    /// A config with both guarded secrets strong, so each test below can move
    /// exactly one of them and know which guard spoke.
    fn healthy() -> AuthConfig {
        let mut cfg = AuthConfig::parse_from(["zeroship-auth"]);
        cfg.settings.platform_mint_key = supplied("platform-mint-key");
        cfg.settings.stash_signing_key = supplied("0123456789abcdef0123456789abcdef");
        cfg.settings.totp_enc_key = supplied(&"00".repeat(32));
        cfg
    }

    /// An in-memory literal, which is what the env and TOML tiers resolve to.
    fn supplied(material: &str) -> Secret<String> {
        Secret::supplied(SourceKind::Env, Some(material.to_owned()))
    }

    #[test]
    fn an_absent_platform_mint_key_fails_the_startup_guard() {
        let mut cfg = healthy();
        cfg.settings.platform_mint_key = Secret::absent();

        let message =
            validate_startup_secrets(&cfg).expect_err("an unset platform mint key is rejected");
        assert!(
            message.contains("ZEROSHIP_AUTH_PLATFORM_MINT_KEY"),
            "{message}"
        );
        assert!(message.contains("required"), "{message}");

        cfg.settings.platform_mint_key = supplied("   ");
        validate_startup_secrets(&cfg).expect_err("a whitespace-only mint key is rejected");
    }

    // The stash key signs the federation stash cookie; a forgeable one bypasses
    // the OAuth state/PKCE check. The guard used to read a plain `String` and is
    // now bridged through `validate_secret_material`, so this pins that the
    // bridge did not turn "absent" or "too short" into a clean boot.
    #[test]
    fn a_weak_or_absent_stash_key_still_fails_the_startup_guard() {
        let mut cfg = healthy();

        // Nothing supplied it: the validator runs on "" and says so itself.
        cfg.settings.stash_signing_key = Secret::absent();
        let message = validate_startup_secrets(&cfg).expect_err("an unset stash key is rejected");
        // The FULL canonical name, not the bare `STASH_SIGNING_KEY` this
        // asserted before: that spelling is a SUBSTRING of the live name, so it
        // kept passing while naming a variable auth does not read, and no
        // rename could ever break it.
        assert!(
            message.contains("ZEROSHIP_AUTH_STASH_SIGNING_KEY"),
            "{message}"
        );
        assert!(message.contains("required"), "{message}");

        // Supplied but too short: a DIFFERENT message, so the guard is reading
        // the material rather than merely noticing a missing source.
        cfg.settings.stash_signing_key = supplied("short");
        let message = validate_startup_secrets(&cfg).expect_err("a short stash key is rejected");
        assert!(message.contains("too short"), "{message}");

        // A known public development value is rejected even at full length.
        cfg.settings.stash_signing_key = supplied("dev-only-stash-signing-key-not-for-production-use!!");
        let message = validate_startup_secrets(&cfg).expect_err("a public dev key is rejected");
        assert!(message.contains("known public"), "{message}");

        // The one-variable control: same field, same shape, strong material -
        // and the whole guard passes. Without it, a guard that rejected
        // everything would also satisfy the three assertions above.
        cfg.settings.stash_signing_key = supplied("0123456789abcdef0123456789abcdef");
        validate_startup_secrets(&cfg).expect("a strong stash key boots");

        // Does NOT cover: that `main` actually calls this function, nor that it
        // exits 1 rather than continuing. `main` is not callable from a test;
        // the process-level behaviour is what tests/config_check_e2e.sh drives.
    }

    // The TOTP at-rest key. Same bridge, a different validator shape (decoded
    // length, not raw length), which is the part a shared helper could have
    // flattened by accident.
    #[test]
    fn a_weak_or_absent_totp_encryption_key_still_fails_the_startup_guard() {
        let mut cfg = healthy();

        cfg.settings.totp_enc_key = Secret::absent();
        let message = validate_startup_secrets(&cfg).expect_err("an unset totp key is rejected");
        assert!(message.contains("ZEROSHIP_AUTH_TOTP_ENC_KEY"), "{message}");

        // Decodable but too small: 3 bytes of base64url.
        cfg.settings.totp_enc_key = supplied("YWJj");
        let message = validate_startup_secrets(&cfg).expect_err("a short totp key is rejected");
        assert!(message.contains("bytes"), "{message}");

        // Undecodable material is rejected too - the validator is not a length
        // check on the encoded text.
        cfg.settings.totp_enc_key = supplied("not!base64!");
        validate_startup_secrets(&cfg).expect_err("an undecodable totp key is rejected");

        // The one-variable control: 32 decoded bytes passes.
        cfg.settings.totp_enc_key = supplied(&"00".repeat(32));
        validate_startup_secrets(&cfg).expect("a strong totp key boots");

        // Does NOT cover: that the key actually decrypts a stored TOTP secret.
        // That is `totp_store_test.rs`.
    }

    // The `--check-config` case the deleted `is_secret_ref` dance existed for: a
    // secret whose source is a FILE is established but deliberately not read
    // during a dry run, so there is no material to judge and the guard must not
    // invent a failure. The paired case is the absent one above, which differs
    // only in having no source at all and DOES fail.
    #[test]
    fn a_configured_but_unread_secret_does_not_fail_the_guard() {
        let mut cfg = healthy();
        cfg.settings.stash_signing_key = Secret::supplied(SourceKind::CliFile, None);
        cfg.settings.totp_enc_key = Secret::supplied(SourceKind::CliFile, None);
        validate_startup_secrets(&cfg).expect("an unread secret is not judged");
        assert!(cfg.settings.stash_signing_key.is_configured());

        // Does NOT cover: that a `--check-config` run reaches this state, i.e.
        // that the generated resolver really declines to open the file. That is
        // asserted on `resolve_secret_sources` in core.
    }
}
