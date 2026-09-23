// The layout query for a deeply nested async block overflows the default
// recursion limit in release builds. `recursion_limit` is per crate ROOT and every
// bin is its own root, so each binary must set it; debug `cargo check` compiles
// anyway, which is why the debug gates do not catch it.
#![recursion_limit = "256"]

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
    audit_credentials, bootstrap_or_exit, mark_dev_escape_active, require_nonempty,
    validate_master_key_material, AuthProviderKind, BuildProfile, CheckConfigReport, CheckValue,
    CredentialPosture, CredentialVerdict, SubsystemCredential,
};
use zeroship_bundle::{
    build_blob_store, BlobStore, StoreUrl,
};
use zeroship_control::config::{ControlSettings, ControlSettingsSources};
use zeroship_control::{
    api, device_handlers, env_handlers, erasure, health,
    internal, oauth_grants_handlers, plan_catalog, stripe_handlers,
    AppState, EnvStore, Quota, RateLimiter, Registry, StripeStore,
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The one outbound mail transport this process gets.
///
/// It was `build_billing_mailer` until the invitation path needed one too. The
/// name changed rather than gaining a second builder: two transports built from
/// one setting would be two answers to "did this send", and the difference
/// between billing mail and transactional mail is what the SEAM above each does
/// with the message, not which socket it leaves by.
fn build_mailer(settings: &ControlSettings) -> Result<Arc<dyn zeroship_mailer::Mailer>, String> {
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

/// Load the single-host join-token minter's configuration, or say there is
/// none.
///
/// BOTH PATHS OR NEITHER. A credential with nowhere to write is a key held for
/// no reason, and a destination with no credential is a volume nothing will
/// ever fill - each on its own is a half-configured deployment that looks
/// configured, which is the shape every other fence in this binary refuses.
///
/// # Errors
///
/// Returns a message when exactly one of the two paths is set, when the zone
/// name is empty, or when the credential cannot be read, is insecurely
/// permissioned, or does not hold a signer key.
fn load_join_minter(
    settings: &ControlSettings,
) -> Result<Option<zeroship_control::join_minter::MinterConfig>, String> {
    let credential = settings.join_token_signer_file.get();
    let destination = settings.join_token_file.get();
    match (
        credential.as_os_str().is_empty(),
        destination.as_os_str().is_empty(),
    ) {
        (true, true) => return Ok(None),
        (false, false) => {}
        _ => {
            return Err(
                "control.join_token_signer_file and control.join_token_file must be set \
                 together: a signer key with nowhere to write mints for nobody, and a \
                 destination with no key is never filled"
                    .to_owned(),
            )
        }
    }
    let zone = settings.join_token_zone.get().trim().to_owned();
    if zone.is_empty() {
        return Err("control.join_token_zone is empty; a join token names one zone".to_owned());
    }
    let (signer_id, key) =
        zeroship_core::service_peers::load_join_signer_credential(credential)
            .map_err(|error| format!("join token minter credential: {error}"))?;
    Ok(Some(zeroship_control::join_minter::MinterConfig {
        signer_id,
        key,
        zone,
        path: destination.clone(),
    }))
}

/// How the native-mode boot guard names the platform issuer input.
///
/// A `const` rather than a literal at the guard so the diagnostic test below
/// can read the exact string an operator sees. Every other auth-provider
/// diagnostic is already reachable through the function that produces it.
const PLATFORM_ISSUER_INPUT: &str = "--auth-platform-issuer / ZEROSHIP_AUTH_PLATFORM_ISSUER";
/// Operator-facing spelling of the bundle master key, for a diagnostic that has
/// to name something the operator can actually set. The bare `MASTER_KEY` is not
/// settable, so an operator reading the refusal needs the full name.
const MASTER_KEY_LABEL: &str = "ZEROSHIP_CONTROL_MASTER_KEY";
/// Same, for the rotation list. The index is appended per entry.
const LEGACY_MASTER_KEYS_LABEL: &str = "ZEROSHIP_CONTROL_LEGACY_MASTER_KEYS";
/// Same, for the shared pairwise salt (`canonical: "pairwise_salt"`).
const PAIRWISE_SALT_LABEL: &str = "ZEROSHIP_PAIRWISE_SALT / --pairwise-salt-file";
/// Operator-facing spelling of the shared internal control key.
const CONTROL_KEY_LABEL: &str = "ZEROSHIP_CONTROL_KEY / --control-key-file";

/// Every credential the control plane needs, tagged by subsystem.
///
/// All three are unconditional. The control plane is the ONE service that
/// cannot have a disabled subsystem here: it serves the internal route registry
/// (`control_key`), decrypts creator environments (`master_key`) and seeds every
/// app's pairwise anchor (`pairwise_salt`). Its call to the worker is NOT a row:
/// that edge is an ed25519 service assertion now, minted from the key file
/// `build_service_auth` loads. Optional credentials it does have - the Stripe
/// webhook secret, the Stripe API key, the mailer credentials - are NOT rows
/// here, because a deployment without Stripe is a supported deployment and the
/// webhook handler already fails closed on an empty secret
/// (`crates/zeroship-control/src/stripe_handlers.rs`). Adding them would be exactly the
/// "blocked on a credential for a service they never enabled" outage the
/// per-subsystem rule forbids.
/// Load this control plane's service identity, or refuse to start.
///
/// ONE OUTCOME: unconfigured, unreadable and unparseable all exit. A
/// misconfigured process must not be allowed to look like a
/// correct-but-unconfigured one, and that is exactly as true of an ORCHESTRATOR
/// looking at the unconfigured one. A control plane that boots and refuses every
/// internal edge is a control plane whose workers cannot load an app.
///
/// The unconfigured case is refused inside `ServiceKeyring::load`, so this
/// function has no empty-path branch to get wrong. It runs before the process
/// touches the database: the same key signs lifecycle publication, so a
/// Control without it could accept deploys it could never publish.
fn load_service_keyring(
    key_file: &std::path::Path,
    peers_file: &std::path::Path,
) -> (
    zeroship_core::service_peers::ServiceKeyring,
    zeroship_core::service_assertion::ServiceTrustBundle,
) {
    use zeroship_core::service_peers::ServiceKeyring;

    // The SAME statement of control's own name the instance path compares `aud`
    // against, so a second spelling cannot make one of them refuse callers the
    // other admits.
    let issuer = match zeroship_control::internal::control_service_issuer() {
        Ok(issuer) => issuer,
        Err(error) => {
            eprintln!("control: refusing to start: control service issuer is malformed: {error}");
            tracing::error!(%error, "control: refusing to start - control service issuer is malformed");
            std::process::exit(1);
        }
    };
    let mut keyring = match ServiceKeyring::load(issuer, key_file, peers_file) {
        Ok(keyring) => keyring,
        Err(error) => {
            eprintln!(
                "control: refusing to start: service key material rejected ({error}); set \
                 control.service_key_file and control.service_peers_file"
            );
            tracing::error!(
                %error,
                "control: refusing to start - service key material rejected; set \
                 control.service_key_file and control.service_peers_file"
            );
            std::process::exit(1);
        }
    };
    let Some(bundle) = keyring.take_bundle() else {
        tracing::error!("control: refusing to start - peer bundle already taken");
        std::process::exit(1);
    };
    (keyring, bundle)
}

/// Assemble this control plane's service identity from its loaded keyring.
fn build_service_auth(
    keyring: zeroship_core::service_peers::ServiceKeyring,
    bundle: zeroship_core::service_assertion::ServiceTrustBundle,
    control_pg: Arc<compio_postgres::Client>,
) -> zeroship_core::service_peers::ServiceAuth {
    use zeroship_core::service_assertion::ServiceAssertionVerifier;
    use zeroship_core::service_peers::ServiceAuth;

    // The FULL profile: control's guarded edges fire at app-load rate, so the
    // single-use claim's write against the shared table is proportional to app
    // loads. The store is the process's own long-lived client, which is the
    // same connection `/readyz` probes - and it is built by the same function
    // the per-request instance verifier uses, because two stores would be two
    // answers to "has this assertion been seen".
    let replay = zeroship_control::internal::control_replay_store(control_pg);
    ServiceAuth::new(keyring, Arc::new(ServiceAssertionVerifier::new(bundle, replay)))
}

fn control_credentials(settings: &ControlSettings) -> Vec<SubsystemCredential<'_>> {
    vec![
        SubsystemCredential {
            subsystem: "internal-route-registry",
            enabled: true,
            label: CONTROL_KEY_LABEL,
            secret: &settings.control_key,
            validate: require_nonempty,
        },
        SubsystemCredential {
            subsystem: "app-env-encryption",
            enabled: true,
            label: MASTER_KEY_LABEL,
            secret: &settings.master_key,
            validate: validate_master_key_material,
        },
        SubsystemCredential {
            subsystem: "pairwise-subject-anchor",
            enabled: true,
            label: PAIRWISE_SALT_LABEL,
            secret: &settings.pairwise_salt,
            validate: zeroship_core::config::validate_pairwise_salt,
        },
    ]
}

/// Apply the boot gate, or exit.
fn enforce_control_credentials(
    settings: &ControlSettings,
    overlay: &zeroship_core::config::ConfigSource,
    check_config: bool,
) -> CredentialPosture {
    let posture = audit_credentials(&control_credentials(settings));
    let verdict = posture.verdict(BuildProfile::current(), check_config);
    if let Some(banner) = posture.banner("zeroship-control", overlay, verdict) {
        eprint!("{banner}");
        tracing::error!(
            subsystems = %posture
                .weak()
                .iter()
                .map(|weak| weak.subsystem)
                .collect::<Vec<_>>()
                .join(","),
            "control: unconfigured service credential"
        );
    }
    match verdict {
        CredentialVerdict::Proceed => posture,
        CredentialVerdict::Refuse => std::process::exit(1),
        CredentialVerdict::DevEscape => {
            mark_dev_escape_active();
            posture
        }
    }
}

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
    // Control uses cyper for provider/admin calls (Stripe reconciliation,
    // OpenMeter, workflow dispatch). Install the workspace's selected rustls provider
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
    // The process's mail transport, built from the resolved mailer setting. An
    // unknown driver or missing creds refuses to boot. Shared by the billing
    // notifier seam and by the request-path transactional mail on
    // `AppState.mailer`.
    let mailer: Arc<dyn zeroship_mailer::Mailer> = match build_mailer(&settings) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "control: refusing to start - mailer not available");
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

    // The worker-enrolment envelope, resolved here so a malformed declaration
    // fails the boot rather than every enrolment at run time. `trust_proxy` is
    // folded in because behind a trusted proxy the observed peer is the proxy,
    // and the derivation this envelope guards would place every worker at one
    // address; the envelope refuses outright instead.
    let worker_enrolment = match zeroship_control::worker_join::EnrolmentEnvelope::parse(
        settings.worker_enrolment_networks.get(),
        settings.worker_enrolment_ports.get(),
        trust_proxy,
    ) {
        Ok(envelope) => envelope,
        Err(message) => {
            eprintln!("control: invalid worker enrolment envelope: {message}");
            std::process::exit(2);
        }
    };

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
    // `auth.platform_issuer` and `auth.platform_jwks_url` ARE the
    // canonical paths, so the values below already carry CLI / env / overlay
    // precedence and `resolve_overlay_string` has nothing left to add.
    let auth_platform_issuer = settings.auth_platform_issuer.get().clone();
    let auth_platform_jwks_url = settings.auth_platform_jwks_url.get().clone();
    let trusted_oauth_clients = zeroship_control::resolve_trusted_oauth_clients(&file.auth);
    tracing::info!(
        trusted_oauth_clients = trusted_oauth_clients.len(),
        "control: trusted OAuth client set resolved"
    );
    let configured_oauth_clients = file.auth.oauth_clients.clone();
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
    let workflow_coordinator_url = settings.workflow_coordinator_url.get().to_owned();
    // Lifecycle publication cannot run against an origin the manager client
    // refuses, and a Control that accepts deploys it can never publish leaves
    // them pending forever. A configuration check refuses it too.
    if let Err(error) =
        zeroship_control::publication::publisher::validate_coordinator(&workflow_coordinator_url)
    {
        eprintln!("control: refusing to start: {error}");
        tracing::error!(%error, "control: refusing to start");
        std::process::exit(2);
    }
    let Some(catalog_max_connections) =
        std::num::NonZeroUsize::new(*settings.catalog_max_connections.get())
    else {
        eprintln!("control: refusing to start: control.catalog_max_connections must be positive");
        std::process::exit(2);
    };
    let stripe_webhook_secret = settings.stripe_webhook_secret.expose_str().to_owned();
    let stripe_secret_key = settings.stripe_secret_key.expose_str().to_owned();
    let stripe_base_url = settings.stripe_base_url.get().clone();
    // Previous master keys, tried as fallbacks on decrypt failure during a
    // rotation grace period. ONE secret holding a comma-list, resolved once and
    // split once. Each entry is a literal key.
    let legacy_keys: Vec<String> = split_legacy_master_keys(&settings.legacy_master_keys);
    let deploy_tmp_dir_str = settings.deploy_tmp_dir.get().clone();
    // Dedicated pairwise-salt secret. MUST match the gateway's
    // value: both derive the per-app `pws_`. The only flag IS the path flag.
    let pairwise_salt = settings.pairwise_salt.expose_str().to_owned();
    let expected_oauth_audience = settings.oauth_audience.get().clone();
    let app_base_domain = settings.app_base_domain.get().clone();
    let spend_recompute_interval = *settings.spend_recompute_interval.get();
    let audit_retention_months = *settings.audit_retention_months.get();
    let audit_retention_check_secs = *settings.audit_retention_check_secs.get();

    // Pure path resolution only — the writability PROBE (create_dir_all + probe
    // file) is deferred to the real startup path so `--check-config`
    // performs NO filesystem mutation but can still report the resolved path.
    let deploy_tmp_dir: std::path::PathBuf = if deploy_tmp_dir_str.is_empty() {
        std::env::temp_dir()
    } else {
        std::path::PathBuf::from(&deploy_tmp_dir_str)
    };

    // Every strength guard below goes through `validate_secret_material`, which
    // runs the validator on the RESOLVED material whenever this run has any.
    //
    // THE BOOT GATE: every row runs a validator over the MATERIAL, so
    // empty, the sentinel and a weak value share one fate. A bare
    // `is_configured()` is not enough: `crates/zeroship-core/src/config/env.rs`
    // resolves `ZEROSHIP_CONTROL_KEY=` to `Secret::supplied(Env, Some(""))` -
    // configured, empty, accepted. The credential that is ABSENT gets a branch of
    // its own and that branch says yes.
    let credentials =
        enforce_control_credentials(&settings, &boot.overlay.source, check_config);
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
    // It is a row of `control_credentials` above, with the same validator.

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
        // Read-only. No filesystem mutation, no signing-key load.
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
            // Presence only. The anon key is operational by
            // classification (Supabase publishes it to browsers), but the report
            // must not PRINT a secret value.
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
        // Report whether an envelope was DECLARED, not what it contains: an
        // operator checking a config wants to know that the enrolment route is
        // live at all, and an undeclared envelope refuses every enrolment.
        report.field(
            "worker_enrolment_declared",
            CheckValue::Flag(worker_enrolment.is_declared()),
        );
        // Presence only: the file is read, and its signers imported, at boot,
        // which a dry run does not reach.
        report.field(
            "join_signers_file_configured",
            CheckValue::Flag(!settings.join_signers_file.get().as_os_str().is_empty()),
        );
        // Whether THIS replica is configured to mint. It reports the
        // configuration, not the election: which replica holds the lease is a
        // runtime fact a dry run cannot know.
        report.field(
            "join_token_minter_configured",
            CheckValue::Flag(
                !settings.join_token_signer_file.get().as_os_str().is_empty()
                    && !settings.join_token_file.get().as_os_str().is_empty(),
            ),
        );
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
        report.field("workflow_coordinator_url", CheckValue::Plain(workflow_coordinator_url.clone()));
        report.field(
            "catalog_max_connections",
            CheckValue::Count(catalog_max_connections.get()),
        );
        report.field(
            "service_credentials",
            CheckValue::Plain(credentials.summary().to_string()),
        );
        report.field(
            "service_credentials_checked",
            CheckValue::Count(credentials.checked()),
        );
        report.field(
            "service_credentials_skipped",
            CheckValue::Count(credentials.skipped()),
        );
        report.field(
            "service_credentials_unread",
            CheckValue::Count(credentials.unread()),
        );

        report.emit(*settings.check_config_format.get());
        return Ok(());
    }

    // Side-effecting preflight runs only on the real startup path, after the
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

    let (service_keyring, service_peers) = load_service_keyring(
        settings.service_key_file.get(),
        settings.service_peers_file.get(),
    );

    ntex::rt::System::build()
        .name("zeroship-control")
        .build(ntex::rt::DefaultRuntime)
        .block_on(async move {
    let registry = Registry::connect(
        &db_url,
        zeroship_control::publication::CatalogOptions {
            max_connections: catalog_max_connections,
        },
    )
    .await
    .expect("failed to connect to database");

    // The content-addressed `BlobStore` is the ONLY deploy-artifact store.
    // `.zship` deploys land in `{prefix}/blobs/` + `{prefix}/manifests/`;
    // control writes through the SAME store gateway + worker read (local disk
    // for dev, S3 for production). The legacy per-app `BundleStore`/VFS is
    // gone. App archive retains the manifest keyspace so restore can publish
    // the same artifact without rebuilding or rewriting history.
    // The S3 inputs are read HERE, not inside `zeroship-bundle`, so the read is
    // recorded against this binary. Resolved only for a remote store: on local
    // disk the credentials are legitimately absent.
    let s3_runtime = store_url.is_remote().then(|| {
        zeroship_core::resolve_s3_runtime!(zeroship_control::config::ControlSettingsConsumer)
            .expect("failed to resolve S3 credentials for the blob store")
    });
    let blob_store: Arc<dyn BlobStore> = build_blob_store(&store_url, s3_runtime.as_ref())
        .expect("failed to initialise blob store");

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
    // (`--db`). The `AuthzGuard` bearer path, audit emitter, and
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

    // The first-party relying parties of the platform OP, from the config
    // overlay. Registering an RP of your own OP is a deployment decision, so it
    // happens once here rather than through an operator credential at runtime.
    //
    // FATAL on failure, deliberately: because nobody is watching a startup
    // registration, a skipped one is a login surface that silently does not exist.
    match zeroship_control::oauth_clients::reconcile_oauth_clients(
        &control_pg,
        configured_oauth_clients.as_deref(),
        &trusted_oauth_clients,
    )
    .await
    {
        Ok(report) => tracing::info!(
            registered = report.registered,
            pruned = report.pruned,
            "control: first-party OAuth clients reconciled from config"
        ),
        Err(message) => {
            eprintln!("control: {message}");
            std::process::exit(2);
        }
    }

    // The join signers this deployment trusts, from the operator's import
    // file. FATAL on refusal for the reason the OAuth reconcile above is:
    // nobody is watching, and a skipped import is every worker refused at join
    // while this process looks configured. A CONTRADICTING file refuses only
    // this replica, so a rolling deploy fails replicas one at a time with a
    // message naming the entries rather than leaving a fleet half-converted.
    match zeroship_control::worker_join::import_join_signers(
        &registry,
        settings.join_signers_file.get(),
    )
    .await
    {
        Ok(Some(report)) => tracing::info!(
            inserted = report.inserted,
            unchanged = report.unchanged,
            revoked = report.revoked,
            "control: join signers imported from config"
        ),
        Ok(None) => tracing::info!(
            "control: no join signer file configured; only signers recorded by an earlier \
             boot can admit workers"
        ),
        Err(message) => {
            eprintln!("control: {message}");
            std::process::exit(2);
        }
    }

    // The single-host join-token minter. Configured on a deployment where
    // nobody is present to mint by hand; one replica is elected and the rest
    // stand by. Refusing the boot on a broken credential is the same call as
    // the import above: a Control that was told to mint and cannot would leave
    // every worker without a token while looking configured.
    let join_minter = match load_join_minter(&settings) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("control: {message}");
            std::process::exit(2);
        }
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
    // Docker `js-packages` stage and COPYed to `/opt/zeroship/console/app.zship`; the
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

    // Tax provider: parse the kind, then build it. `native` (default)
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
    // Provider secrets are read straight out of `--provider-config` through the
    // platform secret grammar: the material, or `urn:zeroship:file:<path>`. A
    // fixed two-entry map would leave `lago` and `openmeter`, whose keys are
    // neither of the known ones, with no value an operator could write.
    let provider_ctx = zeroship_control::metering::provider::ProviderCtx::new(
        provider_config_json,
        Arc::new(zeroship_control::metering::provider::PlatformSecretResolver),
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
                .brokers
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(|_| "redpanda".to_string())
        });
    let effective_stream_config = if settings.stream_config.get().trim() != "{}"
        && !settings.stream_config.get().trim().is_empty()
    {
        settings.stream_config.get().clone()
    } else if let Some(brokers) = file_metering
        .brokers
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        // Matches zeroship_metering::DEFAULT_USAGE_EVENTS_TOPIC.
        let topic = file_metering
            .events_topic
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

    // Billing notifier: a `BillingNotifier` over the relocated `zeroship-mailer`
    // `Mailer` built above. Wraps the mailer + the per-message idempotency key.
    let notifier: Arc<dyn zeroship_control::notify::BillingNotifier> = Arc::new(
        zeroship_control::notify::MailerNotifier::new(Arc::clone(&mailer)),
    );
    tracing::info!(mailer = %mailer_kind, "control: mail transport selected");

    let service_auth = Arc::new(build_service_auth(
        service_keyring,
        service_peers,
        Arc::clone(&control_pg),
    ));

    let state = Arc::new(AppState {
        service_auth,
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
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(30, 60))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(50, 600))),
        trust_proxy,
        worker_enrolment,
        deploy_tmp_dir,
        control_pg,
        app_base_domain,
        origin_scheme,
        trusted_oauth_clients,
        expected_oauth_audience,
        // REFUSES TO BOOT on a policy set that does not validate against
        // deploy/policies/zeroship.cedarschema. Serving with a band Cedar
        // cannot evaluate would deny every request that band was written to
        // permit, and record it as an ordinary non-match.
        static_policies: zeroship_authz::load_platform_policies()
            .expect("control: bundled authz policies parse and validate against the schema"),
        auth_provider,
        provider_registry,
        billing_stack,
        billing_stream,
        tax_provider,
        notifier,
        mailer,
        pairwise_salt,
        projected_charge_cache: Arc::new(
            zeroship_control::billing_read::ProjectedChargeCache::default(),
        ),
    });

    // Spawn the in-process control crons:
    //   - audit_retention: sanctioned deleter for the append-only
    //     `zeroship.app_audit` + `zeroship.authz_decisions` tables (peer of the
    //     auth `audit_events` sweep; shares the `zeroship.audit_retention` GUC).
    //   - orphaned_app_reaper: purges apps left owner-less by the account-erase
    //     reaper (DB row + blobs), excluding the `system = true`
    //     platform console.
    // Both hold an `Arc<AppState>` clone (cheap) and open fresh per-tick
    // connections.
    zeroship_control::cron::spawn_all(
        Arc::clone(&state),
        audit_retention_months,
        audit_retention_check_secs,
        spend_recompute_interval,
    );
    tracing::info!(
        retention_months = audit_retention_months,
        check_secs = audit_retention_check_secs,
        "control: audit-retention + orphaned-app-reaper crons spawned"
    );

    // The worker health monitor. It probes each enrolled instance at the address
    // control DERIVED for it and holds the result in its own state; it issues no
    // write to `zeroship.worker_instances`, which is the property its live test
    // binds. See `worker_health` for why writing an observation into `status`
    // would turn a network blip into permanent eviction.
    //
    // THE VIEW IS PROCESS-LOCAL HERE, AND THAT IS SEQUENCING RATHER THAN AN
    // OVERSIGHT. Its reader is the per-app eligible set, which is not built yet.
    // `AppState` is a struct literal with no builder, so hanging an unread field
    // off it now would mean editing every construction site to carry something
    // nothing consumes. The change that READS the view is the change that should
    // move it onto the state, in the same patch as its first reader.
    let worker_health_view = Arc::new(zeroship_control::worker_health::HealthView::new());
    {
        let pg = Arc::clone(&state.control_pg);
        let view = Arc::clone(&worker_health_view);
        compio::runtime::spawn(async move {
            zeroship_control::worker_health::run(
                pg,
                view,
                zeroship_control::worker_health::DEFAULT_SWEEP_SECS,
            )
            .await;
        })
        .detach();
    }
    tracing::info!(
        sweep_secs = zeroship_control::worker_health::DEFAULT_SWEEP_SECS,
        "control: worker health monitor spawned"
    );

    // Deliver committed lifecycle intents - deploy activations, archive
    // disables and restore activations - to the workflow manager in per-app
    // revision order, on the shared catalog. The deploy and archive handlers
    // never call the manager, so a Control that cannot publish refuses to
    // start rather than accept deploys whose schedules never reach it.
    if let Err(error) = zeroship_control::publication::publisher::start(
        state.registry.catalog(),
        Arc::clone(&state.service_auth),
        &workflow_coordinator_url,
        zeroship_control::publication::publisher::PublisherConfig::default(),
    )
    .await
    {
        eprintln!("control: refusing to start: {error}");
        tracing::error!(%error, "control: refusing to start");
        std::process::exit(1);
    }

    // The single-host join-token minter, if this deployment configured one.
    // Election is inside the rotation: every candidate replica asks for the
    // lease and only the holder writes.
    //
    // THE FIRST ROTATION IS AWAITED HERE, BEFORE THE BIND, and it is fatal.
    // Deployments order their workers after this process is HEALTHY, and a
    // worker reads its token at boot with no retry, so a minter that started
    // rotating concurrently with the bind would let the first worker read an
    // empty volume and refuse its own boot. Being told to mint and not having
    // minted is the same fault as the signer import above: it would leave this
    // process looking configured while every worker failed to start.
    if let Some(minter) = join_minter {
        let audience = match internal::control_service_issuer() {
            Ok(audience) => audience,
            Err(error) => {
                eprintln!("control: this control plane's own issuer is malformed: {error}");
                std::process::exit(2);
            }
        };
        let pg = Arc::clone(&state.control_pg);
        match zeroship_control::join_minter::rotate_once(&pg, &minter, &audience).await {
            Ok(outcome) => tracing::info!(
                path = %minter.path.display(),
                zone = minter.zone.as_str(),
                ?outcome,
                "control: join token minter started"
            ),
            Err(message) => {
                eprintln!("control: join token minter: {message}");
                std::process::exit(2);
            }
        }
        compio::runtime::spawn(async move {
            zeroship_control::join_minter::run(pg, minter, audience).await;
        })
        .detach();
    }

    let bind_addr = format!("{bind_host}:{port}");
    tracing::info!(bind = %bind_addr, "zeroship-control listening");

    // ONE gate for the whole process, shared by every ntex worker thread, so
    // the TTL bounds probe-driven Postgres traffic per PROCESS rather than
    // per thread.
    let readiness = Arc::new(zeroship_core::readiness::ReadinessGate::with_defaults());

    web::server(async move || {
        let hold_state = state.clone();
        let coordinator_url = workflow_coordinator_url.clone();
        // The deploy path's journal client. Built per serving thread for the
        // same reason the hold client is: the HTTP client's pooled streams
        // belong to the thread that opens them. A refusal here cannot be
        // reached in a process that got this far - `publisher::start` above
        // built the same client from the same origin and signer, and exits if
        // it cannot - so this reports a bug rather than a configuration.
        let journal_url = workflow_coordinator_url.clone();
        let journal_auth = Arc::clone(&state.service_auth);
        web::App::new()
            .state(state.clone())
            .state(state.control_pg.clone())
            .state(readiness.clone())
            .state_factory(async move || {
                zeroship_control::deployment_hold_api::DeploymentHoldApi::connect(
                    &hold_state,
                    &coordinator_url,
                ).await.map(std::rc::Rc::new)
            })
            .state_factory(async move || {
                zeroship_control::publication::DeployJournal::connect(&journal_url, journal_auth)
                    .map(std::rc::Rc::new)
            })
            // --- Admin API ---
            .service(
                web::resource("/api/apps")
                    .route(web::post().to(api::create_app))
                    .route(web::get().to(api::list_apps)),
            )
            .service(
                web::resource("/api/apps/{id}")
                    .route(web::get().to(api::get_app))
                    // The terminal lifecycle verb, and the last step of the
                    // account-closure funnel. `DELETE` on the ARCHIVE resource
                    // below is the reversible restore, not this.
                    .route(web::delete().to(api::delete_app)),
            )
            .service(
                web::resource("/api/apps/{id}/archive")
                    .route(web::put().to(api::archive_app))
                    .route(web::delete().to(api::unarchive_app)),
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
            // --- Creator billing READ APIs (BillingRead) -------------
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
            // Creator self-service for the app's raw-TCP egress rules. The
            // creator names destinations and verdicts; the plan's caps and the
            // rule grammar bound what they may write, and an app with no
            // accept rule stays default-deny.
            .configure(zeroship_control::egress_rules::configure)
            // Organizations, projects, membership and invites - the ownership
            // root and the per-project narrowing. Mounted as one block because
            // every route in it authorizes against the same closed rank ladder
            // and every mutation shares one lock discipline.
            .configure(zeroship_control::organizations::configure)
            // Project-owned databases and the app-to-database bindings that
            // reach them. Mounted beside organizations because it authorizes
            // against the same project seat ladder and shares its lock
            // discipline; kept a separate module because a database is a
            // resource with its own Cedar entity type, not a membership fact.
            .configure(zeroship_control::databases::configure)
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
                web::resource("/api/organizations/{id}/stripe/onboard")
                    .route(web::post().to(stripe_handlers::onboard)),
            )
            .service(
                web::resource("/api/organizations/{id}/stripe/callback")
                    .route(web::post().to(stripe_handlers::callback)),
            )
            // --- Stream-2 Connect: server-stamped checkout ---
            .service(
                web::resource("/api/organizations/{id}/connect/checkout")
                    .route(web::post().to(stripe_handlers::connect_checkout)),
            )
            // --- Infrastructure-billing setup: platform Customer + card ---
            .service(
                web::resource("/api/organizations/{id}/billing/setup")
                    .route(web::post().to(stripe_handlers::billing_setup)),
            )
            .service(
                web::resource("/api/organizations/{id}/stripe")
                    .route(web::delete().to(stripe_handlers::unlink)),
            )
            .service(
                web::resource("/api/organizations/{id}/earnings")
                    .route(web::get().to(stripe_handlers::earnings)),
            )
            // --- Internal API ---
            // Every route in it takes its path from the declaration its
            // handler authorizes against, so the served route and the
            // authorized route cannot disagree.
            .configure(internal::configure)
            // The erasure seam: the auth service asks, before it opens the
            // grace window and again before the reaper deletes, whether this
            // human is the last owner of anything.
            .configure(erasure::configure)
            .configure(zeroship_control::deployment_hold_api::configure)
            .service(
                web::resource("/internal/webhooks/stripe")
                    // Give the Bytes extractor headroom above the handler's body cap so
                    // the handler (not the extractor's default-256KiB 400) owns the
                    // oversized-body rejection with its descriptive 413.
                    .state(stripe_handlers::webhook_payload_config())
                    .route(web::post().to(stripe_handlers::webhook)),
            )
            .configure(health::configure)
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
    use zeroship_core::config::GeneratedConfig;

    // Tests that drive the ENVIRONMENT tier of `ControlSettings` live in
    // `crates/zeroship-control/tests/config_env_tier.rs`. That tier is clap's
    // `env = "ZEROSHIP_..."` attribute, so exercising it in-process means
    // `std::env::set_var` / `remove_var` - which mutates the environment every
    // other test in this binary parses in. They run the real `zeroship-control`
    // under `--check-config` with `Command::env`, scoping the environment to the
    // child.
    //
    // What lives here is everything that needs no environment at all.

    #[test]
    fn control_blob_store_flag_uses_unified_name() {
        let cli =
            ControlSettingsSources::try_parse_from(["zeroship-control", "--blob-store", "/tmp/blob-root"])
                .expect("blob-store flag should parse");

        assert_eq!(cli.blob_store.as_deref(), Some("/tmp/blob-root"));
    }

    #[test]
    fn the_retired_platform_spelling_is_rejected_by_the_flag() {
        // `platform` was control's own word for the state now spelled `native`.
        // Both tiers must refuse it, or the two vocabularies survive the merge
        // in the one place an operator would not look.
        //
        // ONLY THE FLAG HALF IS HERE. An explicit `--auth-provider platform`
        // is rejected by clap's value parser without the environment being
        // consulted at all, so this half is hermetic in-process. The OVERLAY
        // half is not: the environment tier outranks the overlay, so an
        // ambient `ZEROSHIP_AUTH_PROVIDER` makes the retired overlay value
        // never get parsed and the refusal never fire. It lives in
        // `crates/zeroship-control/tests/config_env_tier.rs`, against a child process
        // whose environment is cleared.
        let err = ControlSettingsSources::try_parse_from([
            "zeroship-control",
            "--auth-provider",
            "platform",
        ])
        .map(|_| ())
        .expect_err("the retired control spelling must not parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
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
            // Secret strength. The shared validator carries control's label.
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
        // This pins that every diagnostic about an environment variable names a
        // variable this binary actually reads: a diagnostic naming a variable
        // control does not read sends an operator to edit the wrong thing. It
        // covers the auth-provider diagnostics, the Supabase provider config and
        // the secret-strength refusals (bare `SUPABASE_JWT_SECRET` and
        // `PAIRWISE_SALT` spellings no binary reads).
        //
        // The scanner and the derived readable set are the parts worth having
        // exactly once, so this test is extended rather than duplicated.
        //
        // What this does NOT catch: a diagnostic that names a variable control
        // really does read but that is the WRONG one for the failure at hand,
        // any stale name in a diagnostic outside the two sets driven below, and
        // a stale spelling that happens to be a SUBSTRING of a live name (the
        // scanner tokenises, so `CONTROL_KEY` inside `ZEROSHIP_CONTROL_KEY` is not
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

    // resolve_trusted_oauth_clients distinguishes absent / present.
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
        for gone in ["MASTER_KEY", "DATABASE_URL", "CONTROL_KEY", "PAIRWISE_SALT"] {
            assert!(
                !declared.contains(&(*gone).to_owned()),
                "the bare name {gone} survives"
            );
        }

        // Does NOT cover whether the environment is actually READ at that name;
        // the config_env_tier integration target drives the generated resolver.
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
