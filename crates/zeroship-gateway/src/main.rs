#![recursion_limit = "256"]

//! `zeroship-gate` binary entry point. Thin shell over the
//! [`zeroship_gateway`] library: parse flags, build [`GateState`],
//! register routes, run.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use ntex::web;
use zeroship_core::config::{
    audit_credentials, bootstrap_or_exit, mark_dev_escape_active, require_nonempty,
    validate_pairwise_salt, validate_stash_key, BuildProfile, CheckConfigReport, CheckValue,
    CredentialPosture, CredentialVerdict, SubsystemCredential,
};
use zeroship_bundle::{build_blob_store, BlobStore, StoreUrl};
use zeroship_gateway::config::{GateSettings, GateSettingsSources};
use zeroship_gateway::{
    auth_token, backchannel_logout, blob_cache, browser_auth, enforce, health, idempotency,
    oidc_rp, proxy, router, session_token, signing, sync, GateConfig, GateState,
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Operator-facing spelling of the control key, for a diagnostic that has to
/// name something the operator can actually set.
const CONTROL_KEY_LABEL: &str = "ZEROSHIP_CONTROL_KEY / --control-key-file";
/// Operator-facing spelling of the gateway stash signing key. Auth reads a
/// DIFFERENT variable behind the same validator, which is why the validator
/// takes the name as a parameter rather than spelling one itself. Derived from
/// `#[config(name = "gateway.stash_signing_key")]` in `config.rs`.
const STASH_SIGNING_KEY_LABEL: &str =
    "ZEROSHIP_GATEWAY_STASH_SIGNING_KEY / --stash-signing-key-file";
/// Operator-facing spelling of the shared pairwise salt, whose identity is
/// declared in `crates/zeroship-config-macros/src/shared.rs` as `canonical:
/// "pairwise_salt"`.
const PAIRWISE_SALT_LABEL: &str = "ZEROSHIP_PAIRWISE_SALT / --pairwise-salt-file";
/// Operator-facing spelling of the platform broker master secret file.
/// Substituted into the shared validator's message, which names auth's.
const BROKER_SECRET_LABEL: &str =
    "ZEROSHIP_GATEWAY_BROKER_SECRET_FILE / --broker-secret-file";

/// Parse the comma-separated `--workers`/`WORKER_URLS` list into a clean
/// vector, trimming whitespace and dropping empty entries. Parsed ONCE so
/// the check-config count and the runtime hash ring can never disagree
/// (M7) — previously check-config filtered empties while the runtime kept
/// them, so `a,,b` reported 2 workers but routed across 3 (one empty URL).
fn parse_worker_urls(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Every credential the gateway needs, TAGGED BY THE SUBSYSTEM THAT NEEDS IT.
///
/// The per-subsystem property of the boot gate: a deployer who has not enabled
/// a subsystem is never blocked on its credential. The gateway's three are all
/// unconditional today - route sync, the OIDC stash and the pairwise anchor are
/// not optional in a gateway that serves anything - and `enabled: true` states
/// that as a positive claim rather than leaving the dimension absent.
/// `crates/zeroship-migrate-server` is where a `false` actually appears.
///
/// The worker-dispatch credential was a fourth row and is not one now. It has
/// not become optional: it moved to `service_key_file`, a PATH the loader reads
/// and refuses when it is unreadable or group-readable. A strength floor on a
/// shared string could express neither check.
///
/// Each row carries the validator `main` runs, so
/// [`zeroship_core::config::audit_credentials`] runs the check itself rather
/// than a second spelling of it, and it handles the three material cases
/// (`Some`, configured-but-unread on a dry run, unsupplied) exactly as
/// `validate_secret_material` did.
fn gateway_credentials(settings: &GateSettings) -> Vec<SubsystemCredential<'_>> {
    vec![
        SubsystemCredential {
            subsystem: "control-route-sync",
            enabled: true,
            label: CONTROL_KEY_LABEL,
            secret: &settings.control_key,
            validate: require_nonempty,
        },
        SubsystemCredential {
            subsystem: "browser-oidc-stash",
            enabled: true,
            label: STASH_SIGNING_KEY_LABEL,
            secret: &settings.stash_signing_key,
            validate: validate_stash_key,
        },
        // A missing or weak salt aborts boot: the per-app `pws_` anchor must be
        // a strong, stable, operator-set secret.
        SubsystemCredential {
            subsystem: "pairwise-subject-anchor",
            enabled: true,
            label: PAIRWISE_SALT_LABEL,
            secret: &settings.pairwise_salt,
            validate: validate_pairwise_salt,
        },
    ]
}

/// Apply the boot gate, or exit.
///
/// The shared shape every service repeats: audit, take a verdict from the build
/// profile and whether this is a dry run, print the banner, and either exit or
/// record that the process is running on the escape. Returns the posture so the
/// caller can publish it in `--check-config`.
fn enforce_gateway_credentials(
    settings: &GateSettings,
    overlay: &zeroship_core::config::ConfigSource,
    check_config: bool,
) -> CredentialPosture {
    let posture = audit_credentials(&gateway_credentials(settings));
    let verdict = posture.verdict(BuildProfile::current(), check_config);
    if let Some(banner) = posture.banner("zeroship-gate", overlay, verdict) {
        // BOTH sinks, deliberately. Tracing may be JSON-formatted and shipped
        // somewhere nobody is watching during a bring-up; stderr is what the
        // operator running `docker compose up` actually sees.
        eprint!("{banner}");
        tracing::error!(
            subsystems = %posture
                .weak()
                .iter()
                .map(|weak| weak.subsystem)
                .collect::<Vec<_>>()
                .join(","),
            "gateway: unconfigured service credential"
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

/// A [`ReplayStore`](zeroship_core::service_assertion::ReplayStore) over the
/// gateway's per-worker-thread Postgres pool.
///
/// The gateway is a callee on exactly one internal edge -
/// `/oidc/backchannel-logout` - and that edge takes the FULL profile, so the
/// `jti` must be claimed in a store every gateway replica shares. The
/// gateway's `Pool` is `!Send` and lives in a thread-local, so the checkout
/// happens inside `claim` rather than being held on the store.
///
/// **This is the one consumer of the gateway's database credential that the
/// auth redesign's step 6 does NOT delete along with the anchor and RP paths.**
/// That step's F3 says the gateway holds no database credential at all, which
/// is incompatible with a shared replay store terminating here. The tension is
/// recorded rather than resolved: whoever lands step 6 has to either move this
/// edge off the gateway or re-tier it, and finding this comment is how they
/// learn that.
struct PoolReplayStore {
    db: zeroship_gateway::db::DbConfig,
}

impl zeroship_core::service_assertion::ReplayStore for PoolReplayStore {
    fn claim<'a>(
        &'a self,
        key: &'a str,
        expires_at: std::time::SystemTime,
    ) -> zeroship_core::service_assertion::ClaimFuture<'a> {
        Box::pin(async move {
            let pool = zeroship_gateway::db::checkout(&self.db).await.map_err(|error| {
                zeroship_core::service_assertion::ReplayStoreError(error.to_string())
            })?;
            let client = pool.acquire().await.map_err(|error| {
                zeroship_core::service_assertion::ReplayStoreError(error.to_string())
            })?;
            zeroship_authn::service_replay::claim_replay_key(&*client, key, expires_at).await
        })
    }
}

/// Load this gateway's service identity, or refuse to start.
///
/// THERE IS NO UNCONFIGURED ARM, and its absence is the whole of fence F4 in
/// `docs/proposals/2026-09-05-auth-foundation-redesign.md`. This function used
/// to return `ServiceAuth::unconfigured()` when neither file was configured, on
/// the reasoning that every inbound internal edge would then refuse and every
/// dispatch would carry no credential. Both halves of that were true and it was still
/// the wrong answer: a gateway in that state binds its port, answers a liveness
/// probe and looks healthy to an orchestrator, and the first thing that notices
/// is an end user whose request the worker turns away. Refusing here moves the
/// failure to deploy time, when someone is watching.
///
/// The refusal itself lives in `ServiceKeyring::load`, which is what makes an
/// empty path indistinguishable from a wrong one HERE while staying two
/// distinct messages for the operator - a caller cannot reintroduce the escape
/// by writing its own empty-path branch, because there is nothing left for such
/// a branch to do.
///
/// The database requirement is checked AFTER the key material, so a deployment
/// missing both is told about the key material first: that is the one an
/// operator must fix whatever they decide about the inbound edges.
fn build_service_auth(
    key_file: &std::path::Path,
    peers_file: &std::path::Path,
    db: Option<zeroship_gateway::db::DbConfig>,
) -> zeroship_core::service_peers::ServiceAuth {
    use zeroship_core::service_assertion::ServiceAssertionVerifier;
    use zeroship_core::service_peers::{service_issuer, ServiceAuth, ServiceKeyring};

    let issuer = match service_issuer(zeroship_core::service_peers::GATEWAY_SERVICE_NAME) {
        Ok(issuer) => issuer,
        Err(error) => {
            tracing::error!(%error, "gateway: refusing to start - gateway service issuer is malformed");
            std::process::exit(1);
        }
    };
    let mut keyring = match ServiceKeyring::load(issuer, key_file, peers_file) {
        Ok(keyring) => keyring,
        Err(error) => {
            tracing::error!(
                %error,
                "gateway: refusing to start - service key material rejected; set \
                 gateway.service_key_file and gateway.service_peers_file"
            );
            std::process::exit(1);
        }
    };
    let Some(db) = db else {
        tracing::error!(
            "gateway: refusing to start - service key material is configured but no database \
             is, and the inbound backchannel-logout edge's single-use claim needs the \
             shared store"
        );
        std::process::exit(1);
    };
    let Some(bundle) = keyring.take_bundle() else {
        tracing::error!("gateway: refusing to start - peer bundle already taken");
        std::process::exit(1);
    };
    let replay = Arc::new(PoolReplayStore { db });
    ServiceAuth::new(keyring, Arc::new(ServiceAssertionVerifier::new(bundle, replay)))
}

fn main() -> std::io::Result<()> {
    let (settings, boot) = bootstrap_or_exit::<GateSettings>(
        GateSettingsSources::parse(),
        zeroship_gateway::config::DEFAULT_LOG_FILTER,
        "gateway",
    );
    let check_config = *settings.check_config.get();

    let origin_scheme = *settings.origin_scheme.get();
    let trusted_origins = settings.trusted_origins.get().clone();
    let trust_proxy = *settings.trust_proxy.get();
    let port = *settings.port.get();
    let bind_host = settings.bind.get().clone();
    let control_url = settings.control_url.get().clone();
    // Refuse a scheme this transport cannot honour, the same way `--blob-store`
    // below refuses a store URL it cannot parse. Without this an `https://`
    // control URL is silently downgraded to plaintext on port 80 and the
    // control key goes out in the clear.
    if let Err(e) = zeroship_gateway::sync::validate_control_url(&control_url) {
        eprintln!("gateway: invalid --control-url: {e}");
        std::process::exit(2);
    }
    // Secret material, already resolved by the generated resolver from the
    // `-file` flag, the canonical environment name, or the canonical TOML path -
    // in that order. Under `--check-config` a source needing I/O yields no
    // material at all, and these locals are then empty by construction; the
    // report below asks `is_configured()` and the boot path is not reached.
    let control_key = settings.control_key.expose_str().to_owned();
    // M7 — parse the worker URL list ONCE (rejecting empty/whitespace
    // entries) and reuse it for both the check-config count and the
    // runtime hash ring, so the two can never disagree.
    let worker_urls = parse_worker_urls(settings.worker_urls.get());
    let poll_interval = *settings.poll_interval.get();
    let blob_store_root = settings.blob_store.get().clone();
    // Classify the `--blob-store` value: `s3://…` → remote S3, bare path →
    // local disk (dev default). An `s3://` URL is validated now so a
    // misconfiguration fails fast at startup / check-config.
    let store_url = match StoreUrl::parse(&blob_store_root) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("gateway: invalid --blob-store: {e}");
            std::process::exit(2);
        }
    };
    let blob_store_is_remote = store_url.is_remote();
    let blob_cache_mem_mb = *settings.blob_cache_mem_mb.get();
    let blob_cache_disk_gb = *settings.blob_cache_disk_gb.get();
    let blob_cache_disk_root = settings.blob_cache_disk_root.get().clone();
    let auth_ui_url = settings.auth_ui_url.get().clone();
    let db_pool_size = (*settings.db_pool_size.get()).max(1);
    // DSN carries the database password, so it is secret-classed like the keys
    // (a literal DSN, colons and all, is supplied and consumed unchanged).
    let pg_dsn = settings.database_url.expose_str().to_owned();
    let stash_signing_key = settings.stash_signing_key.expose_str().to_owned();
    let pairwise_salt_secret = settings.pairwise_salt.expose_str().to_owned();
    // File-PATH settings (they name a file to read), NOT secret values: the
    // loaders below own the PEM-versus-DER sniff, the raw-byte read and the
    // permission check.
    let broker_secret_path = settings.broker_secret_file.get().clone();
    let signing_key_path = settings.signing_key_file.get().clone();
    let prev_signing_key_path = settings.prev_signing_key_file.get().clone();
    let public_url = settings.public_url.get().clone();

    // THE BOOT GATE. It runs BEFORE the `--check-config` report below, which is
    // what makes a dry run over a placeholder credential exit non-zero instead
    // of printing a truthful field into a pipe nobody reads
    // (docs/proposals/2026-08-20-metering-transport-not-configured.md).
    let credentials = enforce_gateway_credentials(&settings, &boot.overlay.source, check_config);

    // Load the gateway's session-cookie signing key. The setting is optional:
    // when empty, the boot succeeds but the signed session cookie cannot be
    // issued/verified, so the cookie auth arm fails closed. We log a clear
    // warning so operators don't get a surprise.
    let signing_key: Option<Arc<ed25519_dalek::SigningKey>> =
        if signing_key_path.as_os_str().is_empty() {
            tracing::warn!(
                "ZEROSHIP_GATEWAY_SIGNING_KEY_FILE not set - signed session cookies disabled \
                 (cookie auth fails closed)"
            );
            None
        } else {
            let key =
                signing::load_from_path(&signing_key_path).expect("gateway: load signing key");
            let kid = signing::jwk_thumbprint(&key);
            tracing::info!(
                path = %signing_key_path.display(),
                kid = %kid,
                "gateway signing key loaded"
            );
            Some(Arc::new(key))
        };

    // Load the PREVIOUS session-cookie signing key for the rotation overlap.
    // Set ONLY during a key roll. When present,
    // the session Verifier is built via `Verifier::with_previous` (accepts
    // session cookies signed by EITHER key). Ignored (with a warning) when no
    // current key is configured, since there is nothing to overlap with.
    let prev_signing_key: Option<Arc<ed25519_dalek::SigningKey>> =
        if prev_signing_key_path.as_os_str().is_empty() {
            None
        } else if signing_key.is_none() {
            tracing::warn!(
                "ZEROSHIP_GATEWAY_PREV_SIGNING_KEY_FILE set but no current signing key - \
                 ignoring (a previous key needs a current key to overlap with)"
            );
            None
        } else {
            let key = signing::load_from_path(&prev_signing_key_path)
                .expect("gateway: load previous signing key");
            let kid = signing::jwk_thumbprint(&key);
            tracing::info!(
                path = %prev_signing_key_path.display(),
                kid = %kid,
                "gateway PREVIOUS signing key loaded (rotation overlap active)"
            );
            Some(Arc::new(key))
        };

    // BFF redesign slice R1b — the SIGNED STATELESS session cookie. Built from
    // the ed25519 signing key (+ previous key for the rotation overlap),
    // stamping the distinct `zeroship-sess+jwt` typ.
    // Both `Some`, or both `None` (one-to-one with `signing_key`): with no key
    // the gateway cannot sign/verify the session cookie, so the cookie arm fails
    // closed. The Issuer always signs with the CURRENT key; the Verifier folds
    // in the previous key during an overlap so a cookie minted just before a
    // roll still verifies for its ~15 min life.
    let session_issuer: Option<Arc<session_token::Issuer>> = signing_key.as_ref().map(|sk| {
        let issuer = session_token::Issuer::new(sk.as_ref(), public_url.clone())
            .expect("session_token::Issuer construction");
        Arc::new(issuer)
    });
    let session_verifier: Option<Arc<session_token::Verifier>> = signing_key.as_ref().map(|sk| {
        let current = sk.verifying_key();
        let verifier = match prev_signing_key.as_ref() {
            Some(prev) => session_token::Verifier::with_previous(
                &current,
                &prev.verifying_key(),
                public_url.clone(),
            ),
            None => session_token::Verifier::new(&current, public_url.clone()),
        };
        Arc::new(verifier)
    });

    if check_config {
        let log_format = boot.log_format.to_string();
        let mut report = CheckConfigReport::new();
        report.field("port", CheckValue::Count(usize::from(port)));
        report.field("bind", CheckValue::Plain(bind_host.clone()));
        report.field(
            "config_source",
            CheckValue::Plain(boot.overlay.source.to_string()),
        );
        report.field("control_url", CheckValue::Plain(control_url));
        report.field("auth_ui_url", CheckValue::Plain(auth_ui_url));
        report.field("origin_scheme", CheckValue::Plain(origin_scheme.to_string()));
        // The ORIGINS, not their count. A trusted origin is a public host, not
        // a credential, so there is nothing to withhold; and the count could
        // not answer the question an operator actually asks of this report -
        // which origins does this gateway trust, and did my overlay reach it.
        // `crates/zeroship-gateway/tests/config_env_tier.rs` is the reader that needs
        // the values: a count cannot tell a right-sized list from the wrong
        // configuration tier.
        report.field(
            "trusted_origins",
            CheckValue::Plain(
                trusted_origins
                    .iter()
                    .map(zeroship_core::config::TrustedOrigin::as_str)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        );
        report.field("log_filter", CheckValue::Plain(boot.log_filter.clone()));
        report.field("log_format", CheckValue::Plain(log_format));
        report.field("trust_proxy", CheckValue::Flag(trust_proxy));
        report.field("blob_store", CheckValue::Plain(blob_store_root));
        report.field("blob_store_remote", CheckValue::Flag(blob_store_is_remote));
        report.field(
            "blob_cache_mem_mb",
            CheckValue::Count(blob_cache_mem_mb),
        );
        report.field(
            "blob_cache_disk_gb",
            CheckValue::Count(usize::try_from(blob_cache_disk_gb).unwrap_or(usize::MAX)),
        );
        report.field(
            "blob_cache_disk_root",
            CheckValue::Plain(blob_cache_disk_root),
        );
        report.field(
            "poll_interval_secs",
            CheckValue::Count(usize::try_from(poll_interval).unwrap_or(usize::MAX)),
        );
        report.field("workers_count", CheckValue::Count(worker_urls.len()));
        // Every secret is reported by PRESENCE, and presence is all a resolved
        // `Secret<T>` will answer. `!value.is_empty()` used to stand in for that
        // and could not: under `--check-config` a file-sourced secret has no
        // material, so the old test read "unset" for a correctly configured
        // deployment.
        report.field(
            "db_configured",
            CheckValue::Secret(settings.database_url.is_configured()),
        );
        report.field(
            "signing_key_configured",
            CheckValue::Secret(!signing_key_path.as_os_str().is_empty()),
        );
        report.field(
            "gateway_broker_secret_configured",
            CheckValue::Secret(!broker_secret_path.as_os_str().is_empty()),
        );
        report.field(
            "pairwise_salt_configured",
            CheckValue::Secret(settings.pairwise_salt.is_configured()),
        );
        // The posture, PLUS how much of it was measured. `service_credentials
        // = configured` over zero checked credentials is the vacuous green this
        // report must not be able to print, so the counts ride alongside it.
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

    ntex::rt::System::build()
        .name("zeroship-gate")
        .build(ntex::rt::DefaultRuntime)
        .block_on(async move {
    let blob_cache_bytes: usize = blob_cache_mem_mb.saturating_mul(1024 * 1024);
    let disk_cache_bytes: u64 = blob_cache_disk_gb.saturating_mul(1024 * 1024 * 1024);
    // Read here rather than inside `zeroship-bundle`, so the record names the
    // gateway as the reader. Only a remote store needs credentials.
    let s3_runtime = store_url.is_remote().then(|| {
        zeroship_core::resolve_s3_runtime!(zeroship_gateway::config::GateSettingsConsumer)
            .expect("failed to resolve S3 credentials for the blob store")
    });
    let blob_store: Arc<dyn BlobStore> = build_blob_store(&store_url, s3_runtime.as_ref())
        .expect("failed to initialise blob store");
    let disk_cache = blob_cache::DiskBlobCache::new(
        PathBuf::from(&blob_cache_disk_root),
        disk_cache_bytes,
    )
    .expect("failed to initialise disk blob cache");
    tracing::info!(
        blob_store_root = %blob_store_root,
        blob_cache_mem_mb = %blob_cache_mem_mb,
        blob_cache_disk_root = %blob_cache_disk_root,
        blob_cache_disk_gb = %blob_cache_disk_gb,
        "gateway blob store + cache configured"
    );

    let num_workers = worker_urls.len();
    // Bounded load: each worker handles at most 125% of average load
    // With 10K apps and 10 workers, avg = 1K apps → max = 1.25K
    // For request concurrency, use a generous static bound
    let max_per_worker = 500u32;

    tracing::info!(
        workers = num_workers,
        vnodes = 150,
        max_per_worker,
        "gateway routing configured (CHWBL)"
    );

    let hash_ring = proxy::HashRing::new(worker_urls.clone(), max_per_worker);

    // Postgres connection config for the per-origin session store and the
    // anchor/revocation read/write paths. The binary accepts an empty
    // DSN (`--db ""`) for dev / smoke modes that don't exercise the OIDC
    // RP path; downstream handlers gracefully return 401 when `db` is
    // None instead of panicking.
    //
    // `GateState.db` carries only the `Send + Sync` connection params: the
    // compio-postgres `Pool` is `!Send`, so the real pool is built lazily
    // **per ntex worker thread** in a thread-local (see `crate::db`).
    // Every per-request DB touch checks out a pooled connection for ONE
    // operation and releases it on drop, so no single shared connection
    // serializes gateway DB work.
    let db: Option<zeroship_gateway::db::DbConfig> = if pg_dsn.is_empty() {
        tracing::warn!(
            "ZEROSHIP_GATEWAY_DATABASE_URL / --database-url-file not set; gateway session validation disabled (all auth-gated requests will 401)"
        );
        None
    } else {
        let db_cfg = zeroship_gateway::db::DbConfig::new(pg_dsn.clone(), db_pool_size);
        tracing::info!(
            db_pool_size = db_cfg.pool_size(),
            "gateway pg connection pool configured (per-worker)"
        );
        Some(db_cfg)
    };

    // THE KEY-MATERIAL REFUSAL, hoisted out of `GateState` below so it lands
    // before the broker master secret is read. Both are boot refusals, so the
    // only thing the order decides is which one an operator who is missing both
    // is told about first - and this is the one that has to be right whatever
    // else is: without it the gateway signs no `ZeroShip-User` envelope and
    // mints no assertion, so every dispatch fails at the worker's door. See
    // `build_service_auth` for why there is no longer a boot-anyway arm.
    let service_auth = Arc::new(build_service_auth(
        settings.service_key_file.get(),
        settings.service_peers_file.get(),
        db.clone(),
    ));

    // OIDC RP — services every `{app}.zeroship.ai` host. The
    // `client_id` must match the client this gateway host is registered
    // as with the OP; `redirect_uri` is per-app and built at the
    // dispatch site.
    let stash_signing_key_bytes = stash_signing_key.into_bytes();

    // AES-256-GCM key for the server-held refresh family at rest in
    // `zeroship.app_session_anchors.refresh_token_enc`.
    // Derived from the (server-only) stash signing key via
    // `core::crypto::derive_key` so no new CLI flag is needed and the
    // refresh family never sits in PG in plaintext. Domain-separated by the
    // derive prefix; rotating the stash key rotates this key too (acceptable
    // pre-launch — a roll just forces re-login, which the anchor design
    // already tolerates via OP invalid_grant → login_required).
    let anchor_enc_key = {
        let seed = format!(
            "anchor-refresh-enc:{}",
            String::from_utf8_lossy(&stash_signing_key_bytes)
        );
        zeroship_core::crypto::derive_key(&seed)
    };

    // auth-sdk §6.2 — platform-wide pairwise salt for the per-app `pws_…`
    // subject projection. Derived via the SHARED helper so the gateway and the
    // control plane (which revokes the per-app token family on a dashboard
    // "disconnect app", Batch A fix 4) produce byte-identical `pws_…` subjects.
    // Seeded from the DEDICATED `PAIRWISE_SALT` secret (NOT the rotatable stash
    // key): `pws_` is the PERMANENT per-app identity anchor that apps store as a
    // user FK, so its seed must be independent of operational-key rotation. The
    // SAME `PAIRWISE_SALT` value must be configured on gateway + control.
    // Domain-separated from `anchor_enc_key` by the helper's distinct prefix.
    let pairwise_salt = zeroship_core::auth::derive_pairwise_salt(pairwise_salt_secret.as_bytes());

    // Read HERE, not before the `--check-config` return, because a dry run must
    // not open the file. RAW BYTES, and auth's `load_broker_master_secret`
    // reads the same file the same way: the per-client `oac_` derivation on the
    // two sides has to see identical input.
    let broker_bytes = signing::load_broker_master_secret(&broker_secret_path).unwrap_or_else(|e| {
        tracing::error!(
            error = %e,
            "gateway: refusing to start without a readable {BROKER_SECRET_LABEL}"
        );
        std::process::exit(1);
    });
    let broker_secret = oidc_rp::BrokerSecret::from_bytes(broker_bytes).unwrap_or_else(|message| {
        tracing::error!(
            error = %message.replace("AUTH_BROKER_SECRET_FILE", BROKER_SECRET_LABEL),
            "gateway: refusing to start with unsafe broker master secret"
        );
        std::process::exit(1);
    });

    let oidc_rp = Arc::new(oidc_rp::OidcRp::new(
        &auth_ui_url,
        broker_secret,
        stash_signing_key_bytes,
    ));

    // ── Metering infrastructure (coverage #27) ───────────────────────────
    // The gateway is a SECOND usage producer. ONE process-wide meter, shared
    // into `GateState` (so the response path records `gateway_egress_bytes` for
    // static/redirect/error bodies the worker never sees) AND drained by the
    // usage outbox spawned just below. Mirrors the worker: same `Meter`, same
    // shared `build_usage_outbox` producer → the billing stream. The
    // `gate-…-<uuid>` source is unique per boot so gateway and worker producer
    // ids never collide; their events simply SUM in `usage_aggregates`.
    // `gate_base` is the part that must stay STABLE across restarts, because
    // the WAL is named for it. The uuid below is the part that must CHANGE per
    // boot, because two live producers must not share a client id. They were
    // one string until the WAL turned out to be keyed on the changing half.
    let gate_base = zeroship_core::declared_env!(external, "HOSTNAME", zeroship_gateway::config::GateSettingsConsumer)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("gate-{port}"));
    let gate_meter_source = format!("gate-{gate_base}-{}", uuid::Uuid::new_v4());
    let gate_wal = zeroship_metering::wal_identity("gate", &gate_base);
    let meter = Arc::new(zeroship_metering::Meter::with_source(gate_meter_source.clone()));

    let state = Arc::new(GateState {
        service_auth,
        config: GateConfig {
            control_url,
            control_key,
            worker_urls,
            poll_interval_secs: poll_interval,
            auth_ui_url,
            origin_scheme,
            trusted_origins,
            trust_proxy,
            public_url,
        },
        routes: sync::RouteCache::new(),
        hash_ring,
        // TODO(S5 throughput backstop, billing-provider-platform design v7
        // Pillar 4/5): make this plan-aware so FREE-tier apps get a tighter
        // default per-app cap. The current registry is global; wiring the
        // route's plan into limiter defaults belongs in the gateway config
        // slice, not in the usage-aggregate recompute writer.
        rate_limiters: enforce::RateLimitRegistry::new(1000, 2000),
        per_rule_rate_limits: enforce::PerRuleRateLimitRegistry::new(),
        concurrency: enforce::ConcurrencyRegistry::new(100),
        blob_store,
        blob_cache: blob_cache::BlobCache::new(blob_cache_bytes),
        disk_cache,
        idempotency_store: Arc::new(idempotency::InMemoryIdempotencyStore::new()),
        oidc_rp,
        db,
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        revocation_cache: Arc::new(zeroship_authz::wrapper_revocation::RevocationCache::new()),
        signing_key,
        prev_signing_key,
        session_issuer,
        session_verifier,
        anchor_enc_key,
        pairwise_salt,
        meter: Arc::clone(&meter),
    });

    let readiness = Arc::new(health::GatewayReadiness::new(
        state.routes.sync_freshness_handle(),
        std::time::Duration::from_secs(state.config.poll_interval_secs),
        zeroship_core::config::dev_escape_active(),
    ));

    sync::start_sync(state.clone());

    let bind_addr = format!("{bind_host}:{port}");

    // Spawn the gateway usage-event outbox (coverage #27). Drains the gateway's
    // meter every ~10s and publishes UsageEvents to the billing stream via the
    // SAME shared producer wiring as the worker, and now from the same
    // operator-visible identity: the four `metering.*` declarations, whose
    // resolver has already applied flag > `ZEROSHIP_METERING_*` > `[metering]`
    // overlay > default. Disabled (drain-and-drop) when no brokers resolve.
    // Detached - never on the proxy hot path.
    let gate_stream_settings = zeroship_gateway::config::usage_stream_settings(&settings);
    match zeroship_metering::build_usage_outbox(
        &gate_meter_source,
        &gate_wal,
        &gate_stream_settings,
    ) {
        Ok(Some((outbox, outbox_config))) => {
            let topic = outbox.topic().to_string();
            zeroship_metering::spawn_outbox_task(Arc::clone(&meter), outbox, outbox_config);
            tracing::info!(producer = %gate_meter_source, topic = %topic, "gateway usage-event outbox started");
        }
        // Announced, not fatal - see the worker's matching arm for why a
        // no-metering deployment stays supported and what makes it visible.
        Ok(None) => {
            zeroship_metering::spawn_disabled_drain_task(
                Arc::clone(&meter),
                zeroship_metering::DEFAULT_OUTBOX_INTERVAL,
                "no --metering-brokers / ZEROSHIP_METERING_BROKERS".to_string(),
            );
        }
        // FATAL, matching the worker. Brokers are configured, so the operator
        // intends this gateway to bill; the common cause on a stable WAL path
        // is a co-located producer holding the single-writer redb lock. The
        // old arm degraded to a drain-and-drop task, which loses every event
        // for the life of the process - permanent total loss substituted for
        // intermittent partial loss.
        Err(error) => {
            tracing::error!(
                producer = %gate_meter_source,
                wal = %gate_wal.as_str(),
                error = %error,
                "gateway usage outbox could not be built; refusing to boot \
                 rather than dropping billable usage"
            );
            return Err(std::io::Error::other(format!(
                "gateway usage outbox could not be built (wal={}): {error}",
                gate_wal.as_str()
            )));
        }
    }
    tracing::info!(bind = %bind_addr, "gateway listening");

    web::server(async move || {
        web::App::new()
            .state(state.clone())
            .state(readiness.clone())
            .service(
                // ntex's `{path:.*}` only matches a single segment;
                // `{tail}*` is the tail-match syntax that handles
                // nested asset paths like `assets/index-abc.js`.
                // The creator-app body cap, shared with the worker. Without an
                // explicit PayloadConfig ntex applies its own 256 KiB default,
                // which would cap every creator app at a sixteenth of the
                // documented limit and answer with a bare framework 400 that
                // names neither the limit nor the tier that imposed it.
                web::resource("/apps/{app_name}/{tail}*")
                    .state(web::types::PayloadConfig::new(
                        zeroship_core::dispatch_frame::MAX_REQUEST_BODY_BYTES,
                    ))
                    .route(web::route().to(router::handle)),
            )
            .configure(health::configure)
            // The ONE identity-session
            // resource. `/token` is GONE (merged here); both methods live on
            // `/__zeroship/auth/session`:
            //   - POST = code→token exchange + create anchor + ISSUE the signed
            //     session cookie + return `{ user, expires_at }`.
            //   - GET[?mint=1] = decode the live signed cookie, or (expired /
            //     `mint=1`) re-sign a fresh cookie from the server-held anchor.
            // Registered BEFORE the subdomain catch-all. Same-origin-only (no
            // CORS); `?mint=1` + POST additionally require `X-ZS-Auth`.
            .service(
                web::resource("/__zeroship/auth/session")
                    .route(web::post().to(auth_token::session_post))
                    .route(web::get().to(auth_token::session)),
            )
            // The browser-facing auth HTTP
            // surface. Same mounting discipline (BEFORE the subdomain
            // catch-all). `/authorize` 302s to OP (the one cross-site
            // hop); `/popup-callback` serves the same-origin relay page;
            // `/signout` revokes + clears the session state.
            .service(
                web::resource("/__zeroship/auth/authorize")
                    .route(web::get().to(browser_auth::authorize)),
            )
            .service(
                web::resource("/__zeroship/auth/popup-callback")
                    .route(web::get().to(browser_auth::popup_callback)),
            )
            .service(
                web::resource("/__zeroship/auth/signout")
                    .route(web::post().to(browser_auth::signout)),
            )
            // OIDC Back-Channel Logout 1.0 RP endpoint. Registered
            // at the gateway-host level (not per-app) because the
            // URI is stable across every registered client's
            // `backchannel_logout_uri`. Must be
            // mounted BEFORE the subdomain catch-all below — ntex's
            // path routing is registration-order-sensitive for
            // overlapping patterns.
            .configure(backchannel_logout::configure)
            // Subdomain catch-all — must be last (lowest priority)
            .service(
                web::resource("/{tail}*")
                    .state(web::types::PayloadConfig::new(
                        zeroship_core::dispatch_frame::MAX_REQUEST_BODY_BYTES,
                    ))
                    .route(web::route().to(router::handle_subdomain)),
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
    use zeroship_core::config::{GeneratedConfig, SourceKind, SERVICE_CREDENTIAL_SENTINEL};

    // WHAT LEFT THIS MODULE. The two tests that drove the ENVIRONMENT tier of
    // `GateSettings` moved to `crates/zeroship-gateway/tests/config_env_tier.rs`. That
    // tier is clap's `env = "ZEROSHIP_..."` attribute, so the only way to
    // exercise it in-process was `std::env::set_var`, which mutates the
    // environment every other test in this binary parses in. They now run the
    // real `zeroship-gate` under `--check-config` with `Command::env`, which
    // scopes the environment to the child and observes the shipped resolver
    // rather than a reconstruction of it.
    //
    // What stayed here is everything that needs no environment at all.

    /// A temp file that removes itself even when an assertion panics.
    struct SecretFile(PathBuf);

    impl SecretFile {
        fn new(tag: &str, contents: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "zeroship_gate_secret_{tag}_{}_{:?}.txt",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::write(&path, contents).expect("write fixture secret");
            // The resolver refuses a secret file any second local account could
            // read, and `std::fs::write` leaves 0644 under the usual umask.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                    .expect("owner-only fixture secret");
            }
            Self(path)
        }

        fn arg(&self) -> &str {
            self.0.to_str().expect("utf8 fixture path")
        }
    }

    impl Drop for SecretFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn resolve(args: &[&str]) -> GateSettings {
        let mut argv = vec!["zeroship-gate"];
        argv.extend_from_slice(args);
        GateSettings::resolve_config(
            GateSettingsSources::try_parse_from(argv).expect("gateway sources parse"),
            None,
        )
        .expect("gateway settings resolve")
    }

    #[test]
    fn deleted_security_relaxation_flag_is_rejected() {
        let parsed = GateSettingsSources::try_parse_from(["zeroship-gate", "--dev-insecure"]);
        let err = match parsed {
            Ok(_) => panic!("deleted --dev-insecure flag must be rejected"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn gateway_control_key_is_required() {
        assert!(require_nonempty(CONTROL_KEY_LABEL, "").is_err());
        assert!(require_nonempty(CONTROL_KEY_LABEL, "key").is_ok());
    }

    // M6: `--auth-secret` is a deleted legacy knob — clap must reject it
    // as an unknown argument, not silently accept it.
    #[test]
    fn auth_secret_flag_is_rejected() {
        let parsed =
            GateSettingsSources::try_parse_from(["zeroship-gate", "--auth-secret", "x"]);
        let err = match parsed {
            Ok(_) => panic!("--auth-secret must be rejected as an unknown argument"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    // M7: the worker URL list is parsed ONCE; empty/whitespace entries are
    // dropped so the check-config count and runtime hash ring agree.
    #[test]
    fn worker_urls_drops_empty_entries() {
        let parsed = parse_worker_urls("http://a:8080,,http://b:8080");
        assert_eq!(parsed, vec!["http://a:8080", "http://b:8080"]);
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn worker_urls_trims_whitespace_entries() {
        let parsed = parse_worker_urls(" http://a:8080 ,  , http://b:8080 ");
        assert_eq!(parsed, vec!["http://a:8080", "http://b:8080"]);
    }

    #[test]
    fn gateway_stash_key_rejects_missing() {
        let err = validate_stash_key(STASH_SIGNING_KEY_LABEL, "").unwrap_err();
        assert!(err.contains("required"), "{err}");
    }

    #[test]
    fn gateway_control_key_rejects_missing() {
        let err = require_nonempty(CONTROL_KEY_LABEL, "").unwrap_err();
        // The FULL canonical name. The bare `CONTROL_KEY` this asserted before
        // is a SUBSTRING of the live one, so it passed while naming a variable
        // the gateway does not read.
        assert!(err.contains("ZEROSHIP_CONTROL_KEY"), "{err}");
    }

    #[test]
    fn gateway_control_key_accepts_nonempty() {
        assert!(require_nonempty(CONTROL_KEY_LABEL, "secret").is_ok());
    }

    #[test]
    fn gateway_broker_secret_rejects_short_material() {
        let err = oidc_rp::BrokerSecret::from_bytes(b"short".to_vec()).unwrap_err();
        assert!(err.contains("minimum is 32 bytes"), "{err}");
    }

    #[test]
    fn gateway_broker_secret_rejects_dev_sentinel() {
        let err = oidc_rp::BrokerSecret::from_bytes(
            zeroship_core::auth::DEV_BROKER_MASTER_SECRET.to_vec(),
        )
        .unwrap_err();
        assert!(err.contains("dev sentinel"), "{err}");
    }

    #[test]
    fn gateway_broker_secret_accepts_strong_material() {
        oidc_rp::BrokerSecret::from_bytes(
            b"gateway-broker-secret-test-master-32-bytes".to_vec(),
        )
        .expect("strong broker master");
    }

    #[test]
    fn gateway_stash_key_rejects_short() {
        let err = validate_stash_key(STASH_SIGNING_KEY_LABEL, "short").unwrap_err();
        assert!(err.contains("too short"), "{err}");
    }

    #[test]
    fn gateway_stash_key_accepts_strong() {
        let key = "0123456789abcdef0123456789abcdef";
        assert!(validate_stash_key(STASH_SIGNING_KEY_LABEL, key).is_ok());
    }

    // --- Secret<T> conversion: the guards still run on the resolved material ---

    /// The stash-key strength guard, driven end to end through the REAL
    /// declaration: a `--stash-signing-key-file` path is resolved by the
    /// generated resolver and `validate_gateway_secrets` hands the material it
    /// produced to `validate_stash_key`.
    ///
    /// Three cases plus the one-variable partner for each, because "the guard
    /// rejects everything" and "the guard rejects nothing" both satisfy a single
    /// case on its own.
    #[test]
    fn a_weak_or_absent_stash_key_still_fails_the_boot_guard() {
        let strong = "0123456789abcdef0123456789abcdef";
        let good = SecretFile::new("stash_ok", strong);
        let weak = SecretFile::new("stash_weak", "short");

        // Every other required secret supplied, so only the stash key can be
        // the reason a case fails. `--` values are inline files, never argv.
        let control = SecretFile::new("control", "control-key-material");
        let broker = SecretFile::new("broker", "gateway-broker-secret-test-master-32-bytes");
        let salt = SecretFile::new("salt", strong);
        let base = |stash: &str| -> Vec<String> {
            vec![
                "--control-key-file".to_owned(),
                control.arg().to_owned(),
                "--broker-secret-file".to_owned(),
                broker.arg().to_owned(),
                "--pairwise-salt-file".to_owned(),
                salt.arg().to_owned(),
                "--stash-signing-key-file".to_owned(),
                stash.to_owned(),
            ]
        };
        fn borrow(args: &[String]) -> Vec<&str> {
            args.iter().map(String::as_str).collect()
        }

        // 1. WEAK material, read from the file the flag named: rejected.
        let args = base(weak.arg());
        let settings = resolve(&borrow(&args));
        assert_eq!(settings.stash_signing_key.source(), Some(SourceKind::CliFile));
        let posture = audit_credentials(&gateway_credentials(&settings));
        let weak_rows = posture.weak();
        assert_eq!(weak_rows.len(), 1, "{posture:?}");
        assert_eq!(weak_rows[0].subsystem, "browser-oidc-stash");
        assert!(weak_rows[0].message.contains("too short"), "{weak_rows:?}");
        assert!(!weak_rows[0].unset, "a short key is present, not unset");

        // 1b. The one-variable partner: same flags, a strong key at the path.
        let args = base(good.arg());
        let ok = audit_credentials(&gateway_credentials(&resolve(&borrow(&args))));
        assert!(ok.is_ok(), "a strong stash key passes every guard: {ok:?}");
        assert_eq!(ok.checked(), 3, "all three gateway credentials were judged");

        // 2. ABSENT: nothing supplies the stash key at all. The bridge runs the
        // validator on "", which is how it produces its own "is required".
        let mut args = base(good.arg());
        args.truncate(6);
        let settings = resolve(&borrow(&args));
        assert!(!settings.stash_signing_key.is_configured());
        let posture = audit_credentials(&gateway_credentials(&settings));
        assert_eq!(posture.weak().len(), 1, "{posture:?}");
        assert_eq!(posture.weak()[0].subsystem, "browser-oidc-stash");
        assert!(posture.weak()[0].unset, "an absent key is the unset state");
        assert!(posture.weak()[0].message.contains("required"), "{posture:?}");

        // 3. THE SENTINEL. Same flags, same everything, and the placeholder
        // instead of key material: refused with the SAME message the absent case
        // produced, because they are one branch.
        let sentinel = SecretFile::new("stash_sentinel", SERVICE_CREDENTIAL_SENTINEL);
        let args = base(sentinel.arg());
        let settings = resolve(&borrow(&args));
        let sentinel_posture = audit_credentials(&gateway_credentials(&settings));
        assert_eq!(sentinel_posture.weak().len(), 1, "{sentinel_posture:?}");
        assert_eq!(sentinel_posture.weak()[0].message, posture.weak()[0].message);
        assert!(sentinel_posture.weak()[0].unset);

        // Does NOT cover the environment or TOML tiers of this secret: the env
        // tier is process-global and would race sibling tests, and the overlay
        // tier is `resolve_secret_sources`'s, asserted in core. It also does not
        // prove `main` calls `gateway_credentials` - only that the function main
        // calls behaves this way. That link is covered by
        // `crates/zeroship-gateway/tests/credential_boot.rs`, which drives the
        // real binary.
    }

    /// THE PER-SUBSYSTEM PROPERTY, on the gateway's own row set: every row names
    /// a subsystem, no two rows name the same one, and the set is not empty.
    ///
    /// A gate whose row set collapsed to nothing would refuse nothing and print
    /// exactly what a correctly configured gateway prints.
    #[test]
    fn every_gateway_credential_names_a_distinct_subsystem() {
        let strong = "0123456789abcdef0123456789abcdef";
        let control = SecretFile::new("sub_control", "control-key-material");
        let stash = SecretFile::new("sub_stash", strong);
        let salt = SecretFile::new("sub_salt", strong);
        let args = [
            "--control-key-file",
            control.arg(),
            "--stash-signing-key-file",
            stash.arg(),
            "--pairwise-salt-file",
            salt.arg(),
        ];
        let settings = resolve(&args);
        let rows = gateway_credentials(&settings);
        assert_eq!(rows.len(), 3, "the gateway declares three credentials");
        let mut subsystems: Vec<&str> = rows.iter().map(|row| row.subsystem).collect();
        subsystems.sort_unstable();
        subsystems.dedup();
        assert_eq!(subsystems.len(), 3, "two rows share a subsystem name");
        assert!(rows.iter().all(|row| row.label.starts_with("ZEROSHIP_")));
    }

    /// The boot gate's three-way verdict, on the gateway's real rows.
    #[test]
    fn the_gateway_verdict_refuses_in_production_and_on_every_dry_run() {
        let strong = "0123456789abcdef0123456789abcdef";
        let control = SecretFile::new("v_control", SERVICE_CREDENTIAL_SENTINEL);
        let stash = SecretFile::new("v_stash", strong);
        let salt = SecretFile::new("v_salt", strong);
        let args = vec![
            "--control-key-file".to_owned(),
            control.arg().to_owned(),
            "--stash-signing-key-file".to_owned(),
            stash.arg().to_owned(),
            "--pairwise-salt-file".to_owned(),
            salt.arg().to_owned(),
        ];
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let settings = resolve(&borrowed);
        let posture = audit_credentials(&gateway_credentials(&settings));
        assert_eq!(posture.summary(), "weak");
        assert_eq!(
            posture.verdict(BuildProfile::Production, false),
            CredentialVerdict::Refuse
        );
        assert_eq!(
            posture.verdict(BuildProfile::Development, true),
            CredentialVerdict::Refuse
        );
        assert_eq!(
            posture.verdict(BuildProfile::Development, false),
            CredentialVerdict::DevEscape
        );

        // The one-variable control: the same four flags with real material in
        // the control-key file proceed on every profile and both modes.
        let good = SecretFile::new("v_control_ok", "control-key-material");
        let mut fixed = args.clone();
        fixed[1] = good.arg().to_owned();
        let borrowed: Vec<&str> = fixed.iter().map(String::as_str).collect();
        let ok = audit_credentials(&gateway_credentials(&resolve(&borrowed)));
        assert_eq!(ok.summary(), "configured");
        for profile in [BuildProfile::Development, BuildProfile::Production] {
            for dry_run in [false, true] {
                assert_eq!(ok.verdict(profile, dry_run), CredentialVerdict::Proceed);
            }
        }
    }

    /// The `--check-config` half: a dry run must not open the file, and an
    /// unread secret must not be judged. Paired with the boot run over the SAME
    /// missing path, which does fail - so "no error" above is a property of the
    /// mode, not of the guard having been removed.
    #[test]
    fn a_check_config_run_neither_reads_nor_judges_a_secret_file() {
        let missing = std::env::temp_dir().join("zeroship_gate_absent_stash_key");
        let _ = std::fs::remove_file(&missing);
        assert!(!missing.exists(), "the fixture path must really be absent");
        let missing = missing.to_str().expect("utf8 path").to_owned();

        let sources = GateSettingsSources::try_parse_from([
            "zeroship-gate",
            "--check-config",
            "--stash-signing-key-file",
            &missing,
        ])
        .expect("gateway sources parse");
        let settings =
            GateSettings::resolve_config(sources, None).expect("a dry run opens no secret file");
        assert!(settings.stash_signing_key.is_configured());
        assert_eq!(
            settings.stash_signing_key.expose_secret(),
            None,
            "--check-config must not have read the file"
        );
        let posture = audit_credentials(&gateway_credentials(&settings));
        assert!(
            posture
                .weak()
                .iter()
                .all(|weak| weak.subsystem != "browser-oidc-stash"),
            "an unread secret must not be judged: {posture:?}"
        );
        assert_eq!(posture.unread(), 1, "and it must be COUNTED as unread");

        // The one-variable partner: only `--check-config` differs, and the boot
        // run does try to open the same path.
        let boot = GateSettingsSources::try_parse_from([
            "zeroship-gate",
            "--stash-signing-key-file",
            &missing,
        ])
        .expect("gateway sources parse");
        assert!(GateSettings::resolve_config(boot, None).is_err());

        // Does NOT cover whether `main` routes `--check-config` to the report
        // rather than to the server; the config_env_tier integration target does.
    }

    /// A secret is reported by PRESENCE. The resolved declaration derives
    /// `Debug`, so this is the formatter every `{:?}` of the settings reaches -
    /// including any future tracing call that logs them wholesale.
    #[test]
    fn a_resolved_secret_never_formats_its_material_or_its_length() {
        // Deliberately unlike any other text in the struct, so a prefix match
        // below can only be the secret leaking and never an unrelated field.
        const SENTINEL: &str = "j4v9c2t6b8m1q5z7x3n0k4h8r2w6y1p5";
        let file = SecretFile::new("debug", SENTINEL);
        let settings = resolve(&["--stash-signing-key-file", file.arg()]);

        assert!(settings.stash_signing_key.is_configured());
        assert_eq!(
            settings.stash_signing_key.expose_str(),
            SENTINEL,
            "the boot path must still get the real material"
        );

        // The secret's OWN formatter: no value, no prefix of it, no length.
        let field = format!("{:?}", settings.stash_signing_key);
        for length in 4..=SENTINEL.len() {
            assert!(
                !field.contains(&SENTINEL[..length]),
                "Debug leaked a {length}-char prefix of the secret: {field}"
            );
        }
        assert!(
            !field.contains(&SENTINEL.len().to_string()),
            "Debug leaked the secret's length: {field}"
        );
        // The one-variable control: the tier IS published, so this is not
        // passing because Debug prints nothing at all.
        assert_eq!(field, "Secret(configured from CliFile)");

        // And the whole resolved declaration, which is what a `{:?}` on the
        // settings reaches.
        let rendered = format!("{settings:?}");
        assert!(!rendered.contains(SENTINEL), "{rendered}");
        assert!(rendered.contains("Secret(configured from CliFile)"), "{rendered}");

        // Does NOT cover a caller that calls `expose_str` and prints the result
        // itself. No type can stop deliberate disclosure; what it removes is the
        // accidental `{:?}`.
    }

    /// A secret's clap carrier is a PATH flag, and there is no value flag for
    /// any of them - a secret must never travel through argv, where it is
    /// visible in `ps` to every user on the host.
    #[test]
    fn no_gateway_secret_has_a_value_flag() {
        use clap::CommandFactory;

        let command = GateSettingsSources::command();
        let longs = command
            .get_arguments()
            .filter_map(|arg| arg.get_long().map(str::to_owned))
            .collect::<Vec<_>>();
        for secret in [
            "control-key",
            "database-url",
            "stash-signing-key",
            "pairwise-salt",
        ] {
            assert!(
                longs.iter().any(|long| long == &format!("{secret}-file")),
                "{secret} must offer a -file path flag: {longs:?}"
            );
            assert!(
                !longs.iter().any(|long| long == secret),
                "{secret} must NOT offer a value flag: {longs:?}"
            );
        }
        // The deleted pre-conversion spellings, by name: `--db` carried the DSN
        // in argv, and the broker secret's path flag was `gateway`-prefixed.
        for deleted in ["db", "gateway-broker-secret-file"] {
            assert!(
                !longs.iter().any(|long| long == deleted),
                "--{deleted} must be gone, not aliased: {longs:?}"
            );
        }
        // The one-variable control: the key-FILE settings stay operational path
        // flags, because their loader owns the permission check and the raw
        // byte read. They are spelled the same way a secret's path flag is, so
        // the assertion that distinguishes them is the ENV binding below, not
        // the flag name.
        for path_setting in [
            "signing-key-file",
            "prev-signing-key-file",
            "broker-secret-file",
        ] {
            assert!(
                longs.iter().any(|long| long == path_setting),
                "--{path_setting} must exist: {longs:?}"
            );
        }
        // ...and an operational path DOES carry a canonical env binding, which
        // is exactly what a secret's carrier never has. This is what would have
        // caught `gateway.broker_secret` being declared `Secret<String>` while
        // auth's twin stayed a path: the pair then had two shapes, two supply
        // sets, and a silent newline/UTF-8 disagreement about the same bytes.
        let broker_env = command
            .get_arguments()
            .find(|arg| arg.get_long() == Some("broker-secret-file"))
            .and_then(clap::Arg::get_env)
            .map(std::ffi::OsStr::to_string_lossy)
            .map(std::borrow::Cow::into_owned);
        assert_eq!(
            broker_env.as_deref(),
            Some("ZEROSHIP_GATEWAY_BROKER_SECRET_FILE"),
            "the broker master secret is a PATH setting and must bind its canonical env name"
        );

        // Does NOT cover clap's env bindings; those are asserted for the
        // operational settings in the worker's config tests and, for secrets,
        // are absent from the carrier by construction (the macro emits no
        // `env = ...` for a Secret field).
    }
}
