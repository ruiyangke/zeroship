//! zeroship-control — control plane binary. Thin wrapper over
//! `zeroship_control` (the library crate).

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use ntex::web;
use zeroship_core::config::{
    bootstrap_or_exit, env_is_truthy, is_loopback_url, parse_bool_flag, require_unless_dev,
    resolve_overlay_string, validate_master_key_material, CheckConfigReport, CheckFormat,
    CheckValue,
};
use zeroship_bundle::{build_blob_store, BlobStore, StoreUrl};
use zeroship_control::{
    admin_handlers, api, bootstrap_console, env_handlers,
    internal, oauth_grants_handlers, oauth_handlers, stripe_handlers, token_handlers,
    AppState, EnvStore, Quota, RateLimiter, Registry, StripeStore,
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const DEV_HYDRA_PUBLIC_URL: &str = "http://localhost:4444";
const DEV_HYDRA_ADMIN_URL: &str = "http://localhost:4445";

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

    /// Stripe secret API key (`sk_…`) for OUTBOUND calls (the billing
    /// reconciler + `billing/setup`). Required in prod; empty allowed only
    /// under `--dev-insecure`.
    #[arg(
        long = "stripe-secret-key",
        env = "STRIPE_SECRET_KEY",
        default_value = "",
        hide_env_values = true
    )]
    stripe_secret_key: String,

    /// Stripe REST API base URL the outbound client targets. Defaults to
    /// `https://api.stripe.com`; override only for testing against a mock.
    #[arg(
        long = "stripe-base-url",
        env = "STRIPE_BASE_URL",
        default_value = "https://api.stripe.com"
    )]
    stripe_base_url: String,

    /// Metering/billing provider backend (M6). `native` (default) runs the
    /// control-side aggregation → CU×FX → Stripe reconciler. `stripe` (Stripe
    /// Billing Meters — CU → meter_events, Stripe self-invoices) and `openmeter`
    /// (CU → CloudEvents, export-only — OpenMeter aggregates, invoicing stays
    /// Native) are export backends; each refuses to boot without its creds.
    #[arg(
        long = "metering-provider",
        env = "METERING_PROVIDER",
        default_value = "native"
    )]
    metering_provider: String,

    /// Tax provider backend (billing-ops gap #26, PR-5). `native` (default)
    /// computes `0` — the USD launch owes no tax. The seam exists so enabling a
    /// real `StripeTaxProvider` (Stripe `automatic_tax`) later is a provider swap,
    /// not a schema change (`invoices.tax_cents` already exists).
    #[arg(long = "tax-provider", env = "TAX_PROVIDER", default_value = "native")]
    tax_provider: String,

    /// Stripe **Billing Meter** event name (M-Stripe). REQUIRED when
    /// `--metering-provider stripe` (else the deployment refuses to boot — a
    /// Stripe-Meters deployment with no meter is a silent revenue black hole).
    /// This is the operator-provisioned Meter's configured `event_name` (e.g.
    /// `compute_units`); the export cron pushes CU as `meter_events` against it.
    #[arg(
        long = "stripe-meter-event-name",
        env = "STRIPE_METER_EVENT_NAME",
        default_value = ""
    )]
    stripe_meter_event_name: String,

    /// Stripe **Billing Meter id** (`mtr_…`) (M-Stripe). REQUIRED when
    /// `--metering-provider stripe`. The export cron reads the meter's
    /// AGGREGATED value for `(customer, period)` via this id to reconcile a
    /// crash-then-re-drive push past Stripe's ~24h `identifier` dedup window
    /// (C2) — without it a >24h re-drive could double-bill, so the deployment
    /// refuses to boot.
    #[arg(
        long = "stripe-meter-id",
        env = "STRIPE_METER_ID",
        default_value = ""
    )]
    stripe_meter_id: String,

    /// OpenMeter base URL (M-OpenMeter). REQUIRED when `--metering-provider
    /// openmeter` (else the deployment refuses to boot — an OpenMeter deployment
    /// with no endpoint would push CU nowhere). `https://openmeter.cloud` or a
    /// self-hosted deployment. The export cron POSTs CloudEvents to
    /// `{url}/api/v1/events`.
    #[arg(
        long = "openmeter-url",
        env = "OPENMETER_URL",
        default_value = ""
    )]
    openmeter_url: String,

    /// OpenMeter API token (Bearer) (M-OpenMeter). REQUIRED when
    /// `--metering-provider openmeter`. Never logged.
    #[arg(
        long = "openmeter-token",
        env = "OPENMETER_TOKEN",
        default_value = "",
        hide_env_values = true
    )]
    openmeter_token: String,

    /// OpenMeter CloudEvent `type` = the operator-provisioned meter's `eventType`
    /// (M-OpenMeter, e.g. `compute_units`). The export cron pushes CU as
    /// CloudEvents of this `type`.
    #[arg(
        long = "openmeter-event-type",
        env = "OPENMETER_EVENT_TYPE",
        default_value = "compute_units"
    )]
    openmeter_event_type: String,

    /// OpenMeter meter **slug** (M-OpenMeter). REQUIRED when
    /// `--metering-provider openmeter`. The export cron reads the meter's
    /// AGGREGATED value for `(subject, period)` via this slug to reconcile a
    /// crash-then-re-drive push past OpenMeter's dedup window (C2) — without it a
    /// >24h re-drive could double-count, so the deployment refuses to boot.
    #[arg(
        long = "openmeter-meter-slug",
        env = "OPENMETER_METER_SLUG",
        default_value = ""
    )]
    openmeter_meter_slug: String,

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

    // NOTE: the `--bootstrap-builder-client` / `--builder-redirect-uri` /
    // `--builder-client-secret-file` flags were retired in the R5 cutover. The
    // separate `zeroship-builder` Vite service (and its confidential OAuth
    // client) is gone — the AI app-builder IS the console, seeded by
    // `--bootstrap-console` as a public-PKCE gateway-fronted app.

    /// Seed the console (`apps/zeroship-builder`) as a platform-owned regular
    /// app at startup: upsert its `control.apps` row (enterprise plan), its
    /// public-PKCE OAuth client (explicit `sector_identifier` = console host),
    /// ingest the prebuilt `.zship`, and forward the console's sandbox runtime
    /// env (`OPENAI_API_KEY`, `SANDBOX_*`, `ZEROSHIP_SDK_REGISTRY`). The console
    /// is a pure creator app — it holds no control credential, so the seed mints
    /// no PAT. In-process + idempotent; NEVER an HTTP route. Off by default.
    #[arg(long = "bootstrap-console", action = clap::ArgAction::SetTrue)]
    bootstrap_console: bool,

    /// Environment half of `--bootstrap-console`; `1`/`true` are truthy.
    #[arg(skip = env_is_truthy("BOOTSTRAP_CONSOLE"))]
    bootstrap_console_env: bool,

    /// Path to the prebuilt console `.zship` ingested by `--bootstrap-console`.
    #[arg(
        long = "console-zship",
        env = "CONSOLE_ZSHIP",
        default_value = bootstrap_console::DEFAULT_CONSOLE_ZSHIP
    )]
    console_zship: PathBuf,

    /// The console host (explicit OAuth `sector_identifier`) seeded by
    /// `--bootstrap-console`. Defaults to the dev host under `--dev-insecure`
    /// and the prod host otherwise (resolved in `main`).
    #[arg(long = "console-host", env = "CONSOLE_HOST")]
    console_host: Option<String>,

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

    /// HMAC key for short-lived OIDC stash cookies.
    #[arg(
        long = "stash-signing-key",
        env = "STASH_SIGNING_KEY",
        default_value = "",
        hide_env_values = true
    )]
    stash_signing_key: String,

    /// Dedicated PERMANENT pairwise-salt secret (value). The seed for every
    /// app's `pws_` per-app identity anchor (auth-sdk §6.2) — independent of
    /// the rotatable stash key. MUST be identical to the gateway's value and
    /// MUST NOT be rotated without a per-app `pws_` migration. Prefer
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

    /// Retention horizon (months) for the append-only audit tables
    /// `zeroship.app_audit` + `zeroship.authz_decisions`. Rows older than this
    /// are swept by the in-process retention cron — the sanctioned deleter for
    /// those tables (peer of the auth `audit_events` sweep). Default 12 months
    /// matches the events-retention default.
    #[arg(
        long = "audit-retention-months",
        env = "CONTROL_AUDIT_RETENTION_MONTHS",
        default_value_t = zeroship_control::cron::audit_retention::DEFAULT_RETENTION_MONTHS
    )]
    audit_retention_months: u32,

    /// Tick interval (seconds) for the audit-retention cron. Default 3600
    /// (hourly). Operators can drop this for tests; production should leave the
    /// default.
    #[arg(
        long = "audit-retention-check-secs",
        env = "CONTROL_AUDIT_RETENTION_CHECK_SECS",
        default_value_t = zeroship_control::cron::audit_retention::DEFAULT_CHECK_SECS
    )]
    audit_retention_check_secs: u64,
}

impl ControlCli {
    fn bootstrap_console(&self) -> bool {
        self.bootstrap_console || self.bootstrap_console_env
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
            .field("bootstrap_console", &self.bootstrap_console)
            .field("bootstrap_console_env", &self.bootstrap_console_env)
            .field("console_zship", &self.console_zship)
            .field("console_host", &self.console_host)
            .field("deploy_tmp_dir", &self.deploy_tmp_dir)
            .field("config_path", &self.config_path)
            .field("no_config", &self.no_config)
            .field("check_config", &self.check_config)
            .field("check_config_format", &self.check_config_format)
            .field("obs", &self.obs)
            .field("hydra_admin_url", &self.hydra_admin_url)
            .field("allow_remote_hydra_admin", &self.allow_remote_hydra_admin)
            .field("hydra_public_url", &self.hydra_public_url)
            .field("stash_signing_key", &"<redacted>")
            .field("pairwise_salt", &"<redacted>")
            .field("pairwise_salt_file", &self.pairwise_salt_file)
            .field("oauth_audience", &self.oauth_audience)
            .field("app_base_domain", &self.app_base_domain)
            .field("audit_retention_months", &self.audit_retention_months)
            .field("audit_retention_check_secs", &self.audit_retention_check_secs)
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
    let bootstrap_console = cli.bootstrap_console();

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
    // in both modes. The DSN field (`--db`) carries a password, so it goes
    // through the same path. Pure file-PATH fields (`--signing-key-file`,
    // `--builder-client-secret-file`) name a file to read and are NOT resolved here.
    let db_url = zeroship_core::config::obtain_secret(
        "DATABASE_URL / --db",
        &cli.db,
        file_secrets.database_url.as_deref(),
        cli.check_config,
    );
    let blob_store_root = cli.blob_store;
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
    let stripe_secret_key = zeroship_core::config::obtain_secret(
        "STRIPE_SECRET_KEY / --stripe-secret-key",
        &cli.stripe_secret_key,
        file_secrets.stripe_secret_key.as_deref(),
        cli.check_config,
    );
    let stripe_base_url = cli.stripe_base_url.clone();
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
    let console_zship = cli.console_zship;
    // The console host is the explicit OAuth sector_identifier. Default to the
    // dev host under --dev-insecure (compose / *.zeroship.localhost) and the
    // prod host otherwise; an explicit --console-host / CONSOLE_HOST wins.
    let console_host = cli.console_host.unwrap_or_else(|| {
        if insecure_dev {
            bootstrap_console::DEV_CONSOLE_HOST.to_string()
        } else {
            bootstrap_console::PROD_CONSOLE_HOST.to_string()
        }
    });
    let deploy_tmp_dir_str = cli.deploy_tmp_dir;
    let stash_signing_key = zeroship_core::config::obtain_secret(
        "STASH_SIGNING_KEY / --stash-signing-key",
        &cli.stash_signing_key,
        file_secrets.stash_signing_key.as_deref(),
        cli.check_config,
    );
    // Dedicated pairwise-salt secret (auth-sdk §6.2). MUST match the gateway's
    // value — both derive the per-app `pws_`. `--pairwise-salt-file` wins over
    // the inline value / overlay reference.
    let pairwise_salt = resolve_pairwise_salt(
        &cli.pairwise_salt_file,
        &cli.pairwise_salt,
        file_secrets.pairwise_salt.as_deref(),
        cli.check_config,
    );
    let expected_oauth_audience = cli.oauth_audience;
    let app_base_domain = cli.app_base_domain;
    let audit_retention_months = cli.audit_retention_months;
    let audit_retention_check_secs = cli.audit_retention_check_secs;

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
    // per-request ZeroShip-User HMAC, so it carries the ≥32-byte strength floor
    // (empty rejected outside dev, present-but-weak rejected). Skipped for a
    // secret REFERENCE under --check-config (the local is then the raw ref
    // string, which would wrongly fail the length check); it runs on the
    // resolved value at real boot.
    if !cli.check_config || !zeroship_core::config::is_secret_ref(&worker_key) {
        if let Err(message) =
            zeroship_core::config::validate_worker_key(&worker_key, insecure_dev)
        {
            eprintln!("control: {message}");
            tracing::error!(error = %message, "control: refusing to start with unsafe WORKER_KEY");
            std::process::exit(1);
        }
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
        // The OUTBOUND Stripe secret key (sk_…) is REQUIRED in prod: the billing
        // reconciler and `billing/setup` cannot bill without it, and silently
        // running with an empty key would drop revenue on the floor. Empty is
        // allowed ONLY under --dev-insecure (this block is the prod path). Skip
        // when --check-config still holds a raw secret reference.
        if stripe_secret_key.is_empty()
            && (!cli.check_config || !zeroship_core::config::is_secret_ref(&stripe_secret_key))
        {
            tracing::error!(
                "control: STRIPE_SECRET_KEY unset — the billing reconciler + /api/creators/:id/billing/setup \
                 cannot call Stripe. Set --stripe-secret-key, or run with --dev-insecure for local dev."
            );
            std::process::exit(1);
        }
        // M-Stripe prod guard (blueprint §M6 / §M9 risk 3): a `stripe`
        // (Billing Meters) deployment REQUIRES its operator-provisioned meter
        // event name. Booting `stripe` with no meter would push CU nowhere —
        // enforcing locally while billing Stripe $0 (a silent revenue black
        // hole). Refuse to boot. (`build_provider` also rejects it; this is the
        // earlier, clearer message on the prod path.)
        if cli.metering_provider.trim().eq_ignore_ascii_case("stripe")
            && cli.stripe_meter_event_name.trim().is_empty()
        {
            tracing::error!(
                "control: --metering-provider stripe requires --stripe-meter-event-name (the \
                 operator-provisioned Stripe Meter's event name). Refusing to boot a Stripe-Meters \
                 deployment with no meter (it would bill Stripe $0)."
            );
            std::process::exit(1);
        }
        // C2: the >24h re-drive reconcile reads the meter's aggregate by id; a
        // `stripe` deployment with no meter id would have to trust Stripe's 24h
        // identifier window (the over-bill window the fix closes). Refuse to boot.
        if cli.metering_provider.trim().eq_ignore_ascii_case("stripe")
            && cli.stripe_meter_id.trim().is_empty()
        {
            tracing::error!(
                "control: --metering-provider stripe requires --stripe-meter-id (the \
                 operator-provisioned Stripe Meter's `mtr_…` id). It is needed to read the meter's \
                 aggregate back for the >24h re-drive reconcile (C2); refusing to boot without it."
            );
            std::process::exit(1);
        }
        // M-OpenMeter prod guard (blueprint §M6 / §M9 risk 3): an `openmeter`
        // deployment REQUIRES its base URL + API token (else CU pushes go nowhere
        // — enforcing locally while exporting $0) AND a meter slug (needed to read
        // the aggregate back for the >24h re-drive reconcile, C2). Refuse to boot.
        // (`build_provider` also rejects these; this is the earlier, clearer
        // message on the prod path.)
        if cli.metering_provider.trim().eq_ignore_ascii_case("openmeter") {
            if cli.openmeter_url.trim().is_empty() || cli.openmeter_token.trim().is_empty() {
                tracing::error!(
                    "control: --metering-provider openmeter requires --openmeter-url and \
                     --openmeter-token. Refusing to boot an OpenMeter deployment with no endpoint \
                     (it would export $0 while enforcing locally)."
                );
                std::process::exit(1);
            }
            if cli.openmeter_meter_slug.trim().is_empty() {
                tracing::error!(
                    "control: --metering-provider openmeter requires --openmeter-meter-slug (the \
                     operator-provisioned meter's slug). It is needed to read the aggregate back \
                     for the >24h re-drive reconcile (C2); refusing to boot without it."
                );
                std::process::exit(1);
            }
        }
        // The dedicated pairwise-salt secret MUST be a strong, stable,
        // operator-set value outside dev — it seeds the PERMANENT per-app `pws_`
        // anchor and MUST equal the gateway's value. Skip the strength check
        // when `--check-config` still holds a raw reference.
        if !cli.check_config || !zeroship_core::config::is_secret_ref(&pairwise_salt) {
            if let Err(message) =
                zeroship_core::config::validate_pairwise_salt(&pairwise_salt, insecure_dev)
            {
                tracing::error!(error = %message, "control: refusing to start with unsafe pairwise salt");
                std::process::exit(1);
            }
        }
    }
    if insecure_dev {
        tracing::warn!("control: --dev-insecure set; admin + internal auth disabled");
    }

    // Control plane resource-server prerequisites. The console is now a
    // gateway-fronted regular app authenticated via `@zeroship/auth` (BFF) —
    // control has NO OIDC RP of its own anymore. It still needs: the stash
    // signing key (shared OIDC stash MAC) and the hydra public/admin URLs
    // (OAuth bearer introspection + issuer). The `AuthzGuard` bearer path +
    // audit run on the SINGLE `--db` connection (there is no separate auth DB
    // any more). Refuses to boot unless these are configured (`--dev-insecure`
    // permits localhost defaults only).
    if !insecure_dev {
        let mut missing = Vec::new();
        if hydra_public_url.is_empty() {
            missing.push("--hydra-public-url / HYDRA_PUBLIC_URL");
        }
        if stash_signing_key.is_empty() {
            missing.push("--stash-signing-key / STASH_SIGNING_KEY");
        }
        if hydra_admin_url.is_empty() {
            missing.push("--hydra-admin-url / HYDRA_ADMIN_URL");
        }
        if !missing.is_empty() {
            tracing::error!(
                missing = %missing.join(", "),
                "control: refusing to start; the resource-server auth path and OAuth introspection require these flags. \
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
        report.field("bootstrap_console", CheckValue::Flag(bootstrap_console));
        if bootstrap_console {
            report.field("console_host", CheckValue::Plain(console_host.clone()));
            report.field(
                "console_zship",
                CheckValue::Plain(console_zship.display().to_string()),
            );
        }
        report.field("blob_store", CheckValue::Plain(blob_store_root.clone()));
        report.field("blob_store_remote", CheckValue::Flag(blob_store_is_remote));
        report.field(
            "deploy_tmp_dir",
            CheckValue::Plain(deploy_tmp_dir.display().to_string()),
        );
        report.field(
            "pairwise_salt_configured",
            CheckValue::Secret(!pairwise_salt.is_empty()),
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

    // The content-addressed `BlobStore` is the ONLY deploy-artifact store.
    // `.zship` deploys land in `{prefix}/blobs/` + `{prefix}/manifests/`;
    // control writes through the SAME store gateway + worker read (local disk
    // for dev, S3 for production). The legacy per-app `BundleStore`/VFS is
    // gone — app purge now deletes the app's manifest keyspace via
    // `BlobStore::delete_app_manifests`.
    let blob_store: Arc<dyn BlobStore> =
        build_blob_store(&store_url).expect("failed to initialise blob store");

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

    // `stash_signing_key` is validated above for shared-secret hygiene (it is
    // the same STASH_SIGNING_KEY the gateway uses); control no longer consumes
    // it directly now that the bespoke console OIDC RP (and its stash cookie)
    // is gone, so it is not threaded any further.
    let _ = &stash_signing_key;
    // Platform-wide pairwise salt (auth-sdk §6.2) — derived from the DEDICATED
    // `PAIRWISE_SALT` secret (NOT the stash key), via the SHARED helper, so
    // control's disconnect-app revocation writes the family marker on the SAME
    // `(client_id, pws_)` key the gateway arms read (Batch A fix 4). The SAME
    // `PAIRWISE_SALT` value must be configured on gateway + control, and is the
    // PERMANENT per-app identity anchor (never rotate without a migration).
    let pairwise_salt_secret = if pairwise_salt.is_empty() {
        zeroship_core::config::DEV_PAIRWISE_SALT.to_string()
    } else {
        pairwise_salt
    };
    let pairwise_salt =
        zeroship_core::auth::derive_pairwise_salt(pairwise_salt_secret.as_bytes());
    // Control plane is a pure API resource server: no console OIDC RP. The
    // hydra introspector is still needed for the OAuth-bearer arm of the
    // `AuthzGuard` (third-party access tokens introspected against hydra-admin).
    let hydra_introspector = Arc::new(zeroship_core::hydra::HydraIntrospector::new(
        &hydra_admin_url,
    ));

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
    // Runs AFTER migrate (Liquibase, out of band), AFTER the registry / env
    // store / blob store are up, and BEFORE AppState is constructed (registry +
    // env_store are moved into it below). The console is a pure creator app, so
    // the seed touches only the control schema (apps / oauth / env) — no PAT, no
    // auth-schema service principal.
    //
    // Compose wiring (R5 cutover — DONE): the console `.zship` is built in the
    // Docker `sdks` stage and COPYed to `/opt/zeroship/console/app.zship`; the
    // control service runs with `--bootstrap-console --console-host
    // console.zeroship.localhost --console-zship /opt/zeroship/console/app.zship`,
    // ordered after the `migrate` service. `ops/Caddyfile` routes
    // `console.zeroship.localhost` → the gateway (the console is a gateway-fronted
    // app); the separate Vite builder service is retired.
    // Seed the built-in plan tiers (free/pro/unlimited) UNCONDITIONALLY at boot
    // — independent of `--bootstrap-console`. PR4 made `apps.plan_id` an FK into
    // `zeroship.plans`, so `create_app`/`set_plan` (and the console seed) all
    // require the built-in plans to exist. Idempotent (ON CONFLICT DO UPDATE on
    // the deterministic `pln_…` ids), so a re-boot is a no-op.
    bootstrap_console::seed_plans(&registry)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "control: plan-catalog seed failed");
            std::io::Error::other(err.to_string())
        })?;

    if bootstrap_console {
        let console_scheme = if insecure_dev { "http" } else { "https" };
        let cfg = bootstrap_console::ConsoleBootstrapConfig {
            enabled: true,
            console_host: console_host.clone(),
            console_zship,
            scheme: console_scheme.to_string(),
            hydra_admin_url: hydra_admin_url.clone(),
        };
        // The seed's per-app OAuth client upsert (`ensure_app_client`) needs an
        // owned, MUTABLE control-schema connection (it runs a transaction). Open
        // a dedicated one on the control DSN; it is dropped at the end of the
        // seed (Terminate sent on drop).
        let mut control_pg = {
            let (pg_client, pg_conn) =
                compio_postgres::connect(&db_url, compio_postgres::NoTls)
                    .await
                    .expect("control: console-seed control-pg connect");
            compio::runtime::spawn(async move {
                if let Err(e) = pg_conn.run().await {
                    tracing::error!(error = %e, "control/console-seed-pg connection ended");
                }
            })
            .detach();
            pg_client
        };
        bootstrap_console::bootstrap_console(
            &cfg,
            &registry,
            &env_store,
            &blob_store,
            &mut control_pg,
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "control: console bootstrap failed");
            std::io::Error::other(err.to_string())
        })?;
    }

    // Metering provider (M6): parse the kind, then build it. An unknown value
    // or a not-yet-implemented backend (stripe/openmeter) refuses to boot with a
    // clear message rather than silently mis-billing.
    let metering_provider_kind =
        match zeroship_control::metering::provider::MeteringProviderKind::parse(
            &cli.metering_provider,
        ) {
            Ok(k) => k,
            Err(bad) => {
                tracing::error!(
                    value = %bad,
                    "control: unknown --metering-provider (expected native|stripe|openmeter)"
                );
                std::process::exit(1);
            }
        };
    // Build the per-deployment provider config. For the `stripe` backend, carry
    // the operator-provisioned meter creds (event name + the platform Stripe
    // secret + base URL — the SAME account/url the Native rail uses). An empty
    // event name leaves `stripe_meter = None`, which `build_provider` rejects
    // (and the prod guard below catches earlier with a clearer message).
    let metering_provider_config = match metering_provider_kind {
        zeroship_control::metering::provider::MeteringProviderKind::Stripe
            if !cli.stripe_meter_event_name.trim().is_empty() =>
        {
            zeroship_control::metering::provider::MeteringProviderConfig::stripe(
                zeroship_control::metering::provider::StripeMeterConfig {
                    event_name: cli.stripe_meter_event_name.trim().to_string(),
                    meter_id: cli.stripe_meter_id.trim().to_string(),
                    secret_key: zeroship_control::SecretString::new(stripe_secret_key.clone()),
                    base_url: cli.stripe_base_url.clone(),
                },
            )
        }
        // OpenMeter: carry the operator-provisioned base URL + token + event type
        // + meter slug. Empty url/token/slug leaves the config rejectable by
        // `build_provider` (and the prod guard above catches it earlier).
        zeroship_control::metering::provider::MeteringProviderKind::OpenMeter
            if !cli.openmeter_url.trim().is_empty() =>
        {
            zeroship_control::metering::provider::MeteringProviderConfig::openmeter(
                zeroship_control::metering::provider::OpenMeterConfig {
                    base_url: cli.openmeter_url.trim().to_string(),
                    token: zeroship_control::SecretString::new(cli.openmeter_token.clone()),
                    event_type: cli.openmeter_event_type.trim().to_string(),
                    meter_slug: cli.openmeter_meter_slug.trim().to_string(),
                },
            )
        }
        kind => zeroship_control::metering::provider::MeteringProviderConfig {
            kind,
            stripe_meter: None,
            openmeter: None,
        },
    };
    let metering_provider =
        match zeroship_control::metering::provider::build_provider(&metering_provider_config) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "control: refusing to start — metering provider not available");
                std::process::exit(1);
            }
        };
    tracing::info!(
        metering_provider = metering_provider_kind.as_str(),
        "control: metering provider selected"
    );

    // Tax provider (PR-5): parse the kind, then build it. `native` (default)
    // computes 0 (USD launch). An unknown value refuses to boot rather than
    // silently mis-taxing.
    let tax_provider_kind = match zeroship_control::tax::TaxProviderKind::parse(&cli.tax_provider) {
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

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        blob_store,
        control_key: zeroship_control::SecretString::new(control_key),
        master_key: zeroship_control::SecretString::new(master_key),
        stripe_webhook_secret: zeroship_control::SecretString::new(stripe_webhook_secret),
        stripe_secret_key: zeroship_control::SecretString::new(stripe_secret_key),
        stripe_base_url,
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
        control_pg,
        hydra_admin_url,
        app_base_domain,
        trusted_oauth_clients,
        expected_oauth_audience,
        static_policies: zeroship_authz::load_platform_policies()
            .expect("control: bundled authz policies parse"),
        pat_issuer,
        hydra_introspector,
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        metering_provider,
        tax_provider,
        pairwise_salt,
    });

    // Spawn the in-process control crons:
    //   - audit_retention: sanctioned deleter for the append-only
    //     `zeroship.app_audit` + `zeroship.authz_decisions` tables (peer of the
    //     auth `audit_events` sweep; shares the `zeroship.audit_retention` GUC).
    //   - orphaned_app_reaper: purges apps left owner-less by the ISS-12
    //     account-erase reaper (DB row + blobs + Hydra client), excluding the
    //     `system = true` platform console.
    // Both hold an `Arc<AppState>` clone (cheap) and open fresh per-tick
    // connections.
    zeroship_control::cron::spawn_all(
        Arc::clone(&state),
        audit_retention_months,
        audit_retention_check_secs,
    );
    tracing::info!(
        retention_months = audit_retention_months,
        check_secs = audit_retention_check_secs,
        "control: audit-retention + orphaned-app-reaper crons spawned"
    );

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
            // --- Spend-limit override (PR5, M4): creator-facing cap ---
            .service(
                web::resource("/api/apps/{id}/spend-limit")
                    .route(web::get().to(api::get_spend_limit))
                    .route(web::put().to(api::set_spend_limit)),
            )
            // --- Plan catalog (PR4): operator-editable pricing catalog ---
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
            // --- Stream-1 infra-billing setup (PR6): platform Customer + card ---
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
            .service(
                web::resource("/internal/usage")
                    .route(web::post().to(internal::report_usage)),
            )
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
