//! zeroship-control — control plane binary. Thin wrapper over
//! `zeroship_control` (the library crate).

use std::sync::Arc;

use clap::Parser;
use ntex::web;
use zeroship_core::auth_provider::{
    AuthProvider, ConfiguredProvider, PlatformConfig, PlatformProvider, SupabaseConfig,
    SupabaseProvider,
};
use zeroship_core::config::{
    bootstrap_or_exit, validate_master_key_material,
    AuthProviderKind, CheckConfigReport, CheckValue,
};
use zeroship_bundle::{
    build_blob_store, build_workflow_blob_store, BlobStore, StoreUrl, WorkflowBlobStore,
};
use zeroship_control::config::{ControlSettings, ControlSettingsSources};
use zeroship_control::{
    admin_handlers, api, device_handlers, env_handlers,
    internal, migrations_api, oauth_grants_handlers, oauth_handlers, plan_catalog, stripe_handlers,
    workflow_instance_api,
    AppState, EnvStore, Quota, RateLimiter, Registry, StripeStore,
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn build_billing_mailer(
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
                // The RESOLVED material, or None when nothing supplied it.
                // `expose_secret` returning an Option is what keeps "unset"
                // distinguishable from "supplied and empty" at this boundary.
                password: settings.smtp_password.expose_secret().cloned(),
                tls: SmtpTls::Starttls,
            })
            .map_err(|e| format!("smtp mailer: {e}"))?;
            Ok(Arc::new(driver))
        }
        "resend" => {
            let api_key = settings
                .resend_api_key
                .expose_secret()
                .cloned()
                .ok_or_else(|| {
                    "ZEROSHIP_CONTROL_RESEND_API_KEY / --resend-api-key-file is required \
                     when --mailer=resend"
                        .to_string()
                })?;
            Ok(Arc::new(ResendMailer::new(ResendConfig { api_key })))
        }
        other => Err(format!("unknown mailer: {other:?}; use stdout|smtp|resend")),
    }
}

/// Split the resolved legacy-master-key list into its entries.
///
/// ONE secret holding a comma-list, not a list of secrets. A dry run that never
/// read the material yields no entries, which is correct: there is nothing to
/// validate and nothing to decrypt with.
fn split_legacy_master_keys(
    legacy_master_keys: &zeroship_core::config::Secret<String>,
) -> Vec<String> {
    legacy_master_keys
        .expose_secret()
        .map(|csv| {
            csv.split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// How the native-mode boot guard names the platform issuer input.
///
/// A `const` rather than a literal at the guard so the diagnostic test below
/// can read the exact string an operator sees. Every other auth-provider
/// diagnostic is already reachable through the function that produces it.
const PLATFORM_ISSUER_INPUT: &str = "--auth-platform-issuer / ZEROSHIP_AUTH_PLATFORM_ISSUER";
/// Operator-facing spelling of the bundle master key, for a diagnostic that has
/// to name something the operator can actually set. The bare `MASTER_KEY` this
/// used to interpolate is not settable any more, so an operator reading the
/// refusal had no name to act on.
const MASTER_KEY_LABEL: &str = "ZEROSHIP_CONTROL_MASTER_KEY";
/// Same, for the rotation list. The index is appended per entry.
const LEGACY_MASTER_KEYS_LABEL: &str = "ZEROSHIP_CONTROL_LEGACY_MASTER_KEYS";
/// Operator-facing spelling of the worker dispatch key. The shared identity in
/// `crates/config-macros/src/shared.rs` (`canonical: "worker_key"`) projects to
/// this environment name. The bare `WORKER_KEY` the shared validator used to
/// interpolate is not settable.
const WORKER_KEY_LABEL: &str = "ZEROSHIP_WORKER_KEY / --worker-key-file";
/// Same, for the shared pairwise salt (`canonical: "pairwise_salt"`).
const PAIRWISE_SALT_LABEL: &str = "ZEROSHIP_PAIRWISE_SALT / --pairwise-salt-file";

fn build_control_auth_provider(
    auth_provider: AuthProviderKind,
    supabase: ControlSupabaseAuthConfig,
    platform: ControlPlatformAuthConfig,
) -> Result<Arc<AuthProvider>, String> {
    match auth_provider {
        AuthProviderKind::Native => {
            let platform_config = platform_config_required(platform)?;
            Ok(Arc::new(AuthProvider::platform(PlatformProvider::new(
                platform_config,
            ))))
        }
        AuthProviderKind::Supabase => {
            if supabase.anon_key.trim().is_empty() {
                return Err(
                    "ZEROSHIP_AUTH_SUPABASE_ANON_KEY is required for \
                     ZEROSHIP_AUTH_PROVIDER=supabase"
                        .to_string(),
                );
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
            // The trusted SET is DERIVED, not configured: the platform OP joins
            // it whenever an issuer for it exists. That is one setting deciding
            // what auth SERVES and a second deciding whether the platform OP is
            // also reachable - never a third provider value meaning "both".
            let mut providers = vec![ConfiguredProvider::Supabase(SupabaseProvider::new(config))];
            if let Some(platform_config) = platform_config(platform)? {
                providers.push(ConfiguredProvider::Platform(PlatformProvider::new(
                    platform_config,
                )));
            }
            AuthProvider::new(providers)
                .map(Arc::new)
                .map_err(|err| format!("auth provider set: {err}"))
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

fn main() -> std::io::Result<()> {
    // Control uses cyper for provider/admin calls (Supabase identity bridge,
    // Stripe reconciliation). Install the workspace's selected rustls provider
    // before any outbound client can be constructed.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // ONE declaration, one parser. There is no second hand-written struct
    // holding the credentials any more, so there is no second place a flag, an
    // environment name or an overlay path can be spelled.
    let (settings, boot) = bootstrap_or_exit::<ControlSettings>(
        ControlSettingsSources::parse(),
        zeroship_control::config::DEFAULT_LOG_FILTER,
        "control",
    );
    // Billing notifier mailer (PR-6): built from the resolved mailer setting.
    // An unknown driver or missing creds refuses to boot.
    let billing_mailer: Arc<dyn zeroship_mailer::Mailer> =
        match build_billing_mailer(&settings) {
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
    let supabase_anon_key = settings.supabase_anon_key.get().clone();
    // The resolved material, or "" when this run has none. Under --check-config
    // a file-sourced secret deliberately has none, which is why the report below
    // asks `is_configured()` instead of looking at the string.
    let supabase_service_role_key = settings.supabase_service_role_key.expose_str().to_owned();
    let supabase_jwt_secret = settings.supabase_jwt_secret.expose_str().to_owned();
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
    // Every secret is already resolved by the generated declaration, in one
    // place, with one precedence: the `-file` path flag, then the canonical
    // environment name, then the canonical overlay path. The material is
    // dereferenced ONLY on a real boot, so under `--check-config` a file-sourced
    // secret is `is_configured()` with no material at all. That replaces the
    // arrangement where a check run held the raw `urn:` REFERENCE in the same
    // local a boot run held the secret, and every strength guard had to remember
    // to ask which one it was looking at.
    let db_url = settings.database_url.expose_str().to_owned();
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
    let control_key = settings.control_key.expose_str().to_owned();
    let master_key = settings.master_key.expose_str().to_owned();
    let workers_str = settings.worker_urls.get().clone();
    let gateway_url = settings.gateway_url.get().trim_end_matches('/').to_string();
    let migrated_url = settings.migrated_url.get().trim_end_matches('/').to_string();
    let worker_key = settings.worker_key.expose_str().to_owned();
    let stripe_webhook_secret = settings.stripe_webhook_secret.expose_str().to_owned();
    let stripe_secret_key = settings.stripe_secret_key.expose_str().to_owned();
    let stripe_base_url = settings.stripe_base_url.get().clone();
    // Previous master keys, tried as fallbacks on decrypt failure during a
    // rotation grace period. ONE secret holding a comma-list, resolved once and
    // split once. Each entry used to be resolvable as its OWN reference, which
    // meant the parse depended on whether a resolved value contained a comma;
    // now an entry is always a literal key.
    let legacy_keys: Vec<String> = split_legacy_master_keys(&settings.legacy_master_keys);
    let deploy_tmp_dir_str = settings.deploy_tmp_dir.get().clone();
    // Dedicated pairwise-salt secret (auth-sdk 6.2). MUST match the gateway's
    // value: both derive the per-app `pws_`. One declaration now, so the
    // file-wins-over-value dance is gone - the only flag IS the path flag.
    let pairwise_salt = settings.pairwise_salt.expose_str().to_owned();
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

    // S3 / L6: control authenticates the worker admin log fan-out with the
    // worker key. The same key gates the worker's dispatch bearer AND keys the
    // per-request ZeroShip-User HMAC, so it carries the >=32-byte strength floor
    // (empty and present-but-weak values are rejected).
    //
    // Every strength guard below goes through `validate_secret_material`, which
    // runs the validator on the RESOLVED material whenever this run has any. The
    // old `if !check_config || !is_secret_ref(..)` conditional existed only
    // because a check run held the reference TEXT in the value's place; nothing
    // does that now, so the branch is gone rather than restated.
    if let Err(message) = zeroship_core::config::validate_secret_material(
        &settings.worker_key,
        |value| zeroship_core::config::validate_worker_key(WORKER_KEY_LABEL, value),
    ) {
        eprintln!("control: {message}");
        tracing::error!(error = %message, "control: refusing to start with unsafe worker key");
        std::process::exit(1);
    }

    let mut missing = Vec::new();
    if !settings.master_key.is_configured() {
        missing.push("--master-key-file / ZEROSHIP_CONTROL_MASTER_KEY");
    }
    if !settings.control_key.is_configured() {
        missing.push("--control-key-file / ZEROSHIP_CONTROL_KEY");
    }
    if !missing.is_empty() {
        tracing::error!(
            missing = %missing.join(", "),
            "control: refusing to start; required secrets missing"
        );
        std::process::exit(1);
    }
    if let Err(message) = zeroship_core::config::validate_secret_material(
        &settings.master_key,
        |material| validate_master_key_material(MASTER_KEY_LABEL, material),
    ) {
        tracing::error!(error = %message, "control: refusing to start with weak master key");
        std::process::exit(1);
    }
    // The list is one secret, so its ENTRIES are always material by the time
    // they are split: either the run resolved the whole value, or it is a dry
    // run that read nothing and `legacy_keys` is empty.
    for (idx, legacy_key) in legacy_keys.iter().enumerate() {
        let label = format!("{LEGACY_MASTER_KEYS_LABEL}[{idx}]");
        if let Err(message) = validate_master_key_material(&label, legacy_key) {
            tracing::error!(
                error = %message,
                "control: refusing to start with weak legacy master key"
            );
            std::process::exit(1);
        }
    }
    if !settings.stripe_webhook_secret.is_configured() {
        // Not fatal: operators may run without Stripe. Every webhook will
        // reject with 500, so warn before Stripe-side retries reveal it.
        tracing::warn!(
            "control: stripe_webhook_secret unset; /internal/webhooks/stripe will reject every request. \
             Set ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET if you need Stripe integration."
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
    if let Err(message) = zeroship_core::config::validate_secret_material(
        &settings.pairwise_salt,
        |value| zeroship_core::config::validate_pairwise_salt(PAIRWISE_SALT_LABEL, value),
    ) {
        tracing::error!(error = %message, "control: refusing to start with unsafe pairwise salt");
        std::process::exit(1);
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
            // Presence only, unchanged. The anon key is operational by
            // classification (Supabase publishes it to browsers), but a report
            // that started PRINTING a value it used to withhold would be a new
            // disclosure introduced by a refactor, which is not this step.
            CheckValue::Secret(!supabase_anon_key.is_empty()),
        );
        report.field(
            "supabase_service_role_key_configured",
            CheckValue::Secret(settings.supabase_service_role_key.is_configured()),
        );
        report.field(
            "supabase_jwt_secret_configured",
            CheckValue::Secret(settings.supabase_jwt_secret.is_configured()),
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
            CheckValue::Secret(settings.pairwise_salt.is_configured()),
        );
        report.field("workers_count", CheckValue::Count(workers_count));
        report.field("gateway_url", CheckValue::Plain(gateway_url.clone()));
        report.field("migrated_url", CheckValue::Plain(migrated_url.clone()));

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
    // `PAIRWISE_SALT` value must be configured on auth + gateway + control. It
    // is the PERMANENT per-app identity anchor (never rotate without a
    // migration).
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
        migrated_url,
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

    // ONE gate for the whole process, shared by every ntex worker thread, so
    // the TTL bounds probe-driven Postgres traffic per PROCESS rather than
    // per thread.
    let readiness = Arc::new(zeroship_core::readiness::ReadinessGate::with_defaults());

    web::server(async move || {
        web::App::new()
            .state(state.clone())
            .state(readiness.clone())
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
            // The creator-facing entry to the migration service. `migrated`
            // binds loopback (it holds the superuser provisioning DSN), so this
            // is the only route a creator's `zeroship migrate` can take. See
            // the module header for why the hop adds no authority.
            .service(
                web::resource("/api/apps/{id}/migrations/apply")
                    .state(web::types::PayloadConfig::new(
                        migrations_api::MIGRATIONS_APPLY_PAYLOAD_BYTES,
                    ))
                    .route(web::post().to(migrations_api::apply_migrations)),
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
            // Creator self-service for the app's raw-TCP egress hosts. The
            // creator names hosts; the plan's caps and the operator's
            // frontable-suffix catalog bound what they may name, and an app
            // with no rows stays default-deny.
            .configure(zeroship_control::net_grants::configure)
            // --- Auth (resource server) ---
            // No console OIDC RP and no console back-channel-logout endpoint:
            // the console is now a gateway-fronted regular app authenticated
            // via `@zeroship/auth` (BFF). Per-app back-channel logout for the
            // console is handled by the GATEWAY's own per-app BCL endpoint (it
            // is a gateway app like any other). Control exposes only the
            // OAuth-grant management surface below.
            .configure(device_handlers::configure)
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
                web::resource("/healthz")
                    .route(web::get().to(internal::healthz)),
            )
            .service(
                web::resource("/readyz")
                    .route(web::get().to(internal::readyz)),
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
    /// The one env-name scanner, shared with every other binary's copy of this
    /// test. Local copies would be four things to keep in step.
    use zeroship_core::config::env_like_tokens;
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
            ControlSettingsSources::try_parse_from(["zeroship-control", "--blob-store", "/tmp/blob-root"])
                .expect("blob-store flag should parse");

        assert_eq!(cli.blob_store.as_deref(), Some("/tmp/blob-root"));
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
        let env = ControlSettingsSources::try_parse_from(["zeroship-control"]).expect("parse env topology");
        let resolved =
            ControlSettings::resolve_config(env, Some(&overlay)).expect("resolve");
        assert_eq!(resolved.origin_scheme.get(), &OriginScheme::Http);

        let cli = ControlSettingsSources::try_parse_from([
            "zeroship-control",
            "--origin-scheme",
            "https",
        ])
        .expect("parse CLI topology");
        let flagged =
            ControlSettings::resolve_config(cli, Some(&overlay)).expect("resolve");
        assert_eq!(flagged.origin_scheme.get(), &OriginScheme::Https);

        restore_env_var("ZEROSHIP_ORIGIN_SCHEME", old);
    }

    #[test]
    fn auth_provider_selector_defaults_to_native_and_accepts_supabase() {
        // Same environment hazard as the retired-spelling test below: clap
        // reads `ZEROSHIP_AUTH_PROVIDER` into the same carrier the flag uses,
        // so an ambient value - a sibling test's, or the caller's shell -
        // makes "defaults to native" assert about the environment rather than
        // about the compiled default.
        let _guard = CONFIG_ENV_LOCK.lock().expect("env lock");
        let old = zeroship_core::test_env_os!("ZEROSHIP_AUTH_PROVIDER");
        std::env::remove_var("ZEROSHIP_AUTH_PROVIDER");

        let cli = ControlSettingsSources::try_parse_from(["zeroship-control"]).expect("parse defaults");
        let resolved = ControlSettings::resolve_config(cli, None).expect("resolve");
        assert_eq!(resolved.auth_provider.get(), &AuthProviderKind::Native);

        let cli = ControlSettingsSources::try_parse_from(["zeroship-control", "--auth-provider", "supabase"])
            .expect("parse supabase");
        let resolved = ControlSettings::resolve_config(cli, None).expect("resolve");
        assert_eq!(resolved.auth_provider.get(), &AuthProviderKind::Supabase);

        restore_env_var("ZEROSHIP_AUTH_PROVIDER", old);
    }

    #[test]
    fn the_retired_platform_spelling_is_rejected_by_the_flag_and_the_overlay() {
        // `platform` was control's own word for the state now spelled `native`.
        // Both tiers must refuse it, or the two vocabularies survive the merge
        // in the one place an operator would not look.
        //
        // THE OVERLAY HALF READS THE PROCESS ENVIRONMENT, so it needs the same
        // lock the env-mutating tests take AND it needs the variable absent.
        // The environment tier outranks the overlay, so a sibling test holding
        // `ZEROSHIP_AUTH_PROVIDER=supabase` makes the retired overlay value
        // never get parsed and the refusal below never fire. Measured at 1
        // failure in 12 runs of this binary before the lock was taken; the
        // variable is unset here as well as locked, so an ambient value in the
        // caller's shell cannot reproduce it either.
        let _guard = CONFIG_ENV_LOCK.lock().expect("env lock");
        let old = zeroship_core::test_env_os!("ZEROSHIP_AUTH_PROVIDER");
        std::env::remove_var("ZEROSHIP_AUTH_PROVIDER");

        let err = ControlSettingsSources::try_parse_from([
            "zeroship-control",
            "--auth-provider",
            "platform",
        ])
        .map(|_| ())
        .expect_err("the retired control spelling must not parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);

        let overlay: toml::Value = toml::from_str("[auth]\nprovider = \"platform\"\n")
            .expect("fixture overlay");
        let cli = ControlSettingsSources::try_parse_from(["zeroship-control"]).expect("parse defaults");
        let err = ControlSettings::resolve_config(cli, Some(&overlay))
            .map(|_| ())
            .expect_err("the retired control spelling must not resolve from the overlay");
        assert!(
            format!("{err}").contains("auth.provider"),
            "the overlay rejection must name the key: {err}"
        );

        restore_env_var("ZEROSHIP_AUTH_PROVIDER", old);
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

        let cli = ControlSettingsSources::try_parse_from(["zeroship-control"]).expect("parse control");
        let resolved = ControlSettings::resolve_config(cli, None).expect("resolve");
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
        assert_eq!(
            provider.issuers().collect::<Vec<_>>(),
            vec!["https://project.supabase.co/auth/v1"],
            "with no platform issuer configured the trusted set is Supabase alone"
        );
        assert_eq!(provider.platform_issuer(), None);

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
        .expect("valid two-provider set");
        assert_eq!(
            provider.issuers().collect::<Vec<_>>(),
            vec![
                "https://project.supabase.co/auth/v1",
                "https://auth.zeroship.test"
            ],
            "a configured platform issuer JOINS the trusted set; it is not a third provider value"
        );
        assert_eq!(
            provider.supabase_issuer(),
            Some("https://project.supabase.co/auth/v1"),
            "the Supabase device-flow helpers still find their issuer by name"
        );
        assert_eq!(
            provider.platform_issuer(),
            Some("https://auth.zeroship.test")
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
        let command = <ControlSettingsSources as clap::CommandFactory>::command();
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

    /// Build a Supabase config that fails for exactly one reason, and return the
    /// message. `SupabaseConfig::new` is the code that emits it, so these are
    /// DRIVEN, not copied.
    fn supabase_config_error(
        url: &str,
        jwt_secret: Option<&str>,
        jwks_url: Option<&str>,
        issuer: &str,
    ) -> String {
        SupabaseConfig::new(
            url,
            "anon-key",
            None,
            jwt_secret.map(str::to_owned),
            jwks_url.map(str::to_owned),
            issuer,
        )
        .map(|_| ())
        .expect_err("this input must fail closed")
        .to_string()
    }

    /// Every OTHER startup refusal control can emit that names a variable.
    ///
    /// Split from [`auth_provider_diagnostics`] only because these come from a
    /// different module; the check below runs the identical assertion over both.
    /// Extending the existing test was the point: the Supabase arm and the
    /// secret-strength arm are the same defect class as the auth-provider arm,
    /// and a second copy of the scanner would have been a second thing to keep
    /// in step.
    fn other_startup_diagnostics() -> Vec<String> {
        let strong = "0123456789abcdef0123456789abcdef";
        vec![
            // Supabase provider config. These named a bare `SUPABASE_URL`,
            // `SUPABASE_JWT_ISSUER`, `SUPABASE_JWT_SECRET` and
            // `SUPABASE_JWKS_URL` - four spellings control does not read.
            supabase_config_error("", Some(strong), None, "https://issuer.test"),
            supabase_config_error("https://p.supabase.co", Some(strong), None, ""),
            supabase_config_error("https://p.supabase.co", Some("short"), None, "https://i.test"),
            supabase_config_error("https://p.supabase.co", None, Some(""), "https://i.test"),
            // Secret strength. The shared validators used to interpolate a bare
            // `WORKER_KEY` / `PAIRWISE_SALT`; they now carry control's label.
            zeroship_core::config::validate_worker_key(WORKER_KEY_LABEL, "")
                .expect_err("an unset worker key must fail closed"),
            zeroship_core::config::validate_worker_key(WORKER_KEY_LABEL, "short")
                .expect_err("a weak worker key must fail closed"),
            zeroship_core::config::validate_pairwise_salt(PAIRWISE_SALT_LABEL, "")
                .expect_err("an unset pairwise salt must fail closed"),
            validate_master_key_material(MASTER_KEY_LABEL, "YWJj")
                .expect_err("a short master key must fail closed"),
            validate_master_key_material(&format!("{LEGACY_MASTER_KEYS_LABEL}[0]"), "YWJj")
                .expect_err("a short legacy master key must fail closed"),
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
        // EXTENDED 2026-08-13 to the Supabase provider config and the
        // secret-strength refusals, which carried the same defect: bare
        // `SUPABASE_JWT_SECRET`, `WORKER_KEY` and `PAIRWISE_SALT` spellings that
        // no binary reads. Extending this test rather than writing a second one
        // is deliberate - the scanner and the derived readable set are the parts
        // worth having exactly once.
        //
        // What this does NOT catch: a diagnostic that names a variable control
        // really does read but that is the WRONG one for the failure at hand,
        // any stale name in a diagnostic outside the two sets driven below, and
        // a stale spelling that happens to be a SUBSTRING of a live name (the
        // scanner tokenises, so `WORKER_KEY` inside `ZEROSHIP_WORKER_KEY` is not
        // a separate token and is invisible here).
        let readable = env_names_control_reads();
        assert!(
            readable.contains("ZEROSHIP_AUTH_PROVIDER"),
            "the derivation itself is broken: control's own selector is absent"
        );

        for diagnostic in auth_provider_diagnostics()
            .into_iter()
            .chain(other_startup_diagnostics())
        {
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
        assert_eq!(
            provider.issuers().collect::<Vec<_>>(),
            vec!["https://auth.zeroship.test/oauth2"]
        );
        assert_eq!(provider.supabase_issuer(), None);
    }

    #[test]
    fn control_rejects_removed_bundles_flag() {
        let err = ControlSettingsSources::try_parse_from([
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
        let err = ControlSettingsSources::try_parse_from(["zeroship-control", "--dev-insecure"])
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


    // The secret SURFACE. Every credential control takes is a `--<name>-file`
    // path flag; the value spellings that carried the material in argv are gone.
    // A process argument list is world-readable on Linux, so this is the whole
    // reason a secret's flag differs from an operational one's.
    #[test]
    fn every_control_secret_takes_a_path_flag_and_no_value_flag() {
        use clap::CommandFactory as _;

        let command = ControlSettingsSources::command();
        let flags = command
            .get_arguments()
            .filter_map(clap::Arg::get_long)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        for expected in [
            "control-key-file",
            "worker-key-file",
            "master-key-file",
            "database-url-file",
            "pairwise-salt-file",
            "legacy-master-keys-file",
            "stripe-secret-key-file",
            "stripe-webhook-secret-file",
        ] {
            assert!(flags.contains(&expected.to_owned()), "missing --{expected}");
        }
        for gone in [
            "control-key",
            "worker-key",
            "master-key",
            "db",
            "pairwise-salt",
            "legacy-master-keys",
            "stripe-secret-key",
            "stripe-webhook-secret",
            "supabase-jwt-secret",
        ] {
            assert!(
                !flags.contains(&gone.to_owned()),
                "--{gone} still exists; a secret must not have a value flag"
            );
        }

        // Does NOT cover the ENV tier: a secret carries no `env` on its clap
        // carrier by design, so a Command scan cannot see one. The specs are
        // where that name lives, and the next test reads them.
    }

    // The environment projection, from the declaration rather than from clap.
    // The bare pre-conversion family must be gone: a deployment still exporting
    // MASTER_KEY has to find that out, and nothing else here would tell it.
    #[test]
    fn every_control_secret_projects_one_canonical_environment_name() {
        use zeroship_core::config::GeneratedConfig as _;

        let declared = ControlSettings::SPECS
            .iter()
            .filter_map(|spec| spec.env_name())
            .collect::<Vec<_>>();
        for expected in [
            "ZEROSHIP_CONTROL_KEY",
            "ZEROSHIP_WORKER_KEY",
            "ZEROSHIP_CONTROL_MASTER_KEY",
            "ZEROSHIP_CONTROL_DATABASE_URL",
            "ZEROSHIP_PAIRWISE_SALT",
        ] {
            assert!(declared.contains(&expected.to_owned()), "missing {expected}");
        }
        for name in &declared {
            assert!(
                name.starts_with("ZEROSHIP_"),
                "{name} is not a canonical projection"
            );
        }
        for gone in ["MASTER_KEY", "DATABASE_URL", "CONTROL_KEY", "WORKER_KEY"] {
            assert!(
                !declared.contains(&(*gone).to_owned()),
                "the bare name {gone} survives"
            );
        }

        // Does NOT cover whether the environment is actually READ at that name;
        // that is the generated resolver's `read_config_env!` and is exercised
        // end to end by tests/config_check_e2e.sh.
    }

    // A resolved secret publishes presence and nothing else. The sentinel is
    // long and distinctive so a leak of any substring would show, and the
    // length is checked separately because "17 characters" is itself a leak.
    #[test]
    fn a_resolved_secret_never_renders_its_material() {
        use zeroship_core::config::{Secret, SourceKind};

        const SENTINEL: &str = "control-master-key-sentinel-7f3a91c0e5";
        let secret = Secret::supplied(SourceKind::Env, Some(SENTINEL.to_owned()));
        let rendered = format!("{secret:?}");

        assert!(secret.is_configured());
        for length in 4..=SENTINEL.len() {
            assert!(
                !rendered.contains(&SENTINEL[..length]),
                "Debug leaked a {length}-char prefix: {rendered}"
            );
        }
        assert!(!rendered.contains(&SENTINEL.len().to_string()));

        // Does NOT cover a caller that calls expose_secret and prints the result
        // itself. The e2e sentinel case is what covers the assembled report.
    }

    // The legacy-master-key list is ONE secret holding a comma-list, not a list
    // of secrets. A dry run that read no material must yield no entries, because
    // there is nothing to strength-check and nothing to decrypt with.
    #[test]
    fn the_legacy_master_key_list_splits_material_and_nothing_else() {
        use zeroship_core::config::{Secret, SourceKind};

        let key = "00".repeat(32);
        let csv = format!("{key}, {key} ,");
        assert_eq!(
            split_legacy_master_keys(&Secret::supplied(SourceKind::Env, Some(csv))),
            vec![key.clone(), key],
            "entries are trimmed and empties dropped"
        );
        assert!(
            split_legacy_master_keys(&Secret::supplied(SourceKind::CliFile, None)).is_empty(),
            "a configured-but-unread secret yields no entries"
        );
        assert!(split_legacy_master_keys(&Secret::<String>::absent()).is_empty());

        // Does NOT cover the strength guard that then runs over the entries;
        // that is asserted by the boot-guard path, not by the splitter.
    }
}
