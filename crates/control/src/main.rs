//! zeroship-control — control plane binary. Thin wrapper over
//! `zeroship_control` (the library crate).

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use ntex::web;
use zeroship_core::config::{
    bootstrap_or_exit, env_is_truthy, is_loopback_url, parse_bool_flag, require_unless_dev,
    resolve_overlay_string, validate_master_key_material, CheckConfigReport, CheckFormat,
    CheckValue, DEV_STASH_SIGNING_KEY,
};
use zeroship_bundle::{BlobStore, BundleStore, LocalDiskBlobStore, LocalFs};
use zeroship_control::{
    admin_handlers, api, backchannel_logout, bootstrap_builder, env_handlers, internal,
    oauth_grants_handlers, oauth_handlers, oidc_rp, stripe_handlers, token_handlers, AppState,
    EnvStore, Quota, RateLimiter, Registry, StripeStore,
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const DEV_HYDRA_PUBLIC_URL: &str = "http://localhost:4444";
const DEV_HYDRA_ADMIN_URL: &str = "http://localhost:4445";
const DEV_CONSOLE_OIDC_SECRET: &str = "dev-console-oidc-secret";

/// zeroship control-plane startup configuration.
#[derive(Parser)]
#[command(name = "zeroship-control")]
struct ControlCli {
    /// HTTP listen port.
    #[arg(long, env = "CONTROL_PORT", default_value_t = 9090)]
    port: u16,

    /// Address to bind. Defaults to loopback; pass 0.0.0.0 to expose across a network.
    #[arg(long, env = "CONTROL_BIND", default_value = "127.0.0.1")]
    bind: String,

    /// PostgreSQL DSN for control-plane data.
    #[arg(
        long = "db",
        env = "DATABASE_URL",
        default_value = "postgres://localhost/zeroship",
        hide_env_values = true
    )]
    db: String,

    /// Root directory for bundles and content-addressed deploy blobs.
    #[arg(long = "blob-store", env = "BLOB_STORE", default_value = "./bundles")]
    blob_store: String,

    /// Admin/control API shared secret.
    #[arg(long = "control-key", env = "CONTROL_KEY", default_value = "", hide_env_values = true)]
    control_key: String,

    /// Master key used for control-plane encrypted env/secrets.
    #[arg(long = "master-key", env = "MASTER_KEY", default_value = "", hide_env_values = true)]
    master_key: String,

    /// Comma-separated worker base URLs.
    #[arg(long = "workers", env = "WORKER_URLS", default_value = "http://localhost:8080")]
    workers: String,

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

    /// Comma-separated previous master keys accepted during key rotation.
    #[arg(
        long = "legacy-master-keys",
        env = "LEGACY_MASTER_KEYS",
        default_value = "",
        hide_env_values = true
    )]
    legacy_master_keys: String,

    /// Allow explicitly insecure local development startup.
    ///
    /// `--dev-insecure` / `--dev-insecure=true` enables; `--dev-insecure=false`
    /// disables (overriding a stray `ZEROSHIP_DEV_INSECURE=1` in the env).
    #[arg(
        long = "dev-insecure",
        env = "ZEROSHIP_DEV_INSECURE",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = parse_bool_flag
    )]
    dev_insecure: Option<bool>,

    /// Trust `X-Forwarded-For` from an upstream proxy.
    #[arg(
        long = "trust-proxy",
        env = "ZEROSHIP_TRUST_PROXY",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = parse_bool_flag
    )]
    trust_proxy: Option<bool>,

    /// Bootstrap the first-party builder OAuth client at startup.
    #[arg(long = "bootstrap-builder-client", action = clap::ArgAction::SetTrue)]
    bootstrap_builder_client: bool,

    /// Environment half of `--bootstrap-builder-client`; `1`/`true` are truthy.
    #[arg(skip = env_is_truthy("BOOTSTRAP_BUILDER_OAUTH_CLIENT"))]
    bootstrap_builder_client_env: bool,

    /// Redirect URI for the bootstrapped builder OAuth client.
    #[arg(
        long = "builder-redirect-uri",
        env = "BUILDER_REDIRECT_URI",
        default_value = bootstrap_builder::DEFAULT_BUILDER_REDIRECT_URI
    )]
    builder_redirect_uri: String,

    /// File path used to persist the bootstrapped builder client secret.
    #[arg(
        long = "builder-client-secret-file",
        env = "BUILDER_CLIENT_SECRET_FILE",
        default_value = bootstrap_builder::DEFAULT_BUILDER_CLIENT_SECRET_PATH
    )]
    builder_client_secret_file: PathBuf,

    /// Directory for in-flight deploy bodies; empty means the OS temp dir.
    #[arg(long = "deploy-tmp-dir", env = "DEPLOY_TMP_DIR", default_value = "")]
    deploy_tmp_dir: String,

    /// Optional shared config overlay path.
    #[arg(long = "config", env = "ZEROSHIP_CONFIG")]
    config_path: Option<PathBuf>,

    /// Disable auto-discovery of the well-known config overlay (compiled defaults only).
    #[arg(long = "no-config")]
    no_config: bool,

    /// Validate config (CLI + overlay + guards) and print the resolved non-secret config, then exit without starting the server.
    #[arg(long = "check-config")]
    check_config: bool,

    /// Output format for `--check-config`: `text` (default) or `json`.
    #[arg(long = "check-config-format", default_value = "text", value_parser = ["text", "json"])]
    check_config_format: String,

    /// Observability CLI/env overrides.
    #[command(flatten)]
    obs: zeroship_core::observability::ObservabilityFlags,

    /// Hydra admin API base URL.
    #[arg(long = "hydra-admin-url", env = "HYDRA_ADMIN_URL")]
    hydra_admin_url: Option<String>,

    /// Allow a non-loopback Hydra **admin** API URL. The admin API is
    /// privileged; outside `--dev-insecure` a remote admin URL is refused
    /// unless this is set.
    #[arg(
        long = "allow-remote-hydra-admin",
        env = "ALLOW_REMOTE_HYDRA_ADMIN",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = parse_bool_flag
    )]
    allow_remote_hydra_admin: Option<bool>,

    /// Hydra public issuer/base URL used by the console OIDC RP.
    #[arg(long = "hydra-public-url", env = "HYDRA_PUBLIC_URL")]
    hydra_public_url: Option<String>,

    /// Console OIDC client secret.
    #[arg(
        long = "console-oidc-secret",
        env = "CONSOLE_OIDC_SECRET",
        default_value = "",
        hide_env_values = true
    )]
    console_oidc_secret: String,

    /// HMAC key for short-lived OIDC stash cookies.
    #[arg(
        long = "stash-signing-key",
        env = "STASH_SIGNING_KEY",
        default_value = "",
        hide_env_values = true
    )]
    stash_signing_key: String,

    /// PostgreSQL DSN for auth/console session tables.
    #[arg(long = "auth-db", env = "AUTH_DB_URL", default_value = "", hide_env_values = true)]
    auth_db_url: String,

    /// Expected OAuth access-token audience for control bearer auth.
    #[arg(long = "oauth-audience", env = "OAUTH_AUDIENCE", default_value = "control.zeroship.ai")]
    oauth_audience: String,

    /// Apex domain hosted creator apps serve under. An app named `myapp`
    /// serves at `myapp.{app_base_domain}`; the per-app OAuth client's
    /// redirect_uris + sector_identifier are derived from that apex host
    /// (Slice 1d, §1.1). Defaults to the prod apex; dev/compose set
    /// `zeroship.localhost`.
    #[arg(long = "app-base-domain", env = "APP_BASE_DOMAIN", default_value = "zeroship.ai")]
    app_base_domain: String,
}

impl ControlCli {
    fn bootstrap_builder_client(&self) -> bool {
        self.bootstrap_builder_client || self.bootstrap_builder_client_env
    }
}

// S2: hand-written `Debug` that redacts every raw-secret field. The derive is
// intentionally dropped so a stray `{:?}` (e.g. in a clap parse error or a test
// `.unwrap_err()`) can never echo a DSN, master key, control key, worker key,
// stash key, Stripe secret, console OIDC secret, or legacy master keys.
impl std::fmt::Debug for ControlCli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlCli")
            .field("port", &self.port)
            .field("bind", &self.bind)
            .field("db", &"<redacted>")
            .field("blob_store", &self.blob_store)
            .field("control_key", &"<redacted>")
            .field("master_key", &"<redacted>")
            .field("workers", &self.workers)
            .field("worker_key", &"<redacted>")
            .field("signing_key_file", &self.signing_key_file)
            .field("stripe_webhook_secret", &"<redacted>")
            .field("legacy_master_keys", &"<redacted>")
            .field("dev_insecure", &self.dev_insecure)
            .field("trust_proxy", &self.trust_proxy)
            .field("bootstrap_builder_client", &self.bootstrap_builder_client)
            .field("bootstrap_builder_client_env", &self.bootstrap_builder_client_env)
            .field("builder_redirect_uri", &self.builder_redirect_uri)
            .field("builder_client_secret_file", &self.builder_client_secret_file)
            .field("deploy_tmp_dir", &self.deploy_tmp_dir)
            .field("config_path", &self.config_path)
            .field("no_config", &self.no_config)
            .field("check_config", &self.check_config)
            .field("check_config_format", &self.check_config_format)
            .field("obs", &self.obs)
            .field("hydra_admin_url", &self.hydra_admin_url)
            .field("allow_remote_hydra_admin", &self.allow_remote_hydra_admin)
            .field("hydra_public_url", &self.hydra_public_url)
            .field("console_oidc_secret", &"<redacted>")
            .field("stash_signing_key", &"<redacted>")
            .field("auth_db_url", &"<redacted>")
            .field("oauth_audience", &self.oauth_audience)
            .field("app_base_domain", &self.app_base_domain)
            .finish()
    }
}

fn main() -> std::io::Result<()> {
    let cli = ControlCli::parse();
    let boot = bootstrap_or_exit(
        cli.config_path.as_deref(),
        !cli.no_config,
        &cli.obs,
        "info,zeroship_control=debug",
        "control",
    );
    let file = &boot.overlay.config;
    // `[secrets]` file-overlay tier for the secret-reference resolver. Cloned once
    // up front so individual `obtain_secret` calls can borrow the per-field refs
    // (CLI/env > [secrets] file ref > default) without re-borrowing `boot`.
    let file_secrets = boot.overlay.config.secrets.clone();
    let filter = &boot.log_filter;

    // CLI presence overrides env, so `--dev-insecure=false` disables a stray
    // `ZEROSHIP_DEV_INSECURE=1`.
    let insecure_dev = cli.dev_insecure.unwrap_or(false);
    let trust_proxy = cli.trust_proxy.unwrap_or(false);
    let allow_remote_hydra_admin = cli.allow_remote_hydra_admin.unwrap_or(false);
    let bootstrap_builder_client = cli.bootstrap_builder_client();

    let hydra_admin_url = resolve_overlay_string(
        cli.hydra_admin_url,
        file.auth.hydra_admin_url.clone(),
        insecure_dev.then_some(DEV_HYDRA_ADMIN_URL),
    );
    let hydra_public_url = resolve_overlay_string(
        cli.hydra_public_url,
        file.auth.hydra_public_url.clone(),
        insecure_dev.then_some(DEV_HYDRA_PUBLIC_URL),
    );
    let trusted_oauth_clients = zeroship_control::resolve_trusted_oauth_clients(&file.auth);
    tracing::info!(
        trusted_oauth_clients = trusted_oauth_clients.len(),
        "control: trusted OAuth client set resolved"
    );

    // S8: control consumes the privileged Hydra ADMIN API. A non-loopback admin
    // URL is refused unless explicitly opted in via --allow-remote-hydra-admin —
    // literal loopback only (no DNS), closing the rebind/TOCTOU window. Dev mode
    // does NOT bypass this (matching auth): allowing a privileged remote admin
    // endpoint is its own deliberate opt-in, separate from --dev-insecure.
    if !hydra_admin_url.is_empty()
        && !allow_remote_hydra_admin
        && !is_loopback_url(&hydra_admin_url)
    {
        eprintln!(
            "control: refusing to start; HYDRA_ADMIN_URL ({hydra_admin_url}) is not a loopback \
             address. The Hydra admin API is privileged — pass --allow-remote-hydra-admin \
             (or ALLOW_REMOTE_HYDRA_ADMIN=1) to use a remote admin endpoint."
        );
        tracing::error!(
            hydra_admin_url = %hydra_admin_url,
            "control: refusing to start with non-loopback Hydra admin URL"
        );
        std::process::exit(1);
    }

    let port = cli.port;
    let bind_host = cli.bind;
    // Secret-bearing inputs (the fields `ControlCli::Debug` redacts) are resolved
    // through the shared secret-reference resolver. On the real boot path a
    // `urn:zeroship:{env,file,...}` / `arn:aws:secretsmanager:...` reference is
    // dereferenced to its value; under `--check-config` only the reference FORMAT
    // is validated (no env/file/network side effects) and the raw ref string is
    // kept for the read-only report. A literal secret passes through byte-for-byte
    // in both modes. DSN fields (`--db`, `--auth-db`) carry passwords, so they go
    // through the same path. Pure file-PATH fields (`--signing-key-file`,
    // `--builder-client-secret-file`) name a file to read and are NOT resolved here.
    let db_url = zeroship_core::config::obtain_secret(
        "DATABASE_URL / --db",
        &cli.db,
        file_secrets.database_url.as_deref(),
        cli.check_config,
    );
    let blob_store_root = cli.blob_store;
    let control_key = zeroship_core::config::obtain_secret(
        "CONTROL_KEY / --control-key",
        &cli.control_key,
        file_secrets.control_key.as_deref(),
        cli.check_config,
    );
    let master_key = zeroship_core::config::obtain_secret(
        "MASTER_KEY / --master-key",
        &cli.master_key,
        file_secrets.master_key.as_deref(),
        cli.check_config,
    );
    let workers_str = cli.workers;
    let worker_key = zeroship_core::config::obtain_secret(
        "WORKER_KEY / --worker-key",
        &cli.worker_key,
        file_secrets.worker_key.as_deref(),
        cli.check_config,
    );
    let signing_key_file = cli.signing_key_file;
    let stripe_webhook_secret = zeroship_core::config::obtain_secret(
        "STRIPE_WEBHOOK_SECRET / --stripe-webhook-secret",
        &cli.stripe_webhook_secret,
        file_secrets.stripe_webhook_secret.as_deref(),
        cli.check_config,
    );
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
            cli.check_config,
        );
        // In --check-config the file reference is only format-validated (csv is then
        // the raw ref, which must not be split); split only a resolved/literal value.
        if cli.check_config && zeroship_core::config::is_secret_ref(&csv) {
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
                if cli.check_config {
                    zeroship_core::config::validate_secret_ref_or_exit(legacy_label, entry);
                    entry.to_owned()
                } else {
                    zeroship_core::config::resolve_secret_or_exit(legacy_label, entry)
                }
            })
            .collect()
    };
    let builder_redirect_uri = cli.builder_redirect_uri;
    let builder_client_secret_path = cli.builder_client_secret_file;
    let deploy_tmp_dir_str = cli.deploy_tmp_dir;
    let console_oidc_secret = zeroship_core::config::obtain_secret(
        "CONSOLE_OIDC_SECRET / --console-oidc-secret",
        &cli.console_oidc_secret,
        file_secrets.console_oidc_secret.as_deref(),
        cli.check_config,
    );
    let stash_signing_key = zeroship_core::config::obtain_secret(
        "STASH_SIGNING_KEY / --stash-signing-key",
        &cli.stash_signing_key,
        file_secrets.stash_signing_key.as_deref(),
        cli.check_config,
    );
    let auth_db_url = zeroship_core::config::obtain_secret(
        "AUTH_DB_URL / --auth-db",
        &cli.auth_db_url,
        file_secrets.auth_db_url.as_deref(),
        cli.check_config,
    );
    let expected_oauth_audience = cli.oauth_audience;
    let app_base_domain = cli.app_base_domain;

    // Pure path resolution only — the writability PROBE (create_dir_all + probe
    // file) is deferred to the real startup path (M1) so `--check-config`
    // performs NO filesystem mutation but can still report the resolved path.
    let deploy_tmp_dir: std::path::PathBuf = if deploy_tmp_dir_str.is_empty() {
        std::env::temp_dir()
    } else {
        std::path::PathBuf::from(&deploy_tmp_dir_str)
    };

    // S3: control authenticates the worker admin log fan-out with WORKER_KEY.
    // Enforce its presence outside dev (worker already refuses a non-loopback
    // bind without it; this guards the caller side symmetrically).
    if let Err(message) =
        require_unless_dev("WORKER_KEY / --worker-key", &worker_key, insecure_dev)
    {
        eprintln!("control: {message}");
        tracing::error!(error = %message, "control: refusing to start without WORKER_KEY");
        std::process::exit(1);
    }

    if !insecure_dev {
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
                "control: refusing to start; required secrets missing. \
                 Pass --dev-insecure (or ZEROSHIP_DEV_INSECURE=1) to run \
                 without them — NEVER in production."
            );
            std::process::exit(1);
        }
        // Strength guards run on the RESOLVED value at real boot. During
        // `--check-config` a secret REFERENCE is still the raw `urn:`/`arn:`
        // string (not yet dereferenced), so skip the strength check for a ref —
        // it would wrongly fail length/entropy on the reference text. A literal
        // is checked in both modes.
        if !cli.check_config || !zeroship_core::config::is_secret_ref(&master_key) {
            if let Err(message) =
                validate_master_key_material("MASTER_KEY", &master_key, insecure_dev)
            {
                tracing::error!(error = %message, "control: refusing to start with weak MASTER_KEY");
                std::process::exit(1);
            }
        }
        for (idx, legacy_key) in legacy_keys.iter().enumerate() {
            let label = format!("LEGACY_MASTER_KEYS[{idx}]");
            if !cli.check_config || !zeroship_core::config::is_secret_ref(legacy_key) {
                if let Err(message) =
                    validate_master_key_material(&label, legacy_key, insecure_dev)
                {
                    tracing::error!(
                        error = %message,
                        "control: refusing to start with weak legacy master key"
                    );
                    std::process::exit(1);
                }
            }
        }
        if stripe_webhook_secret.is_empty() {
            // Not fatal — operators may run a control plane without
            // Stripe entirely. But every webhook delivery will reject
            // with 500, so log loudly at startup so a misconfigured
            // deploy isn't noticed only via Stripe-side retries.
            tracing::warn!(
                "control: stripe_webhook_secret unset — /internal/webhooks/stripe will reject every request. \
                 Set --stripe-webhook-secret if you need Stripe integration."
            );
        }
    }
    if insecure_dev {
        tracing::warn!("control: --dev-insecure set; admin + internal auth disabled");
    }

    // Phase 3 U7/U8 — control plane OIDC RP for `console.zeroship.ai`.
    // Mandatory post-U8: the legacy `auth_handlers` / `auth_service`
    // chain has been retired, so the OIDC RP is the only console-auth
    // surface. Control also needs hydra-admin for OAuth bearer
    // introspection. Refuses to boot unless these pieces are configured
    // (`--dev-insecure` permits localhost defaults only).
    if !insecure_dev {
        let mut missing = Vec::new();
        if hydra_public_url.is_empty() {
            missing.push("--hydra-public-url / HYDRA_PUBLIC_URL");
        }
        if console_oidc_secret.is_empty() {
            missing.push("--console-oidc-secret / CONSOLE_OIDC_SECRET");
        }
        if stash_signing_key.is_empty() {
            missing.push("--stash-signing-key / STASH_SIGNING_KEY");
        }
        if auth_db_url.is_empty() {
            missing.push("--auth-db / AUTH_DB_URL");
        }
        if hydra_admin_url.is_empty() {
            missing.push("--hydra-admin-url / HYDRA_ADMIN_URL");
        }
        if !missing.is_empty() {
            tracing::error!(
                missing = %missing.join(", "),
                "control: refusing to start; OIDC RP and OAuth introspection require these flags. \
                 Pass --dev-insecure to run with localhost defaults."
            );
            std::process::exit(1);
        }

        // D5: control adopts the shared stash-key strength check (gateway
        // already had it). Empty is reported by the missing-secrets block
        // above; this additionally rejects a present-but-weak key (<32
        // bytes), which also catches the dev sentinel (27 bytes) in prod.
        // Skipped for a secret REFERENCE under --check-config (the local is
        // then the raw ref string, which would wrongly fail the length check);
        // it runs on the resolved value at real boot.
        if !cli.check_config || !zeroship_core::config::is_secret_ref(&stash_signing_key) {
            if let Err(message) =
                zeroship_core::config::validate_stash_key(&stash_signing_key, insecure_dev)
            {
                tracing::error!(error = %message, "control: refusing to start with weak STASH_SIGNING_KEY");
                std::process::exit(1);
            }
        }
    }

    if cli.check_config {
        // M1: read-only. No filesystem mutation, no signing-key load.
        let workers_count = workers_str
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .count();
        let log_format_str = boot
            .log_format
            .map_or_else(|| "auto".to_string(), |fmt| fmt.to_string());

        let mut report = CheckConfigReport::new();
        report.field("port", CheckValue::Count(usize::from(port)));
        report.field("bind", CheckValue::Plain(bind_host.clone()));
        report.field(
            "config_source",
            CheckValue::Plain(boot.overlay.source.to_string()),
        );
        report.field("hydra_admin_url", CheckValue::Plain(hydra_admin_url.clone()));
        report.field(
            "hydra_public_url",
            CheckValue::Plain(hydra_public_url.clone()),
        );
        report.field(
            "trusted_oauth_clients_count",
            CheckValue::Count(trusted_oauth_clients.len()),
        );
        report.field("log_filter", CheckValue::Plain(filter.clone()));
        report.field("log_format", CheckValue::Plain(log_format_str));
        report.field("insecure_dev", CheckValue::Flag(insecure_dev));
        report.field("trust_proxy", CheckValue::Flag(trust_proxy));
        report.field(
            "bootstrap_builder_client",
            CheckValue::Flag(bootstrap_builder_client),
        );
        report.field("blob_store", CheckValue::Plain(blob_store_root.clone()));
        report.field(
            "deploy_tmp_dir",
            CheckValue::Plain(deploy_tmp_dir.display().to_string()),
        );
        report.field(
            "auth_db_configured",
            CheckValue::Flag(!auth_db_url.is_empty()),
        );
        report.field("workers_count", CheckValue::Count(workers_count));

        let fmt = if cli.check_config_format == "json" {
            CheckFormat::Json
        } else {
            CheckFormat::Text
        };
        report.emit(fmt);
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

    let pat_issuer = if signing_key_file.is_empty() {
        tracing::warn!("control: using dev-only PAT signing key");
        Arc::new(token_handlers::PatIssuer::dev_insecure())
    } else {
        let signing_key = token_handlers::load_signing_key_from_path(
            std::path::Path::new(&signing_key_file),
        )
        .map_err(|err| {
            tracing::error!(error = %err, "control: failed to load PAT signing key");
            std::io::Error::new(std::io::ErrorKind::InvalidInput, err)
        })?;
        Arc::new(token_handlers::PatIssuer::new(&signing_key).map_err(|err| {
            tracing::error!(error = %err, "control: failed to initialize PAT issuer");
            std::io::Error::new(std::io::ErrorKind::InvalidInput, err)
        })?)
    };

    ntex::rt::System::build()
        .name("zeroship-control")
        .build(ntex::rt::DefaultRuntime)
        .block_on(async move {
    let registry = Registry::new(&db_url)
        .await
        .expect("failed to connect to database");

    let vfs = Arc::new(
        LocalFs::new(&blob_store_root).expect("failed to initialise bundle store"),
    ) as Arc<dyn BundleStore + Send + Sync>;

    // BlobStore lives alongside the legacy BundleStore on the same
    // root. New `.zship` deploys land in `<blob_store_root>/blobs/` and
    // `<blob_store_root>/manifests/`; legacy `<blob_store_root>/<app_id>/...`
    // files stay where they are until the old BundleStore path is
    // retired.
    let blob_root = PathBuf::from(&blob_store_root);
    let blob_store: Arc<dyn BlobStore> = Arc::new(
        LocalDiskBlobStore::new(blob_root)
            .expect("failed to initialise blob store"),
    );

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
        insecure_dev,
    )
    .expect("env store init");
    let stripe_store = StripeStore::new(registry.clone());

    let stash_key_bytes = if stash_signing_key.is_empty() {
        // Dev-only fallback. Ephemeral keys are fine for the 10-minute
        // stash window during local development; in prod the guard
        // above already exited.
        DEV_STASH_SIGNING_KEY.as_bytes().to_vec()
    } else {
        stash_signing_key.into_bytes()
    };
    let console_oidc_secret_value = if console_oidc_secret.is_empty() {
        DEV_CONSOLE_OIDC_SECRET.to_string()
    } else {
        console_oidc_secret
    };
    tracing::info!(
        hydra_public_url = %hydra_public_url,
        "control: console OIDC RP enabled"
    );
    let oidc_rp = Arc::new(oidc_rp::ConsoleOidcRp::new(
        &hydra_public_url,
        "console.zeroship.ai",
        console_oidc_secret_value,
        stash_key_bytes,
    ));
    let hydra_introspector = Arc::new(zeroship_core::hydra::HydraIntrospector::new(
        &hydra_admin_url,
    ));

    let auth_db_url_resolved = if auth_db_url.is_empty() {
        // Dev fallback: reuse the control DB URL so /auth/callback
        // works against a single local Postgres without operator
        // ceremony. Production refused to start without --auth-db
        // above.
        db_url.clone()
    } else {
        auth_db_url
    };
    let auth_pg: Arc<compio_postgres::Client> = {
        let (pg_client, pg_conn) = compio_postgres::connect(&auth_db_url_resolved, compio_postgres::NoTls)
            .await
            .expect("control: auth-pg connect");
        compio::runtime::spawn(async move {
            if let Err(e) = pg_conn.run().await {
                tracing::error!(error = %e, "control/auth-pg connection ended");
            }
        })
        .detach();
        Arc::new(pg_client)
    };

    if bootstrap_builder_client {
        // Schema (incl. the control.* authz/oauth tables) is owned by Liquibase
        // (db/changelog), applied out of band before boot — not here.
        let cfg = bootstrap_builder::BuilderClientBootstrapConfig {
            enabled: true,
            hydra_admin_url: hydra_admin_url.clone(),
            redirect_uri: builder_redirect_uri,
            client_secret_path: builder_client_secret_path,
            skip_consent: trusted_oauth_clients
                .contains(bootstrap_builder::BUILDER_CLIENT_ID),
        };
        bootstrap_builder::bootstrap_builder_oauth_client(&auth_pg, &cfg)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "control: builder OAuth client bootstrap failed");
                std::io::Error::other(err.to_string())
            })?;
    }

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        vfs,
        blob_store,
        control_key: zeroship_control::SecretString::new(control_key),
        master_key: zeroship_control::SecretString::new(master_key),
        stripe_webhook_secret: zeroship_control::SecretString::new(stripe_webhook_secret),
        worker_urls: workers_str
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        worker_key: zeroship_control::SecretString::new(worker_key),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(30, 60))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(50, 600))),
        insecure_dev,
        trust_proxy,
        deploy_tmp_dir,
        oidc_rp,
        auth_pg,
        auth_db_url: auth_db_url_resolved,
        hydra_admin_url,
        app_base_domain,
        trusted_oauth_clients,
        expected_oauth_audience,
        static_policies: zeroship_authz::load_platform_policies()
            .expect("control: bundled authz policies parse"),
        pat_issuer,
        hydra_introspector,
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
    });

    let bind_addr = format!("{bind_host}:{port}");
    if insecure_dev && bind_host != "127.0.0.1" && bind_host != "::1" && bind_host != "localhost" {
        tracing::warn!(
            bind = %bind_addr,
            "control: binding a non-loopback address under --dev-insecure (admin + internal \
             auth disabled) — do NOT expose this on an untrusted network"
        );
    }
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
            // --- Auth (creator console) ---
            // OIDC RP callback for the `console.zeroship.ai` client.
            // The legacy `/auth/{login,register,logout,userinfo,consent,
            // authorize,google/*}` handlers were retired in P3-U8 along
            // with `auth_service` / `auth_handlers`; this is now the
            // only console-auth surface.
            .service(web::resource("/auth/callback").route(web::get().to(api::auth_callback)))
            .configure(token_handlers::configure)
            .configure(oauth_grants_handlers::configure)
            // OIDC Back-Channel Logout 1.0 RP endpoint. Hydra POSTs
            // here on user sign-out; we verify the logout_token and
            // revoke the user's console sessions. The URI must match
            // `backchannel_logout_uri` on the `console.zeroship.ai`
            // client in `ops/auth-clients.example.toml`. Mounted via
            // `.configure(...)` to mirror the gateway pattern.
            .configure(backchannel_logout::configure)
            // --- Stripe Connect ---
            .service(
                web::resource("/api/creators/{id}/stripe/onboard")
                    .route(web::post().to(stripe_handlers::onboard)),
            )
            .service(
                web::resource("/api/creators/{id}/stripe/callback")
                    .route(web::post().to(stripe_handlers::callback)),
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
            .service(
                web::resource("/internal/usage")
                    .route(web::post().to(internal::report_usage)),
            )
            .service(
                web::resource("/internal/webhooks/stripe")
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

    #[test]
    fn control_blob_store_flag_uses_unified_name() {
        let cli =
            ControlCli::try_parse_from(["zeroship-control", "--blob-store", "/tmp/blob-root"])
                .expect("blob-store flag should parse");

        assert_eq!(cli.blob_store, "/tmp/blob-root");
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

    // Master-key strength logic now lives in `zeroship_core::config::secrets`
    // (fully tested there). These two assert control still calls *through* to
    // the shared validator with the expected outcomes.
    #[test]
    fn master_key_accepts_32_byte_hex_in_non_dev() {
        let key = "00".repeat(32);
        assert!(validate_master_key_material("MASTER_KEY", &key, false).is_ok());
    }

    #[test]
    fn master_key_allows_dev_shortcut_in_insecure_dev() {
        assert!(validate_master_key_material("MASTER_KEY", "password", true).is_ok());
    }

    // S1: a stray `ZEROSHIP_DEV_INSECURE=1` in the environment MUST be
    // overridable from the CLI. `--dev-insecure=false` resolves to false,
    // while env=1 alone (no CLI flag) enables.
    #[test]
    fn dev_insecure_cli_false_overrides_env_one() {
        // Serialise env mutation across the two env-touching tests.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().expect("env lock");
        let old = std::env::var_os("ZEROSHIP_DEV_INSECURE");
        std::env::set_var("ZEROSHIP_DEV_INSECURE", "1");

        // env=1 alone (no CLI flag) enables.
        let cli = ControlCli::try_parse_from(["zeroship-control"]).expect("parse with env only");
        assert_eq!(
            cli.dev_insecure,
            Some(true),
            "ZEROSHIP_DEV_INSECURE=1 should enable"
        );
        assert!(cli.dev_insecure.unwrap_or(false));

        // explicit CLI false beats the stray env=1.
        let cli = ControlCli::try_parse_from(["zeroship-control", "--dev-insecure=false"])
            .expect("parse with explicit false");
        let insecure_dev = cli.dev_insecure.unwrap_or(false);
        assert!(!insecure_dev, "CLI --dev-insecure=false must beat env=1");

        match old {
            Some(value) => std::env::set_var("ZEROSHIP_DEV_INSECURE", value),
            None => std::env::remove_var("ZEROSHIP_DEV_INSECURE"),
        }
    }

    // S3: a missing WORKER_KEY is fatal outside dev, allowed inside dev.
    #[test]
    fn missing_worker_key_is_fatal_outside_dev() {
        // Outside dev: empty worker key rejected.
        assert!(require_unless_dev("WORKER_KEY / --worker-key", "", false).is_err());
        // Inside dev: allowed.
        assert!(require_unless_dev("WORKER_KEY / --worker-key", "", true).is_ok());
        // Present: allowed even outside dev.
        assert!(require_unless_dev("WORKER_KEY / --worker-key", "k", false).is_ok());
    }

    // S8: a non-loopback Hydra admin URL is rejected outside dev unless
    // --allow-remote-hydra-admin is set. This mirrors the guard in `main`.
    fn hydra_admin_guard_rejects(hydra_admin_url: &str, allow_remote: bool) -> bool {
        !hydra_admin_url.is_empty() && !allow_remote && !is_loopback_url(hydra_admin_url)
    }

    #[test]
    fn non_loopback_hydra_admin_rejected_without_opt_in() {
        // Remote admin URL, no opt-in -> rejected. Dev mode does NOT bypass this:
        // allowing a privileged remote admin endpoint is its own explicit opt-in.
        assert!(hydra_admin_guard_rejects("http://hydra:4445", false));
        // Loopback admin URL -> allowed.
        assert!(!hydra_admin_guard_rejects("http://127.0.0.1:4445", false));
        assert!(!hydra_admin_guard_rejects("http://localhost:4445", false));
        // Remote admin URL with explicit --allow-remote-hydra-admin -> allowed.
        assert!(!hydra_admin_guard_rejects("http://hydra:4445", true));
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
