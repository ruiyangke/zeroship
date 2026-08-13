//! zeroship-control — control plane binary. Thin wrapper over
//! `zeroship_control` (the library crate).

use std::sync::Arc;

use clap::Parser;
use ntex::web;
use zeroship_core::auth_provider::{
    AuthProvider, DualIssuerProvider, LegacyAuthProvider, PlatformConfig, PlatformProvider,
    SupabaseConfig, SupabaseProvider,
};
use zeroship_core::config::{
    bootstrap_or_exit, validate_master_key_material, AuthProviderKind, CheckConfigReport,
    CheckValue,
};
use zeroship_bundle::{
    build_blob_store, build_workflow_blob_store, BlobStore, StoreUrl, WorkflowBlobStore,
};
use zeroship_control::config::{ControlSettings, ControlSettingsSources};
use zeroship_control::{
    admin_handlers, api, device_handlers, env_handlers,
    internal, oauth_grants_handlers, oauth_handlers, plan_catalog, stripe_handlers, token_handlers,
    workflow_instance_api,
    AppState, EnvStore, Quota, RateLimiter, Registry, StripeStore,
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Single-binary dev fallback DSN. NOT a clap `default_value` — see the `--db`
/// field doc: a non-empty clap default occupies `obtain_secret`'s CLI tier and
/// silently shadows the `[secrets] database_url` reference. It is applied after
/// both higher tiers come up empty.
const DEFAULT_DB_URL: &str = "postgres://localhost/zeroship";

/// zeroship control-plane startup configuration.
///
/// Only the credential-bearing fields remain here; every operational value is
/// generated in `zeroship_control::config`.
#[derive(Parser)]
#[command(name = "zeroship-control")]
struct ControlCli {
    /// `PostgreSQL` DSN for control-plane data.
    ///
    /// The default is EMPTY, and it has to be: `obtain_secret`'s contract is
    /// "`cli` is the clap-merged CLI/env value (`""` when unset)", and it takes
    /// the CLI branch on ANY non-empty string. A compiled-in `default_value`
    /// here is indistinguishable from an operator-supplied `--db`, so it wins
    /// over the `[secrets] database_url` reference and the file tier can never
    /// be reached. The compiled fallback is applied AFTER `obtain_secret`,
    /// which is the only place it can sit without shadowing the file tier
    /// (precedence: CLI/env > `[secrets]` reference > default).
    #[arg(long = "db", env = "DATABASE_URL", default_value = "", hide_env_values = true)]
    db: String,

    /// Admin/control API shared secret.
    #[arg(long = "control-key", env = "CONTROL_KEY", default_value = "", hide_env_values = true)]
    control_key: String,

    /// Master key used for control-plane encrypted env/secrets.
    #[arg(long = "master-key", env = "MASTER_KEY", default_value = "", hide_env_values = true)]
    master_key: String,

    /// Shared secret for worker admin endpoints.
    #[arg(long = "worker-key", env = "WORKER_KEY", default_value = "", hide_env_values = true)]
    worker_key: String,

    /// PEM/PKCS#8 signing key file for PAT issuance.
    #[arg(long = "signing-key-file", env = "SIGNING_KEY_FILE", default_value = "")]
    signing_key_file: String,

    /// Stripe webhook signing secret.
    #[arg(
        long = "stripe-webhook-secret",
        env = "STRIPE_WEBHOOK_SECRET",
        default_value = "",
        hide_env_values = true
    )]
    stripe_webhook_secret: String,

    /// Stripe secret API key (`sk_...`) for outbound calls (the billing
    /// reconciler + `billing/setup`). Operations that need Stripe reject an
    /// empty value; webhook verification uses its separate signing secret.
    #[arg(
        long = "stripe-secret-key",
        env = "STRIPE_SECRET_KEY",
        default_value = "",
        hide_env_values = true
    )]
    stripe_secret_key: String,

    /// SMTP password (optional).
    #[arg(long = "smtp-password", env = "CONTROL_SMTP_PASSWORD")]
    smtp_password: Option<String>,

    /// Resend API key - required when the mailer is `resend`.
    #[arg(long = "resend-api-key", env = "CONTROL_RESEND_API_KEY")]
    resend_api_key: Option<String>,

    /// Comma-separated previous master keys accepted during key rotation.
    #[arg(
        long = "legacy-master-keys",
        env = "LEGACY_MASTER_KEYS",
        default_value = "",
        hide_env_values = true
    )]
    legacy_master_keys: String,

    /// Supabase anon API key used for GoTrue browser/session API calls.
    #[arg(
        long = "supabase-anon-key",
        env = "SUPABASE_ANON_KEY",
        default_value = "",
        hide_env_values = true
    )]
    supabase_anon_key: String,

    /// Supabase service-role key. Optional in this read-side slice; P-S2 uses it
    /// for admin lookups while provisioning identity links.
    #[arg(
        long = "supabase-service-role-key",
        env = "SUPABASE_SERVICE_ROLE_KEY",
        default_value = "",
        hide_env_values = true
    )]
    supabase_service_role_key: String,

    /// HS256 GoTrue JWT secret. Mutually exclusive with the Supabase JWKS URL.
    #[arg(
        long = "supabase-jwt-secret",
        env = "SUPABASE_JWT_SECRET",
        default_value = "",
        hide_env_values = true
    )]
    supabase_jwt_secret: String,

    /// Dedicated PERMANENT pairwise-salt secret (value). The seed for every
    /// app's `pws_` per-app identity anchor - independent of the rotatable
    /// stash key. MUST be identical to the gateway's value and MUST NOT be
    /// rotated without a per-app `pws_` migration. Prefer
    /// `--pairwise-salt-file` in production.
    #[arg(
        long = "pairwise-salt",
        env = "PAIRWISE_SALT",
        default_value = "",
        hide_env_values = true
    )]
    pairwise_salt: String,

    /// Path to a file holding the dedicated pairwise-salt secret. Takes
    /// precedence over `--pairwise-salt` / `PAIRWISE_SALT` when set.
    #[arg(long = "pairwise-salt-file", env = "PAIRWISE_SALT_FILE", default_value = "")]
    pairwise_salt_file: String,

    /// Every operational value, generated from one declaration in
    /// `zeroship_control::config`.
    #[command(flatten)]
    settings: ControlSettingsSources,
}

// S2: hand-written `Debug` that redacts every raw-secret field. The derive is
// intentionally dropped so a stray `{:?}` (e.g. in a clap parse error or a test
// `.unwrap_err()`) can never echo a DSN, master key, control key, worker key,
// Stripe secrets, console OIDC secrets, or legacy master keys.
impl std::fmt::Debug for ControlCli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlCli")
            .field("db", &"<redacted>")
            .field("control_key", &"<redacted>")
            .field("master_key", &"<redacted>")
            .field("worker_key", &"<redacted>")
            .field("signing_key_file", &self.signing_key_file)
            .field("stripe_webhook_secret", &"<redacted>")
            .field("stripe_secret_key", &"<redacted>")
            .field("smtp_password", &"<redacted>")
            .field("resend_api_key", &"<redacted>")
            .field("legacy_master_keys", &"<redacted>")
            .field("supabase_anon_key", &"<redacted>")
            .field("supabase_service_role_key", &"<redacted>")
            .field("supabase_jwt_secret", &"<redacted>")
            .field("pairwise_salt", &"<redacted>")
            .field("pairwise_salt_file", &self.pairwise_salt_file)
            .field("settings", &self.settings)
            .finish()
    }
}

/// Resolve the dedicated pairwise-salt secret (mirrors the gateway). Precedence:
///   1. `--pairwise-salt-file` / `PAIRWISE_SALT_FILE` (read verbatim, trim a
///      trailing newline) — keeps the value out of the process table,
///   2. else `obtain_secret` on `--pairwise-salt` / `PAIRWISE_SALT` (+ overlay).
///
/// A configured-but-unreadable file is fatal — a misconfigured prod salt must
/// fail loudly, not silently fall through to the dev default.
/// Build the billing-notification mailer from `--mailer` (default stdout), mirroring
/// auth's `build_mailer`. Returns a `String` error (consumed at the boot call site,
/// which logs + exits) when a selected driver's required creds are missing.
fn build_billing_mailer(
    cli: &ControlCli,
    settings: &ControlSettings,
) -> Result<Arc<dyn zeroship_mailer::Mailer>, String> {
    use zeroship_mailer::{
        ResendConfig, ResendMailer, SmtpConfig, SmtpMailer, SmtpTls, StdoutMailer,
    };
    match settings.mailer.get().as_str() {
        "stdout" => Ok(Arc::new(StdoutMailer)),
        "smtp" => {
            let host = settings.smtp_host.get().clone();
            if host.is_empty() {
                return Err(
                    "ZEROSHIP_CONTROL_SMTP_HOST is required when --mailer=smtp".to_string()
                );
            }
            let username = settings.smtp_username.get().clone();
            let driver = SmtpMailer::new(&SmtpConfig {
                host,
                port: *settings.smtp_port.get(),
                username: (!username.is_empty()).then_some(username),
                password: cli.smtp_password.clone(),
                tls: SmtpTls::Starttls,
            })
            .map_err(|e| format!("smtp mailer: {e}"))?;
            Ok(Arc::new(driver))
        }
        "resend" => {
            let api_key = cli.resend_api_key.clone().ok_or_else(|| {
                "CONTROL_RESEND_API_KEY is required when --mailer=resend".to_string()
            })?;
            Ok(Arc::new(ResendMailer::new(ResendConfig { api_key })))
        }
        other => Err(format!("unknown mailer: {other:?}; use stdout|smtp|resend")),
    }
}

/// How the native-mode boot guard names the platform issuer input.
///
/// A `const` rather than a literal at the guard so the diagnostic test below
/// can read the exact string an operator sees. Every other auth-provider
/// diagnostic is already reachable through the function that produces it.
const PLATFORM_ISSUER_INPUT: &str = "--auth-platform-issuer / ZEROSHIP_AUTH_PLATFORM_ISSUER";

fn build_control_auth_provider(
    auth_provider: AuthProviderKind,
    supabase: ControlSupabaseAuthConfig,
    platform: ControlPlatformAuthConfig,
) -> Result<Arc<AuthProvider>, String> {
    match auth_provider {
        AuthProviderKind::Native => {
            let platform_config = platform_config_required(platform)?;
            Ok(Arc::new(AuthProvider::Platform(PlatformProvider::new(
                platform_config,
            ))))
        }
        AuthProviderKind::Supabase => {
            if supabase.anon_key.trim().is_empty() {
                return Err("SUPABASE_ANON_KEY is required for ZEROSHIP_AUTH_PROVIDER=supabase"
                    .to_string());
            }
            let config = SupabaseConfig::new(
                supabase.url,
                supabase.anon_key,
                empty_string_as_none(supabase.service_role_key),
                empty_string_as_none(supabase.jwt_secret),
                empty_string_as_none(supabase.jwks_url),
                supabase.jwt_issuer,
            )
            .map_err(|err| format!("supabase auth provider config: {err}"))?;
            let legacy = LegacyAuthProvider::Supabase(SupabaseProvider::new(config));
            let Some(platform_config) = platform_config(platform)? else {
                return Ok(Arc::new(AuthProvider::from(legacy)));
            };
            Ok(Arc::new(AuthProvider::DualIssuer(DualIssuerProvider::new(
                PlatformProvider::new(platform_config),
                legacy,
            ))))
        }
    }
}

#[derive(Clone, Copy)]
struct ControlSupabaseAuthConfig<'a> {
    url: &'a str,
    anon_key: &'a str,
    service_role_key: &'a str,
    jwt_secret: &'a str,
    jwks_url: &'a str,
    jwt_issuer: &'a str,
}

#[derive(Clone, Copy)]
struct ControlPlatformAuthConfig<'a> {
    issuer: &'a str,
    jwks_url: &'a str,
}

fn platform_config(
    platform: ControlPlatformAuthConfig<'_>,
) -> Result<Option<PlatformConfig>, String> {
    if platform.issuer.trim().is_empty() {
        if !platform.jwks_url.trim().is_empty() {
            return Err(
                "ZEROSHIP_AUTH_PLATFORM_ISSUER is required when \
                 ZEROSHIP_AUTH_PLATFORM_JWKS_URL is set"
                    .to_string(),
            );
        }
        return Ok(None);
    }
    PlatformConfig::new(
        platform.issuer,
        empty_string_as_none(platform.jwks_url),
    )
    .map(Some)
    .map_err(|err| format!("platform auth provider config: {err}"))
}

fn platform_config_required(
    platform: ControlPlatformAuthConfig<'_>,
) -> Result<PlatformConfig, String> {
    platform_config(platform)?.ok_or_else(|| {
        "ZEROSHIP_AUTH_PLATFORM_ISSUER is required for ZEROSHIP_AUTH_PROVIDER=native".to_string()
    })
}

fn empty_string_as_none(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn resolve_pairwise_salt(
    salt_file: &str,
    salt_value: &str,
    file_ref: Option<&str>,
    check_config: bool,
) -> String {
    if !salt_file.is_empty() {
        return std::fs::read_to_string(salt_file)
            .map(|s| s.trim_end_matches(['\n', '\r']).to_string())
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, path = %salt_file, "control: cannot read --pairwise-salt-file");
                std::process::exit(1);
            });
    }
    zeroship_core::config::obtain_secret(
        "PAIRWISE_SALT / --pairwise-salt",
        salt_value,
        file_ref,
        check_config,
    )
}

fn main() -> std::io::Result<()> {
    // Control uses cyper for provider/admin calls (Supabase identity bridge,
    // Stripe reconciliation). Install the workspace's selected rustls provider
    // before any outbound client can be constructed.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = ControlCli::parse();
    // The generated sources are taken out of the parser first; everything left
    // on `cli` is a secret awaiting the Secret<T> conversion.
    let sources = cli.settings.clone();
    let (settings, boot) = bootstrap_or_exit::<ControlSettings>(
        sources,
        zeroship_control::config::DEFAULT_LOG_FILTER,
        "control",
    );
    // Billing notifier mailer (PR-6): built from the resolved mailer setting.
    // An unknown driver or missing creds refuses to boot.
    let billing_mailer: Arc<dyn zeroship_mailer::Mailer> =
        match build_billing_mailer(&cli, &settings) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "control: refusing to start - billing mailer not available"
                );
                std::process::exit(1);
            }
        };
    let mailer_kind = settings.mailer.get().clone();
    let check_config = *settings.check_config.get();
    let file = &boot.overlay.config;
    // `[secrets]` file-overlay tier for the secret-reference resolver. Cloned once
    // up front so individual `obtain_secret` calls can borrow the per-field refs
    // (CLI/env > [secrets] file ref > default) without re-borrowing `boot`.
    let file_secrets = boot.overlay.config.secrets.clone();
    // `[metering]` file-overlay tier for the billing stream (CLI/env > file).
    let file_metering = boot.overlay.config.metering.clone();
    let filter = &boot.log_filter;

    let trust_proxy = *settings.trust_proxy.get();
    let origin_scheme = *settings.origin_scheme.get();

    // No hand-rolled value parse here any more: the declaration resolves to
    // `AuthProviderKind`, so clap and the overlay reject an unknown spelling
    // before this function is reached.
    let auth_provider_kind = *settings.auth_provider.get();
    let supabase_url = settings.supabase_url.get().clone();
    let supabase_anon_key = cli.supabase_anon_key.clone();
    let supabase_service_role_key = cli.supabase_service_role_key.clone();
    let supabase_jwt_secret = cli.supabase_jwt_secret.clone();
    let supabase_jwks_url = settings.supabase_jwks_url.get().clone();
    let supabase_jwt_issuer = settings.supabase_jwt_issuer.get().clone();
    // The overlay tier these two used to reach by hand is now the generated
    // resolver's: `auth.platform_issuer` and `auth.platform_jwks_url` ARE the
    // canonical paths, so the values below already carry CLI / env / overlay
    // precedence and `resolve_overlay_string` has nothing left to add.
    let auth_platform_issuer = settings.auth_platform_issuer.get().clone();
    let auth_platform_jwks_url = settings.auth_platform_jwks_url.get().clone();
    let trusted_oauth_clients = zeroship_control::resolve_trusted_oauth_clients(&file.auth);
    tracing::info!(
        trusted_oauth_clients = trusted_oauth_clients.len(),
        "control: trusted OAuth client set resolved"
    );

    let port = *settings.port.get();
    let bind_host = settings.bind.get().clone();
    // Secret-bearing inputs (the fields `ControlCli::Debug` redacts) are resolved
    // through the shared secret-reference resolver. On the real boot path a
    // `urn:zeroship:{env,file,...}` / `arn:aws:secretsmanager:...` reference is
    // dereferenced to its value; under `--check-config` only the reference FORMAT
    // is validated (no env/file/network side effects) and the raw ref string is
    // kept for the read-only report. A literal secret passes through byte-for-byte
    // in both modes. The DSN field (`--db`) carries a password, so it goes
    // through the same path. Pure file-PATH fields (`--signing-key-file`,
    // `--builder-client-secret-file`) name a file to read and are NOT resolved here.
    let db_url = zeroship_core::config::obtain_secret(
        "DATABASE_URL / --db",
        &cli.db,
        file_secrets.database_url.as_deref(),
        check_config,
    );
    // The compiled fallback, applied only once BOTH higher tiers came up empty.
    // `--check-config` is a read-only report of what was CONFIGURED, so it keeps
    // the empty string rather than substituting a default nobody supplied. That
    // arm is currently unobservable — the report does not print the DSN, and the
    // two binaries' `--check-config` output was diffed byte-for-byte (timestamps
    // aside) across this change. It is here so the guard is already right if the
    // DSN is ever added to the report, NOT because it fixes anything today.
    let db_url = if db_url.is_empty() && !check_config {
        DEFAULT_DB_URL.to_string()
    } else {
        db_url
    };
    let blob_store_root = settings.blob_store.get().clone();
    // `s3://…` → remote S3 store (control writes deploys through the SAME
    // store gateway/worker read), bare path → local disk (dev default).
    // Validated now so a bad `s3://` URL fails fast.
    let store_url = match StoreUrl::parse(&blob_store_root) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("control: invalid --blob-store: {e}");
            std::process::exit(2);
        }
    };
    let blob_store_is_remote = store_url.is_remote();
    let control_key = zeroship_core::config::obtain_secret(
        "CONTROL_KEY / --control-key",
        &cli.control_key,
        file_secrets.control_key.as_deref(),
        check_config,
    );
    let master_key = zeroship_core::config::obtain_secret(
        "MASTER_KEY / --master-key",
        &cli.master_key,
        file_secrets.master_key.as_deref(),
        check_config,
    );
    let workers_str = settings.worker_urls.get().clone();
    let gateway_url = settings.gateway_url.get().trim_end_matches('/').to_string();
    let worker_key = zeroship_core::config::obtain_secret(
        "WORKER_KEY / --worker-key",
        &cli.worker_key,
        file_secrets.worker_key.as_deref(),
        check_config,
    );
    let signing_key_file = cli.signing_key_file;
    let stripe_webhook_secret = zeroship_core::config::obtain_secret(
        "STRIPE_WEBHOOK_SECRET / --stripe-webhook-secret",
        &cli.stripe_webhook_secret,
        file_secrets.stripe_webhook_secret.as_deref(),
        check_config,
    );
    let stripe_secret_key = zeroship_core::config::obtain_secret(
        "STRIPE_SECRET_KEY / --stripe-secret-key",
        &cli.stripe_secret_key,
        file_secrets.stripe_secret_key.as_deref(),
        check_config,
    );
    let stripe_base_url = settings.stripe_base_url.get().clone();
    // Comma-separated list of previous master keys, tried as fallbacks on decrypt
    // failure during a rotation grace period. A CLI/env value is a comma-list where
    // EACH entry may be a literal or its own secret reference (resolved per entry); an
    // absent CLI/env value falls back to a single `[secrets]` file reference that
    // dereferences to a comma-list string (entries are then literal).
    let legacy_label = "LEGACY_MASTER_KEYS / --legacy-master-keys";
    let legacy_keys: Vec<String> = if cli.legacy_master_keys.is_empty() {
        let csv = zeroship_core::config::obtain_secret(
            legacy_label,
            "",
            file_secrets.legacy_master_keys.as_deref(),
            check_config,
        );
        // In --check-config the file reference is only format-validated (csv is then
        // the raw ref, which must not be split); split only a resolved/literal value.
        if check_config && zeroship_core::config::is_secret_ref(&csv) {
            Vec::new()
        } else {
            csv.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        }
    } else {
        cli.legacy_master_keys
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|entry| {
                if check_config {
                    zeroship_core::config::validate_secret_ref_or_exit(legacy_label, entry);
                    entry.to_owned()
                } else {
                    zeroship_core::config::resolve_secret_or_exit(legacy_label, entry)
                }
            })
            .collect()
    };
    let deploy_tmp_dir_str = settings.deploy_tmp_dir.get().clone();
    // Dedicated pairwise-salt secret (auth-sdk §6.2). MUST match the gateway's
    // value — both derive the per-app `pws_`. `--pairwise-salt-file` wins over
    // the inline value / overlay reference.
    let pairwise_salt = resolve_pairwise_salt(
        &cli.pairwise_salt_file,
        &cli.pairwise_salt,
        file_secrets.pairwise_salt.as_deref(),
        check_config,
    );
    let expected_oauth_audience = settings.oauth_audience.get().clone();
    let app_base_domain = settings.app_base_domain.get().clone();
    let spend_recompute_interval = *settings.spend_recompute_interval.get();
    let audit_retention_months = *settings.audit_retention_months.get();
    let audit_retention_check_secs = *settings.audit_retention_check_secs.get();

    // Pure path resolution only — the writability PROBE (create_dir_all + probe
    // file) is deferred to the real startup path (M1) so `--check-config`
    // performs NO filesystem mutation but can still report the resolved path.
    let deploy_tmp_dir: std::path::PathBuf = if deploy_tmp_dir_str.is_empty() {
        std::env::temp_dir()
    } else {
        std::path::PathBuf::from(&deploy_tmp_dir_str)
    };

    // S3 / L6: control authenticates the worker admin log fan-out with
    // WORKER_KEY. The same key gates the worker's dispatch bearer AND keys the
    // per-request ZeroShip-User HMAC, so it carries the >=32-byte strength floor
    // (empty and present-but-weak values are rejected). Skipped for a
    // secret REFERENCE under --check-config (the local is then the raw ref
    // string, which would wrongly fail the length check); it runs on the
    // resolved value at real boot.
    if !check_config || !zeroship_core::config::is_secret_ref(&worker_key) {
        if let Err(message) = zeroship_core::config::validate_worker_key(&worker_key) {
            eprintln!("control: {message}");
            tracing::error!(error = %message, "control: refusing to start with unsafe WORKER_KEY");
            std::process::exit(1);
        }
    }

    let mut missing = Vec::new();
    if master_key.is_empty() {
        missing.push("--master-key / MASTER_KEY");
    }
    if control_key.is_empty() {
        missing.push("--control-key / CONTROL_KEY");
    }
    if signing_key_file.is_empty() {
        missing.push("--signing-key-file / SIGNING_KEY_FILE");
    }
    if !missing.is_empty() {
        tracing::error!(
            missing = %missing.join(", "),
            "control: refusing to start; required secrets missing"
        );
        std::process::exit(1);
    }
    // Strength guards run on the RESOLVED value at real boot. During
    // `--check-config` a secret REFERENCE is still the raw `urn:`/`arn:`
    // string (not yet dereferenced), so skip the strength check for a ref - it
    // would wrongly fail length/entropy on the reference text. A literal is
    // checked in both modes.
    if !check_config || !zeroship_core::config::is_secret_ref(&master_key) {
        if let Err(message) = validate_master_key_material("MASTER_KEY", &master_key) {
            tracing::error!(error = %message, "control: refusing to start with weak MASTER_KEY");
            std::process::exit(1);
        }
    }
    for (idx, legacy_key) in legacy_keys.iter().enumerate() {
        let label = format!("LEGACY_MASTER_KEYS[{idx}]");
        if !check_config || !zeroship_core::config::is_secret_ref(legacy_key) {
            if let Err(message) = validate_master_key_material(&label, legacy_key) {
                tracing::error!(
                    error = %message,
                    "control: refusing to start with weak legacy master key"
                );
                std::process::exit(1);
            }
        }
    }
    if stripe_webhook_secret.is_empty() {
        // Not fatal: operators may run without Stripe. Every webhook will
        // reject with 500, so warn before Stripe-side retries reveal it.
        tracing::warn!(
            "control: stripe_webhook_secret unset; /internal/webhooks/stripe will reject every request. \
             Set --stripe-webhook-secret if you need Stripe integration."
        );
    }
    // STRIPE_SECRET_KEY is optional at process scope because a deployment may
    // not enable outbound Stripe operations. A provider that requires it
    // rejects an empty resolved key, and direct Stripe calls cannot authenticate
    // without it. This never changes webhook verification, which uses the
    // independent STRIPE_WEBHOOK_SECRET and always fails closed when absent.
    //
    // The dedicated pairwise-salt secret must be strong and stable. It seeds
    // the permanent per-app `pws_` anchor and must equal the gateway's value.
    // Skip the strength check when `--check-config` holds a raw reference.
    if !check_config || !zeroship_core::config::is_secret_ref(&pairwise_salt) {
        if let Err(message) = zeroship_core::config::validate_pairwise_salt(&pairwise_salt) {
            tracing::error!(error = %message, "control: refusing to start with unsafe pairwise salt");
            std::process::exit(1);
        }
    }

    // Control plane resource-server prerequisites. The console is now a
    // gateway-fronted regular app authenticated via `@zeroship/auth` (BFF) —
    // control has NO OIDC RP of its own anymore. The `AuthzGuard` bearer path +
    // audit run on the SINGLE `--db` connection (there is no separate auth DB
    // any more). Platform mode requires an explicit native OP issuer; this is
    // deployment topology and must match the issuer embedded in access tokens.
    if auth_provider_kind == AuthProviderKind::Native {
        let mut missing = Vec::new();
        if auth_platform_issuer.is_empty() {
            missing.push(PLATFORM_ISSUER_INPUT);
        }
        if !missing.is_empty() {
            tracing::error!(
                missing = %missing.join(", "),
                "control: refusing to start; the resource-server auth path requires these flags"
            );
            std::process::exit(1);
        }
    }

    if check_config {
        // M1: read-only. No filesystem mutation, no signing-key load.
        let workers_count = workers_str
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .count();
        let log_format_str = boot.log_format.to_string();

        let mut report = CheckConfigReport::new();
        report.field("port", CheckValue::Count(usize::from(port)));
        report.field("bind", CheckValue::Plain(bind_host.clone()));
        report.field(
            "config_source",
            CheckValue::Plain(boot.overlay.source.to_string()),
        );
        report.field(
            "auth_provider",
            CheckValue::Plain(auth_provider_kind.to_string()),
        );
        report.field("supabase_url", CheckValue::Plain(supabase_url.clone()));
        report.field(
            "supabase_anon_key_configured",
            CheckValue::Secret(!supabase_anon_key.is_empty()),
        );
        report.field(
            "supabase_service_role_key_configured",
            CheckValue::Secret(!supabase_service_role_key.is_empty()),
        );
        report.field(
            "supabase_jwt_secret_configured",
            CheckValue::Secret(!supabase_jwt_secret.is_empty()),
        );
        report.field("supabase_jwks_url", CheckValue::Plain(supabase_jwks_url.clone()));
        report.field(
            "supabase_jwt_issuer",
            CheckValue::Plain(supabase_jwt_issuer.clone()),
        );
        report.field(
            "auth_platform_issuer",
            CheckValue::Plain(auth_platform_issuer.clone()),
        );
        let platform_jwks_report = match platform_config(ControlPlatformAuthConfig {
            issuer: &auth_platform_issuer,
            jwks_url: &auth_platform_jwks_url,
        }) {
            Ok(Some(config)) => config.jwks_url,
            Ok(None) => String::new(),
            Err(message) => {
                eprintln!("control: {message}");
                tracing::error!(error = %message, "control: invalid platform auth provider config");
                std::process::exit(1);
            }
        };
        report.field(
            "auth_platform_jwks_url",
            CheckValue::Plain(platform_jwks_report),
        );
        report.field(
            "trusted_oauth_clients_count",
            CheckValue::Count(trusted_oauth_clients.len()),
        );
        report.field("log_filter", CheckValue::Plain(filter.clone()));
        report.field("log_format", CheckValue::Plain(log_format_str));
        report.field("trust_proxy", CheckValue::Flag(trust_proxy));
        report.field("origin_scheme", CheckValue::Plain(origin_scheme.to_string()));
        report.field("blob_store", CheckValue::Plain(blob_store_root.clone()));
        report.field("blob_store_remote", CheckValue::Flag(blob_store_is_remote));
        report.field(
            "stream_transport",
            CheckValue::Plain(
                if settings.stream_transport.get().trim().is_empty() {
                    "(disabled)".to_string()
                } else {
                    settings.stream_transport.get().clone()
                },
            ),
        );
        report.field(
            "spend_recompute_interval_secs",
            CheckValue::Count(spend_recompute_interval as usize),
        );
        report.field(
            "deploy_tmp_dir",
            CheckValue::Plain(deploy_tmp_dir.display().to_string()),
        );
        report.field(
            "pairwise_salt_configured",
            CheckValue::Secret(!pairwise_salt.is_empty()),
        );
        report.field("workers_count", CheckValue::Count(workers_count));
        report.field("gateway_url", CheckValue::Plain(gateway_url.clone()));

        report.emit(*settings.check_config_format.get());
        return Ok(());
    }

    // M1: side-effecting preflight runs only on the real startup path, after the
    // read-only `--check-config` early-return above.

    // Validate the deploy tmp dir is creatable + writable so operators don't
    // discover a misconfigured path on first deploy. Idempotent if it exists.
    if let Err(e) = std::fs::create_dir_all(&deploy_tmp_dir) {
        tracing::error!(
            path = %deploy_tmp_dir.display(),
            error = %e,
            "control: deploy_tmp_dir not creatable, refusing to start",
        );
        std::process::exit(1);
    }
    let probe = deploy_tmp_dir.join(format!(
        ".zeroship-probe-{}",
        uuid::Uuid::new_v4().simple()
    ));
    if let Err(e) = std::fs::write(&probe, b"") {
        tracing::error!(
            path = %deploy_tmp_dir.display(),
            error = %e,
            "control: deploy_tmp_dir not writable, refusing to start",
        );
        std::process::exit(1);
    }
    let _ = std::fs::remove_file(&probe);
    tracing::info!(path = %deploy_tmp_dir.display(), "control: deploy_tmp_dir configured");

    let signing_key = zeroship_authn::load_signing_key_from_path(
        std::path::Path::new(&signing_key_file),
    )
    .map_err(|err| {
        tracing::error!(error = %err, "control: failed to load PAT signing key");
        std::io::Error::new(std::io::ErrorKind::InvalidInput, err)
    })?;
    let pat_issuer = Arc::new(zeroship_authn::PatIssuer::new(&signing_key).map_err(|err| {
        tracing::error!(error = %err, "control: failed to initialize PAT issuer");
        std::io::Error::new(std::io::ErrorKind::InvalidInput, err)
    })?);

    ntex::rt::System::build()
        .name("zeroship-control")
        .build(ntex::rt::DefaultRuntime)
        .block_on(async move {
    let registry = Registry::new(&db_url)
        .await
        .expect("failed to connect to database");

    // The content-addressed `BlobStore` is the ONLY deploy-artifact store.
    // `.zship` deploys land in `{prefix}/blobs/` + `{prefix}/manifests/`;
    // control writes through the SAME store gateway + worker read (local disk
    // for dev, S3 for production). The legacy per-app `BundleStore`/VFS is
    // gone — app purge now deletes the app's manifest keyspace via
    // `BlobStore::delete_app_manifests`.
    // The S3 inputs are read HERE, not inside `zeroship-bundle`, so the read is
    // recorded against this binary. Resolved only for a remote store: on local
    // disk the credentials are legitimately absent.
    let s3_runtime = store_url.is_remote().then(|| {
        zeroship_core::resolve_s3_runtime!(zeroship_control::config::ControlSettingsConsumer)
            .expect("failed to resolve S3 credentials for the blob store")
    });
    let blob_store: Arc<dyn BlobStore> = build_blob_store(&store_url, s3_runtime.as_ref())
        .expect("failed to initialise blob store");
    let workflow_blob_store: Arc<dyn WorkflowBlobStore> =
        build_workflow_blob_store(&store_url, s3_runtime.as_ref())
            .expect("failed to initialise workflow blob store");

    if !legacy_keys.is_empty() {
        tracing::info!(
            legacy_keys = legacy_keys.len(),
            "control: EnvStore booted with legacy master keys (rotation grace period)"
        );
    }
    let legacy_key_refs: Vec<&str> = legacy_keys.iter().map(String::as_str).collect();
    let env_store = EnvStore::new_with_previous(
        registry.clone(),
        &master_key,
        &legacy_key_refs,
    )
    .expect("env store init");
    let stripe_store = StripeStore::new(registry.clone());

    // Platform-wide pairwise salt (auth-sdk §6.2) — derived from the DEDICATED
    // `PAIRWISE_SALT` secret (NOT the stash key), via the SHARED helper, so
    // control's disconnect-app revocation writes the family marker on the SAME
    // `(client_id, pws_)` key the gateway arms read (Batch A fix 4). The SAME
    // `PAIRWISE_SALT` value must be configured on gateway + control, and is the
    // PERMANENT per-app identity anchor (never rotate without a migration).
    let pairwise_salt = zeroship_core::auth::derive_pairwise_salt(pairwise_salt.as_bytes());
    // Control plane is a pure API resource server: no console OIDC RP. The
    // selected auth provider still drives the OAuth-bearer arm of the
    // `AuthzGuard` after local PAT verification fails.
    let auth_provider = match build_control_auth_provider(
        auth_provider_kind,
        ControlSupabaseAuthConfig {
            url: &supabase_url,
            anon_key: &supabase_anon_key,
            service_role_key: &supabase_service_role_key,
            jwt_secret: &supabase_jwt_secret,
            jwks_url: &supabase_jwks_url,
            jwt_issuer: &supabase_jwt_issuer,
        },
        ControlPlatformAuthConfig {
            issuer: &auth_platform_issuer,
            jwks_url: &auth_platform_jwks_url,
        },
    ) {
        Ok(provider) => provider,
        Err(message) => {
            eprintln!("control: {message}");
            tracing::error!(error = %message, "control: refusing to start with invalid auth provider");
            std::process::exit(1);
        }
    };

    // Single shared long-lived connection on the one physical `zeroship` DB
    // (`--db`). There is no separate auth database any more — the former
    // `--auth-db` was only ever a config capability that compose already
    // pointed at this same DB. The `AuthzGuard` bearer path, audit emitter, and
    // OAuth-grant handlers pipeline onto this handle; anything needing a
    // transaction opens its own owned connection via `registry.conn()`.
    let control_pg: Arc<compio_postgres::Client> = {
        let (pg_client, pg_conn) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
            .await
            .expect("control: control-pg connect");
        compio::runtime::spawn(async move {
            if let Err(e) = pg_conn.run().await {
                tracing::error!(error = %e, "control/control-pg connection ended");
            }
        })
        .detach();
        Arc::new(pg_client)
    };

    // Console seed (R5): make the console deployable + served as a platform-owned
    // regular app. In-process + idempotent + trusted; NEVER an HTTP route.
    // Runs AFTER migrate (zeroship-migrate, out of band), AFTER the registry / env
    // store / blob store are up, and BEFORE AppState is constructed (registry +
    // env_store are moved into it below). The console is a pure creator app, so
    // the seed touches only the control schema (apps / oauth / env) — no PAT, no
    // auth-schema service principal.
    //
    // Compose wiring (R5 cutover — DONE): the console `.zship` is built in the
    // Docker `sdks` stage and COPYed to `/opt/zeroship/console/app.zship`; the
    // control service runs with `--bootstrap-console --console-host
    // console.zeroship.localhost --console-zship /opt/zeroship/console/app.zship`,
    // ordered after the `migrate` service. `deploy/ops/Caddyfile` routes
    // `console.zeroship.localhost` → the gateway (the console is a gateway-fronted
    // app); the separate Vite builder service is retired.
    // Seed the built-in plan tiers (free/pro/unlimited) UNCONDITIONALLY at boot
    // — independent of `--bootstrap-console`. Because `apps.plan_id` is an FK
    // into `zeroship.plans`, `create_app`/`set_plan` (and the console seed) all
    // require the built-in plans to exist. Idempotent (ON CONFLICT DO UPDATE on
    // the deterministic `pln_…` ids), so a re-boot is a no-op.
    plan_catalog::seed_plans(&registry)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "control: plan-catalog seed failed");
            std::io::Error::other(err.to_string())
        })?;

    let provider_config_json: serde_json::Value =
        match serde_json::from_str(settings.provider_config.get()) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "control: --provider-config must be valid JSON");
            std::process::exit(1);
        }
    };

    // Tax provider (PR-5): parse the kind, then build it. `native` (default)
    // computes 0 (USD launch). An unknown value refuses to boot rather than
    // silently mis-taxing.
    let tax_provider_kind = match zeroship_control::tax::TaxProviderKind::parse(settings.tax_provider.get()) {
        Ok(k) => k,
        Err(bad) => {
            tracing::error!(value = %bad, "control: unknown --tax-provider (expected native)");
            std::process::exit(1);
        }
    };
    let tax_provider = match zeroship_control::tax::build_tax_provider(
        &zeroship_control::tax::TaxProviderConfig { kind: tax_provider_kind },
    ) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "control: refusing to start — tax provider not available");
            std::process::exit(1);
        }
    };
    tracing::info!(tax_provider = tax_provider_kind.as_str(), "control: tax provider selected");

    let provider_registry = zeroship_control::metering::provider::builtin_registry();
    let lite_store = Arc::new(zeroship_control::metering::provider::ControlLiteStore::new(
        registry.clone(),
        StripeStore::new(registry.clone()),
        zeroship_control::SecretString::new(stripe_secret_key.clone()),
        stripe_base_url.clone(),
        Arc::clone(&tax_provider),
    ));
    let mut secret_values = std::collections::HashMap::new();
    secret_values.insert("stripe_secret_key".to_string(), stripe_secret_key.clone());
    secret_values.insert("stripe_webhook_secret".to_string(), stripe_webhook_secret.clone());
    let provider_ctx = zeroship_control::metering::provider::ProviderCtx::new(
        provider_config_json,
        Arc::new(zeroship_control::metering::provider::StaticSecretResolver::new(
            secret_values,
        )),
        Some(lite_store),
    );
    let billing_stack = match zeroship_control::metering::provider::build_stack(
        &provider_registry,
        &provider_ctx,
        &zeroship_control::metering::provider::BillingStackConfig {
            meter_provider: settings.meter_provider.get().clone(),
            invoicer_provider: settings.invoicer_provider.get().clone(),
            production: true,
            allow_unsupported_billing: *settings.allow_unsupported_billing.get(),
        },
    ) {
        Ok(stack) => Arc::new(stack),
        Err(e) => {
            tracing::error!(error = %e, "control: refusing to start — billing provider stack invalid");
            std::process::exit(1);
        }
    };
    tracing::info!(
        meter_provider = billing_stack.meter_id(),
        invoicer_provider = billing_stack.invoicer_id(),
        "control: billing provider stack selected"
    );

    // Resolve the billing stream from CLI/env, falling back to the `[metering]`
    // file overlay so the same section that configures the producers (worker +
    // gateway) can configure the control-plane consumers — fully from
    // zeroship.toml, not env-only. Explicit --stream-transport / --stream-config
    // (or their env) still win.
    let effective_transport = Some(settings.stream_transport.get().trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            file_metering
                .redpanda_brokers
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(|_| "redpanda".to_string())
        });
    let effective_stream_config = if settings.stream_config.get().trim() != "{}"
        && !settings.stream_config.get().trim().is_empty()
    {
        settings.stream_config.get().clone()
    } else if let Some(brokers) = file_metering
        .redpanda_brokers
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        // Matches zeroship_metering::DEFAULT_USAGE_EVENTS_TOPIC.
        let topic = file_metering
            .usage_events_topic
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("usage-events");
        serde_json::json!({ "brokers": brokers, "topic": topic }).to_string()
    } else {
        settings.stream_config.get().clone()
    };

    let billing_stream = match effective_transport
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(id) => {
            let stream_config_json: serde_json::Value =
                match serde_json::from_str(&effective_stream_config) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = %e, "control: --stream-config must be valid JSON");
                        std::process::exit(1);
                    }
                };
            let mut registry = zeroship_stream::StreamRegistry::default();
            zeroship_stream::adapters::register_builtin(&mut registry);
            let stream_registry = Arc::new(registry);
            let config = zeroship_stream::StreamConfig::from(stream_config_json);
            match zeroship_control::BillingStreamConfig::new(
                Arc::clone(&stream_registry),
                id,
                config,
                settings.billing_forwarder_group_id.get().clone(),
                settings.spend_recompute_group_id.get().clone(),
            ) {
                Ok(streams) => {
                    if let Err(e) = streams
                        .build_forwarder()
                        .and_then(|_| streams.build_recompute())
                    {
                        tracing::error!(error = %e, "control: refusing to start — stream transport invalid");
                        std::process::exit(1);
                    }
                    if let Err(e) = streams.start_control_usage_outbox() {
                        tracing::error!(
                            error = %e,
                            "control: refusing to start — control usage outbox unavailable"
                        );
                        std::process::exit(1);
                    }
                    // The in-memory transport is registered in the same
                    // `register_builtin` registry as redpanda and is selectable
                    // here by one string, so nothing downstream tells them
                    // apart. It reports durable success for a push into a
                    // process-static Vec, and the usage outbox reads that `Ok`
                    // as broker-acked and TRIMS its redb WAL - so choosing it
                    // silently converts the billing path from durable to
                    // best-effort, and a restart loses every event not yet
                    // forwarded.
                    //
                    // Only the control plane can reach this: the worker and
                    // gateway producers build their outbox through
                    // `zeroship_metering::build_usage_outbox`, which hardcodes
                    // "redpanda".
                    if streams.transport_id() == "memory" {
                        tracing::warn!(
                            stream = streams.transport_id(),
                            "control: in-memory billing stream selected - usage events are NOT \
                             durable and are not shared between processes; the outbox WAL is \
                             trimmed on a publish that only reached this process's memory. \
                             Intended for tests and local development."
                        );
                    }
                    tracing::info!(
                        stream = streams.transport_id(),
                        forwarder_group_id = streams.forwarder_group_id(),
                        recompute_group_id = streams.recompute_group_id(),
                        "control: billing event stream selected"
                    );
                    Some(streams)
                }
                Err(e) => {
                    tracing::error!(error = %e, "control: refusing to start — stream transport invalid");
                    std::process::exit(1);
                }
            }
        }
        None => {
            tracing::info!(
                "control: billing event stream disabled; stream forwarding and spend recompute are disabled"
            );
            None
        }
    };

    // Billing notifier (PR-6): a `BillingNotifier` over the relocated `zeroship-mailer`
    // `Mailer` built above. Wraps the mailer + the per-message idempotency key.
    let notifier: Arc<dyn zeroship_control::notify::BillingNotifier> =
        Arc::new(zeroship_control::notify::MailerNotifier::new(billing_mailer));
    tracing::info!(mailer = %mailer_kind, "control: billing notifier selected");

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        blob_store,
        workflow_blob_store,
        control_key: zeroship_control::SecretString::new(control_key),
        master_key: zeroship_control::SecretString::new(master_key),
        stripe_webhook_secret: zeroship_control::SecretString::new(stripe_webhook_secret),
        stripe_secret_key: zeroship_control::SecretString::new(stripe_secret_key),
        stripe_base_url,
        gateway_url,
        worker_urls: workers_str
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        worker_key: zeroship_control::SecretString::new(worker_key),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(30, 60))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(50, 600))),
        trust_proxy,
        deploy_tmp_dir,
        control_pg,
        app_base_domain,
        origin_scheme,
        trusted_oauth_clients,
        expected_oauth_audience,
        static_policies: zeroship_authz::load_platform_policies()
            .expect("control: bundled authz policies parse"),
        pat_issuer,
        auth_provider,
        provider_registry,
        billing_stack,
        billing_stream,
        tax_provider,
        notifier,
        pairwise_salt,
        projected_charge_cache: Arc::new(
            zeroship_control::billing_read::ProjectedChargeCache::default(),
        ),
    });

    // Spawn the in-process control crons:
    //   - audit_retention: sanctioned deleter for the append-only
    //     `zeroship.app_audit` + `zeroship.authz_decisions` tables (peer of the
    //     auth `audit_events` sweep; shares the `zeroship.audit_retention` GUC).
    //   - orphaned_app_reaper: purges apps left owner-less by the ISS-12
    //     account-erase reaper (DB row + blobs), excluding the `system = true`
    //     platform console.
    // Both hold an `Arc<AppState>` clone (cheap) and open fresh per-tick
    // connections.
    zeroship_control::cron::spawn_all_with_options(
        Arc::clone(&state),
        audit_retention_months,
        audit_retention_check_secs,
        spend_recompute_interval,
        zeroship_control::cron::SpawnOptions {
            workflow_scan: !*settings.disable_workflow_engine.get(),
            workflow_reaper: !*settings.disable_workflow_engine.get(),
            workflow_sweeps: !*settings.disable_workflow_engine.get(),
            scheduler_authoritative: false,
        },
    );
    tracing::info!(
        retention_months = audit_retention_months,
        check_secs = audit_retention_check_secs,
        "control: audit-retention + orphaned-app-reaper crons spawned"
    );

    let bind_addr = format!("{bind_host}:{port}");
    tracing::info!(bind = %bind_addr, "zeroship-control listening");

    web::server(async move || {
        web::App::new()
            .state(state.clone())
            .configure(admin_handlers::configure)
            .configure(oauth_handlers::configure)
            // --- Admin API ---
            .service(
                web::resource("/api/apps")
                    .route(web::post().to(api::create_app))
                    .route(web::get().to(api::list_apps)),
            )
            .service(
                web::resource("/api/apps/{id}")
                    .route(web::get().to(api::get_app))
                    .route(web::delete().to(api::delete_app)),
            )
            .service(
                // 256MB cap matches `MAX_COMPRESSED_BYTES` in deploy.rs.
                // ntex's PayloadConfig only enforces a single upper
                // bound on the request body — the decompressed cap is
                // enforced separately as we read the tar stream.
                web::resource("/api/apps/{id}/deploy")
                    .state(web::types::PayloadConfig::new(
                        zeroship_control::deploy::MAX_COMPRESSED_BYTES,
                    ))
                    .route(web::post().to(api::deploy)),
            )
            .service(
                web::resource("/api/apps/{id}/plan")
                    .route(web::put().to(api::set_plan)),
            )
            // --- Spend-limit override: creator-facing cap ---
            .service(
                web::resource("/api/apps/{id}/spend-limit")
                    .route(web::get().to(api::get_spend_limit))
                    .route(web::put().to(api::set_spend_limit)),
            )
            // --- Creator billing READ APIs (PR-7, BillingRead) -------------
            // Creator-scoped to OWNED apps; operator (Resource::Any) reads any.
            .service(
                web::resource("/api/apps/{id}/invoices")
                    .route(web::get().to(api::list_app_invoices)),
            )
            .service(
                web::resource("/api/apps/{id}/projected-charge")
                    .route(web::get().to(api::get_projected_charge)),
            )
            .service(
                web::resource("/api/apps/{id}/billing-status")
                    .route(web::get().to(api::get_billing_status)),
            )
            .service(
                web::resource("/api/invoices/{id}")
                    .route(web::get().to(api::get_invoice)),
            )
            .service(
                web::resource("/api/billing/credit-balance")
                    .route(web::get().to(api::get_credit_balance)),
            )
            .service(
                web::resource("/api/billing/payment-method")
                    .route(web::get().to(api::get_payment_method)),
            )
            // --- Plan catalog: operator-editable pricing catalog ---
            .service(
                web::resource("/api/plans")
                    .route(web::get().to(api::list_plans)),
            )
            .service(
                web::resource("/api/plans/{id}")
                    .route(web::get().to(api::get_plan))
                    .route(web::put().to(api::upsert_plan))
                    .route(web::delete().to(api::archive_plan)),
            )
            // Global default FX (gap #28) — operator-only (BillingRead/Write on
            // Resource::Any); the missing runtime lever for the GLOBAL FX a plan
            // inherits when `plans.fx` is NULL.
            .service(
                web::resource("/api/pricing-config")
                    .route(web::get().to(api::get_pricing_config))
                    .route(web::put().to(api::set_pricing_config)),
            )
            // Operator credit grant (billing-ops gap #26, PR-2) — OPERATOR-ONLY
            // (BillingWrite on Resource::Any). Idempotency-Key header required.
            .service(
                web::resource("/api/billing/credit")
                    .route(web::post().to(api::grant_credit)),
            )
            // Operator refund + void/reissue (billing-ops gap #26, PR-3) —
            // OPERATOR-ONLY (BillingWrite on Resource::Any). Refund requires an
            // Idempotency-Key header. {id} is the internal inv_… invoice id.
            .service(
                web::resource("/api/invoices/{id}/refunds")
                    .route(web::post().to(api::refund_invoice)),
            )
            .service(
                web::resource("/api/invoices/{id}/void")
                    .route(web::post().to(api::void_invoice)),
            )
            .service(
                web::resource("/api/apps/{id}/usage")
                    .route(web::get().to(api::get_usage)),
            )
            .service(
                web::resource("/api/apps/{id}/logs")
                    .route(web::get().to(api::get_app_logs)),
            )
            .service(
                web::resource("/api/apps/{id}/vars")
                    .state(web::types::PayloadConfig::new(
                        env_handlers::ENV_MUTATION_PAYLOAD_BYTES,
                    ))
                    .route(web::get().to(env_handlers::list_vars))
                    .route(web::post().to(env_handlers::set_var)),
            )
            .service(
                web::resource("/api/apps/{id}/vars/{key}")
                    .route(web::delete().to(env_handlers::delete_var)),
            )
            .service(
                web::resource("/api/apps/{id}/secrets")
                    .state(web::types::PayloadConfig::new(
                        env_handlers::ENV_MUTATION_PAYLOAD_BYTES,
                    ))
                    .route(web::get().to(env_handlers::list_secrets))
                    .route(web::post().to(env_handlers::set_secret)),
            )
            .service(
                web::resource("/api/apps/{id}/secrets/{key}")
                    .route(web::delete().to(env_handlers::delete_secret)),
            )
            .service(
                web::resource("/api/apps/{id}/env/expose")
                    .route(web::get().to(env_handlers::list_expose))
                    .route(web::put().to(env_handlers::set_expose)),
            )
            .service(
                web::resource("/api/apps/{id}/audit")
                    .route(web::get().to(env_handlers::list_audit)),
            )
            // --- Auth (resource server) ---
            // No console OIDC RP and no console back-channel-logout endpoint:
            // the console is now a gateway-fronted regular app authenticated
            // via `@zeroship/auth` (BFF). Per-app back-channel logout for the
            // console is handled by the GATEWAY's own per-app BCL endpoint (it
            // is a gateway app like any other). Control exposes only the PAT /
            // OAuth-grant management surfaces below.
            .configure(device_handlers::configure)
            .configure(token_handlers::configure)
            .configure(oauth_grants_handlers::configure)
            // --- Stripe Connect ---
            .service(
                web::resource("/api/creators/{id}/stripe/onboard")
                    .route(web::post().to(stripe_handlers::onboard)),
            )
            .service(
                web::resource("/api/creators/{id}/stripe/callback")
                    .route(web::post().to(stripe_handlers::callback)),
            )
            // --- Stream-2 Connect: server-stamped checkout + operator fee policy (G1) ---
            .service(
                web::resource("/api/creators/{id}/connect/checkout")
                    .route(web::post().to(stripe_handlers::connect_checkout)),
            )
            .service(
                web::resource("/api/creators/{id}/fee-policy")
                    .route(web::put().to(stripe_handlers::set_fee_policy)),
            )
            // --- Infrastructure-billing setup: platform Customer + card ---
            .service(
                web::resource("/api/creators/{id}/billing/setup")
                    .route(web::post().to(stripe_handlers::billing_setup)),
            )
            .service(
                web::resource("/api/creators/{id}/stripe")
                    .route(web::delete().to(stripe_handlers::unlink)),
            )
            .service(
                web::resource("/api/creators/{id}/earnings")
                    .route(web::get().to(stripe_handlers::earnings)),
            )
            // --- Internal API ---
            .service(
                web::resource("/internal/versions")
                    .route(web::get().to(internal::get_versions)),
            )
            .service(
                web::resource("/internal/apps/{app_id}")
                    .route(web::get().to(internal::get_app_version)),
            )
            .service(
                web::resource("/internal/apps/{app_id}/env")
                    .route(web::get().to(internal::get_app_env)),
            )
            .service(
                web::resource("/internal/routes")
                    .route(web::get().to(internal::get_routes)),
            )
            .configure(workflow_instance_api::configure)
            .service(
                web::resource("/internal/billing/reconcile")
                    .route(web::post().to(internal::force_reconcile)),
            )
            .service(
                web::resource("/internal/spend/reconcile")
                    .route(web::post().to(internal::force_spend_reconcile)),
            )
            .service(
                web::resource("/internal/webhooks/stripe")
                    // Give the Bytes extractor headroom above the handler's body cap so
                    // the handler (not the extractor's default-256KiB 400) owns the
                    // oversized-body rejection with its descriptive 413.
                    .state(stripe_handlers::webhook_payload_config())
                    .route(web::post().to(stripe_handlers::webhook)),
            )
            // --- Health ---
            .service(
                web::resource("/health")
                    .route(web::get().to(internal::health)),
            )
    })
    .bind(&bind_addr)?
    .run()
    .await
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_core::config::{GeneratedConfig, OriginScheme};

    static CONFIG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn restore_env_var(key: &str, old: Option<std::ffi::OsString>) {
        match old {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn control_blob_store_flag_uses_unified_name() {
        let cli =
            ControlCli::try_parse_from(["zeroship-control", "--blob-store", "/tmp/blob-root"])
                .expect("blob-store flag should parse");

        assert_eq!(cli.settings.blob_store.as_deref(), Some("/tmp/blob-root"));
    }

    #[test]
    fn origin_scheme_cli_overrides_environment_and_environment_overrides_file() {
        let _guard = CONFIG_ENV_LOCK.lock().expect("env lock");
        let old = zeroship_core::test_env_os!("ZEROSHIP_ORIGIN_SCHEME");
        std::env::set_var("ZEROSHIP_ORIGIN_SCHEME", "http");

        // The environment reaches the same clap carrier the flag does, and the
        // generated resolver prefers that carrier over the overlay. The
        // hand-written resolve_origin_scheme helper this used to call is gone.
        let overlay: toml::Value =
            toml::from_str("origin_scheme = \"https\"\n").expect("fixture overlay");
        let env = ControlCli::try_parse_from(["zeroship-control"]).expect("parse env topology");
        let resolved =
            ControlSettings::resolve_config(env.settings, Some(&overlay)).expect("resolve");
        assert_eq!(resolved.origin_scheme.get(), &OriginScheme::Http);

        let cli = ControlCli::try_parse_from([
            "zeroship-control",
            "--origin-scheme",
            "https",
        ])
        .expect("parse CLI topology");
        let flagged =
            ControlSettings::resolve_config(cli.settings, Some(&overlay)).expect("resolve");
        assert_eq!(flagged.origin_scheme.get(), &OriginScheme::Https);

        restore_env_var("ZEROSHIP_ORIGIN_SCHEME", old);
    }

    #[test]
    fn auth_provider_selector_defaults_to_native_and_accepts_supabase() {
        let cli = ControlCli::try_parse_from(["zeroship-control"]).expect("parse defaults");
        let resolved = ControlSettings::resolve_config(cli.settings, None).expect("resolve");
        assert_eq!(resolved.auth_provider.get(), &AuthProviderKind::Native);

        let cli = ControlCli::try_parse_from(["zeroship-control", "--auth-provider", "supabase"])
            .expect("parse supabase");
        let resolved = ControlSettings::resolve_config(cli.settings, None).expect("resolve");
        assert_eq!(resolved.auth_provider.get(), &AuthProviderKind::Supabase);
    }

    #[test]
    fn the_retired_platform_spelling_is_rejected_by_the_flag_and_the_overlay() {
        // `platform` was control's own word for the state now spelled `native`.
        // Both tiers must refuse it, or the two vocabularies survive the merge
        // in the one place an operator would not look.
        let err = ControlCli::try_parse_from([
            "zeroship-control",
            "--auth-provider",
            "platform",
        ])
        .map(|_| ())
        .expect_err("the retired control spelling must not parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);

        let overlay: toml::Value = toml::from_str("[auth]\nprovider = \"platform\"\n")
            .expect("fixture overlay");
        let cli = ControlCli::try_parse_from(["zeroship-control"]).expect("parse defaults");
        let err = ControlSettings::resolve_config(cli.settings, Some(&overlay))
            .map(|_| ())
            .expect_err("the retired control spelling must not resolve from the overlay");
        assert!(
            format!("{err}").contains("auth.provider"),
            "the overlay rejection must name the key: {err}"
        );
    }

    #[test]
    fn the_shared_variable_reaches_control_from_the_environment() {
        // The cross-binary half of this - control and auth resolving the SAME
        // process variable to the same value - is driven end to end against
        // both real binaries in `tests/config_check_e2e.sh`, which is the only
        // vector that can observe two processes at once.
        let _guard = CONFIG_ENV_LOCK.lock().expect("env lock");
        let old = zeroship_core::test_env_os!("ZEROSHIP_AUTH_PROVIDER");
        std::env::set_var("ZEROSHIP_AUTH_PROVIDER", "supabase");

        let cli = ControlCli::try_parse_from(["zeroship-control"]).expect("parse control");
        let resolved = ControlSettings::resolve_config(cli.settings, None).expect("resolve");
        assert_eq!(resolved.auth_provider.get(), &AuthProviderKind::Supabase);

        restore_env_var("ZEROSHIP_AUTH_PROVIDER", old);
    }

    #[test]
    fn supabase_auth_provider_requires_anon_key_and_pinned_mode() {
        let err = build_control_auth_provider(
            AuthProviderKind::Supabase,
            ControlSupabaseAuthConfig {
                url: "https://project.supabase.co",
                anon_key: "",
                service_role_key: "",
                jwt_secret: "test-supabase-jwt-secret-at-least-32-bytes",
                jwks_url: "",
                jwt_issuer: "https://project.supabase.co/auth/v1",
            },
            ControlPlatformAuthConfig {
                issuer: "",
                jwks_url: "",
            },
        )
        .unwrap_err();
        assert!(err.contains("SUPABASE_ANON_KEY"), "unexpected error: {err}");

        let err = build_control_auth_provider(
            AuthProviderKind::Supabase,
            ControlSupabaseAuthConfig {
                url: "https://project.supabase.co",
                anon_key: "anon",
                service_role_key: "",
                jwt_secret: "test-supabase-jwt-secret-at-least-32-bytes",
                jwks_url: "https://project.supabase.co/auth/v1/.well-known/jwks.json",
                jwt_issuer: "https://project.supabase.co/auth/v1",
            },
            ControlPlatformAuthConfig {
                issuer: "",
                jwks_url: "",
            },
        )
        .unwrap_err();
        assert!(
            err.contains("exactly one Supabase verification mode"),
            "unexpected error: {err}"
        );

        let provider = build_control_auth_provider(
            AuthProviderKind::Supabase,
            ControlSupabaseAuthConfig {
                url: "https://project.supabase.co",
                anon_key: "anon",
                service_role_key: "",
                jwt_secret: "test-supabase-jwt-secret-at-least-32-bytes",
                jwks_url: "",
                jwt_issuer: "https://project.supabase.co/auth/v1",
            },
            ControlPlatformAuthConfig {
                issuer: "",
                jwks_url: "",
            },
        )
        .expect("valid HS256 supabase provider");
        assert_eq!(provider.issuer(), "https://project.supabase.co/auth/v1");

        let provider = build_control_auth_provider(
            AuthProviderKind::Supabase,
            ControlSupabaseAuthConfig {
                url: "https://project.supabase.co",
                anon_key: "anon",
                service_role_key: "",
                jwt_secret: "test-supabase-jwt-secret-at-least-32-bytes",
                jwks_url: "",
                jwt_issuer: "https://project.supabase.co/auth/v1",
            },
            ControlPlatformAuthConfig {
                issuer: "https://auth.zeroship.test",
                jwks_url: "",
            },
        )
        .expect("valid dual-issuer provider");
        assert_eq!(
            provider.issuer(),
            "https://project.supabase.co/auth/v1",
            "dual issuer reports the legacy issuer for Supabase device-flow helpers"
        );
    }

    /// Every environment variable name `zeroship-control` actually reads.
    ///
    /// DERIVED, never listed. The clap command carries `env = "..."` for the
    /// binary's own hand-written args AND for every flattened generated
    /// operational setting; `SPECS` adds the converted secrets, which have no
    /// clap env tier by construction. A list here would be a third spelling
    /// that could be edited to agree with a stale diagnostic, which is the
    /// failure this test exists to catch.
    fn env_names_control_reads() -> std::collections::BTreeSet<String> {
        let mut names = std::collections::BTreeSet::new();
        let command = <ControlCli as clap::CommandFactory>::command();
        for arg in command.get_arguments() {
            if let Some(env) = arg.get_env() {
                names.insert(env.to_string_lossy().into_owned());
            }
        }
        for spec in ControlSettings::SPECS {
            if let Some(env) = spec.env_name() {
                names.insert(env);
            }
        }
        names
    }

    /// The substrings of `text` that are shaped like an environment name.
    ///
    /// A maximal run of `[A-Z0-9_]` holding at least one underscore. That
    /// shape admits `SUPABASE_ANON_KEY` and `ZEROSHIP_AUTH_PROVIDER` while
    /// excluding ordinary prose and bare acronyms such as `URL` or `JWKS`.
    fn env_like_tokens(text: &str) -> Vec<String> {
        text.split(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .filter(|token| token.len() >= 4 && token.contains('_'))
            .map(str::to_owned)
            .collect()
    }

    /// Every auth-provider diagnostic, obtained by DRIVING the code that emits
    /// it rather than by copying its wording.
    fn auth_provider_diagnostics() -> Vec<String> {
        let no_supabase = ControlSupabaseAuthConfig {
            url: "https://project.supabase.co",
            anon_key: "",
            service_role_key: "",
            jwt_secret: "test-supabase-jwt-secret-at-least-32-bytes",
            jwks_url: "",
            jwt_issuer: "https://project.supabase.co/auth/v1",
        };
        let no_platform = ControlPlatformAuthConfig {
            issuer: "",
            jwks_url: "",
        };
        vec![
            build_control_auth_provider(AuthProviderKind::Supabase, no_supabase, no_platform)
                .map(|_| ())
                .expect_err("supabase without an anon key must fail closed"),
            build_control_auth_provider(AuthProviderKind::Native, no_supabase, no_platform)
                .map(|_| ())
                .expect_err("native without an issuer must fail closed"),
            platform_config(ControlPlatformAuthConfig {
                issuer: "",
                jwks_url: "https://auth.zeroship.test/oauth2/.well-known/jwks.json",
            })
            .map(|_| ())
            .expect_err("a JWKS URL without an issuer must fail closed"),
            PLATFORM_ISSUER_INPUT.to_owned(),
        ]
    }

    #[test]
    fn every_auth_provider_diagnostic_names_a_variable_control_reads() {
        // The defect this pins: all three of these diagnostics named
        // ZEROSHIP_AUTH_PROVIDER while control read
        // ZEROSHIP_CONTROL_AUTH_PROVIDER, and two named AUTH_PLATFORM_ISSUER
        // after that variable had gained its ZEROSHIP_ prefix. An operator who
        // followed the advice edited a variable this binary does not read.
        //
        // What this does NOT catch: a diagnostic that names a variable control
        // really does read but that is the WRONG one for the failure at hand,
        // and any stale name in a diagnostic outside the set
        // `auth_provider_diagnostics` can reach.
        let readable = env_names_control_reads();
        assert!(
            readable.contains("ZEROSHIP_AUTH_PROVIDER"),
            "the derivation itself is broken: control's own selector is absent"
        );

        for diagnostic in auth_provider_diagnostics() {
            let tokens = env_like_tokens(&diagnostic);
            assert!(
                !tokens.is_empty(),
                "diagnostic names no variable at all: {diagnostic:?}"
            );
            for token in tokens {
                assert!(
                    readable.contains(&token),
                    "diagnostic {diagnostic:?} tells the operator to set {token}, \
                     which zeroship-control does not read"
                );
            }
        }
    }

    #[test]
    fn the_diagnostic_check_rejects_a_variable_control_does_not_read() {
        // The one-variable control for the test above. Same instrument, same
        // token shape, one thing changed: a name nothing declares. Without
        // this, a `readable` set that had silently become everything - or an
        // `env_like_tokens` that matched nothing - would still print green.
        let readable = env_names_control_reads();
        let stale = "ZEROSHIP_CONTROL_AUTH_PROVIDER is required for supabase";
        assert_eq!(
            env_like_tokens(stale),
            vec!["ZEROSHIP_CONTROL_AUTH_PROVIDER".to_owned()],
            "the token scanner must see the retired name"
        );
        assert!(
            !readable.contains("ZEROSHIP_CONTROL_AUTH_PROVIDER"),
            "the retired control-scoped name must no longer be read"
        );
    }

    #[test]
    fn native_auth_provider_requires_issuer_and_uses_default_jwks_url() {
        let err = build_control_auth_provider(
            AuthProviderKind::Native,
            ControlSupabaseAuthConfig {
                url: "",
                anon_key: "",
                service_role_key: "",
                jwt_secret: "",
                jwks_url: "",
                jwt_issuer: "",
            },
            ControlPlatformAuthConfig {
                issuer: "",
                jwks_url: "",
            },
        )
        .unwrap_err();
        assert!(
            err.contains("ZEROSHIP_AUTH_PLATFORM_ISSUER"),
            "unexpected error: {err}"
        );

        let provider = build_control_auth_provider(
            AuthProviderKind::Native,
            ControlSupabaseAuthConfig {
                url: "",
                anon_key: "",
                service_role_key: "",
                jwt_secret: "",
                jwks_url: "",
                jwt_issuer: "",
            },
            ControlPlatformAuthConfig {
                issuer: "https://auth.zeroship.test/oauth2",
                jwks_url: "",
            },
        )
        .expect("valid platform provider");
        assert_eq!(provider.issuer(), "https://auth.zeroship.test/oauth2");
    }

    #[test]
    fn control_rejects_removed_bundles_flag() {
        let err = ControlCli::try_parse_from([
            "zeroship-control",
            "--bundles",
            "/tmp/bundle-root",
        ])
        .unwrap_err();

        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    // Master-key strength logic lives in `zeroship_core::config::secrets`.
    // This asserts control still calls through to the shared validator.
    #[test]
    fn master_key_accepts_32_byte_hex() {
        let key = "00".repeat(32);
        assert!(validate_master_key_material("MASTER_KEY", &key).is_ok());
    }

    #[test]
    fn control_rejects_removed_relaxation_flag() {
        let err = ControlCli::try_parse_from(["zeroship-control", "--dev-insecure"])
            .expect_err("removed flag must be unknown");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    // M4: resolve_trusted_oauth_clients distinguishes absent / present.
    #[test]
    fn trusted_oauth_clients_none_is_default_set() {
        let auth = zeroship_core::config::AuthSection {
            trusted_oauth_clients: None,
            ..Default::default()
        };
        assert_eq!(
            zeroship_control::resolve_trusted_oauth_clients(&auth),
            zeroship_control::default_trusted_oauth_clients()
        );
    }

    #[test]
    fn trusted_oauth_clients_some_empty_is_empty_set() {
        let auth = zeroship_core::config::AuthSection {
            trusted_oauth_clients: Some(Vec::new()),
            ..Default::default()
        };
        assert!(zeroship_control::resolve_trusted_oauth_clients(&auth).is_empty());
    }

    #[test]
    fn trusted_oauth_clients_some_vec_is_exactly_that_set() {
        let auth = zeroship_core::config::AuthSection {
            trusted_oauth_clients: Some(vec!["a".to_string(), "b".to_string()]),
            ..Default::default()
        };
        let resolved = zeroship_control::resolve_trusted_oauth_clients(&auth);
        assert_eq!(resolved.len(), 2);
        assert!(resolved.contains("a"));
        assert!(resolved.contains("b"));
    }

    // Secret-reference resolver wiring. The resolution/validation helpers live in
    // `zeroship_core::config::secrets` (fully tested there). These assert control's
    // wiring contract: literals pass through byte-identically, the strength-guard
    // skip is gated on `is_secret_ref`, and malformed references are rejected.

    // (a) A literal secret resolves to itself byte-for-byte through the public
    // resolver — control must keep literal behavior unchanged on the boot path.
    #[test]
    fn literal_secret_resolves_to_itself() {
        let literal = "00".repeat(32); // a typical 32-byte-hex master key literal
        assert_eq!(
            zeroship_core::config::resolve_secret(&literal).expect("literal resolves"),
            literal
        );
        // A DSN literal (carries a password) is likewise unchanged.
        let dsn = "postgres://user:pass@db.internal:5432/zeroship";
        assert_eq!(
            zeroship_core::config::resolve_secret(dsn).expect("dsn literal resolves"),
            dsn
        );
    }

    // (b) The boolean that gates each strength guard: under --check-config a
    // *referenced* secret SKIPS the strength check (the local is the raw ref
    // string), while a *literal* still triggers it. At real boot the guard always
    // runs. This is exactly `!check_config || !is_secret_ref(value)`.
    #[test]
    fn check_config_skips_strength_guard_for_reference_only() {
        let is_ref = zeroship_core::config::is_secret_ref;
        let guard_runs = |check_config: bool, value: &str| !check_config || !is_ref(value);

        let reference = "urn:zeroship:env:MASTER_KEY";
        let literal = "00".repeat(32);

        // check-config + reference -> guard SKIPPED (would wrongly fail on ref text).
        assert!(!guard_runs(true, reference));
        // check-config + literal -> guard RUNS (literal must still be strength-checked).
        assert!(guard_runs(true, &literal));
        // real boot -> guard always RUNS, ref or literal (value is already resolved).
        assert!(guard_runs(false, reference));
        assert!(guard_runs(false, &literal));
    }

    // (c) A malformed reference (reserved `urn:zeroship:` scheme, unknown backend)
    // is rejected by the format-only validator the check-config path uses.
    #[test]
    fn malformed_reference_is_rejected() {
        assert!(zeroship_core::config::validate_secret_ref("urn:zeroship:nope:x").is_err());
        // A well-formed reference and a plain literal both validate.
        assert!(zeroship_core::config::validate_secret_ref("urn:zeroship:env:MY_VAR").is_ok());
        assert!(zeroship_core::config::validate_secret_ref(&"00".repeat(32)).is_ok());
    }

    // (d) The `[secrets]` file tier. Control now obtains every secret via
    // `obtain_secret(label, cli, file_secrets.<field>.as_deref(), check_config)`.
    // This pins the two wiring guarantees that would regress if the file tier were
    // dropped or the precedence inverted:
    //   1. an empty CLI/env value falls back to the `[secrets]` file reference, and
    //   2. a present CLI/env value WINS over the file entry.
    // Resolution itself (env/file deref) lives in core; here we use a real env-backed
    // reference end-to-end so the fallback actually produces a value.
    #[test]
    fn secrets_file_tier_used_when_cli_empty_and_cli_wins_over_file() {
        // Serialize against any other env-touching test in this binary.
        static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_GUARD.lock().expect("env guard");

        let key = format!("ZEROSHIP_CONTROL_SECRETS_TIER_{}", std::process::id());
        let file_ref = format!("urn:zeroship:env:{key}");
        std::env::set_var(&key, "value-from-file-tier");

        // Mirror control's wiring exactly: a `SecretSection` carries the file ref.
        let file_secrets = zeroship_core::config::SecretSection {
            master_key: Some(file_ref.clone()),
            ..Default::default()
        };

        // (1) Empty CLI/env => the [secrets] file reference is consulted and resolved
        //     (real-boot path, check_config = false).
        let resolved = zeroship_core::config::obtain_secret(
            "MASTER_KEY / --master-key",
            "",
            file_secrets.master_key.as_deref(),
            false,
        );
        assert_eq!(
            resolved, "value-from-file-tier",
            "empty CLI must fall back to the [secrets] file reference"
        );

        // (2) A present CLI/env literal WINS over the file entry, byte-for-byte.
        let cli_literal = "00".repeat(32);
        let won = zeroship_core::config::obtain_secret(
            "MASTER_KEY / --master-key",
            &cli_literal,
            file_secrets.master_key.as_deref(),
            false,
        );
        assert_eq!(
            won, cli_literal,
            "a present CLI/env value must take precedence over the [secrets] file entry"
        );

        std::env::remove_var(&key);
    }
}
