//! Install-time seed that makes the **console** (`apps/zeroship-builder`)
//! deployable as a regular zeroship app on the standard runtime.
//!
//! Mirrors [`crate::bootstrap_builder`] (run in-process at control boot, behind
//! a flag, NEVER an HTTP route), but instead of registering a confidential
//! console OIDC client it seeds the console as a **platform-owned regular app**:
//!
//! 1. Upsert the `control.apps` row for the console host on the **enterprise**
//!    plan (no CPU/wall cap — the console does heavy SSE/AI work; see
//!    `registry.rs::runtime_limits_for_plan`). The app id is **derived
//!    deterministically** from the console host so re-running the seed (or
//!    seeding dev vs. prod) is stable and idempotent.
//! 2. Upsert the per-app **public PKCE** OAuth client (`oac_<base62>`) via the
//!    same [`crate::app_oauth_client::ensure_app_client`] every creator app
//!    uses, with an **explicit** `sector_identifier` = the console host (the
//!    console host is reserved / 2-label, so we do NOT derive `{name}.{base}` —
//!    we pass the host verbatim as the apex host).
//! 3. Ingest the prebuilt console `.zship` through the **same** BlobStore +
//!    manifest ingest the deploy handler uses ([`zeroship_bundle::ingest`]) and
//!    commit `deploy_hash` + `manifest_json`
//!    ([`Registry::set_deploy_with_manifest`]) so the gateway route-sync serves
//!    it.
//! 4. Forward the console's server-side runtime env (`OPENAI_API_KEY`,
//!    `SANDBOX_TOKEN`, `SANDBOX_URL`, `ZEROSHIP_SDK_REGISTRY`) from the control
//!    process env into the per-app env store so the deployed console can craft
//!    and preview apps in the sandbox.
//!
//! Everything is **idempotent** — every write is an upsert and every derived
//! identifier is deterministic, so re-running the seed at every boot does not
//! error or duplicate.
//!
//! ## Pure creator app — no control credential
//!
//! The console is a **pure creator app**: it crafts and previews apps in a
//! sandbox, holds **no** control-plane credential, and makes **zero** control
//! calls (platform-deploy is deferred to a future device-code flow). The seed
//! therefore mints **no** PAT and injects **no** `ZEROSHIP_CONTROL_SERVICE_TOKEN`
//! — creator identity comes only from the platform BFF session (`currentUser()`),
//! exactly like any other creator app. See
//! `docs/superpowers/specs/2026-05-31-console-pure-creator-app-design.md`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use compio_postgres::Client;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroship_bundle::BlobStore;

use crate::app_oauth_client::{self, client_id_for_app};
use crate::env_store::EnvStore;
use crate::registry::Registry;

/// Default console host in dev (compose / `*.zeroship.localhost`).
pub const DEV_CONSOLE_HOST: &str = "console.zeroship.localhost";
/// Default console host in prod.
pub const PROD_CONSOLE_HOST: &str = "console.zeroship.ai";

/// Default prebuilt console `.zship` path (relative to the repo root / CWD the
/// control binary runs in). Matches the artifact `apps/zeroship-builder` emits
/// (`pnpm build` → `dist/app.zship`, R5a).
pub const DEFAULT_CONSOLE_ZSHIP: &str = "apps/zeroship-builder/dist/app.zship";

/// The console app **name** is the SUBDOMAIN LABEL of the console host (the
/// first DNS label) — NOT a hex-suffixed stem. This is the load-bearing routing
/// fix: the gateway resolves `console.*` by `lookup_by_name(label)` where
/// `label` is the first label of the request `Host` (see
/// `crates/gateway/src/router/dispatch.rs` `subdomain_of` → `lookup_by_name`),
/// and the name index is keyed on `control.apps.name`. So the seeded row's
/// `name` MUST equal that label or the gateway 503s the console.
///
/// For `console.zeroship.localhost` (dev) and `console.zeroship.ai` (prod) the
/// label is `console` in both — and compose uses ONE database with a SINGLE
/// console per deployment, so the `control.apps.name` UNIQUE constraint that
/// drove the old hex suffix does not apply. The full host stays explicit as the
/// OAuth `sector_identifier` (see [`bootstrap_console`]); the name is never used
/// to DERIVE the apex host.
#[must_use]
pub fn console_app_name(host: &str) -> String {
    host.split('.').next().unwrap_or(host).to_owned()
}

/// The console app's enterprise plan id — unlimited CPU/wall via
/// `registry.rs::runtime_limits_for_plan` ("unlimited" | "enterprise").
pub const CONSOLE_PLAN_ID: &str = "enterprise";

/// How a console runtime env var lands in the per-app env store.
///
/// Both classes reach the worker's **server-side** `process.env` (V8 worker
/// only — the browser/client bundle never receives `process.env`; see
/// `crates/runtime/src/core/init.rs` `setup_globals`). They differ only in
/// at-rest handling:
///   - [`EnvClass::Secret`] → AES-256-GCM encrypted in `control.app_secrets` and
///     added to the per-app `expose` list (the opt-in that surfaces an exposed
///     secret in `process.env`). The browser never sees it.
///   - [`EnvClass::Var`]    → plaintext in `control.app_vars`. Vars are always in
///     `process.env`, so no `expose` entry is needed. Used for non-sensitive
///     config (URLs), never for credentials.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnvClass {
    /// Encrypted at rest + expose entry (credentials).
    Secret,
    /// Plaintext (non-sensitive config — URLs, registry endpoints).
    Var,
}

/// One console runtime env var, forwarded from the **control process env**.
struct RuntimeEnvSpec {
    /// The env-store key on the console app (what the console reads via
    /// `process.env.<key>` server-side). MUST satisfy `EnvStore`'s key regex
    /// (`/^[A-Z][A-Z0-9_]{0,63}$/`).
    app_key: &'static str,
    /// The env var to read at seed time from the **control** process env
    /// (`std::env::var`). Usually identical to `app_key`; kept distinct so the
    /// source name can diverge from the app-facing name if it ever needs to.
    source_env: &'static str,
    /// Secret (encrypt + expose) vs. plaintext var.
    class: EnvClass,
}

/// The FULL server-side runtime env the **deployed console** needs to FUNCTION.
/// Each is forwarded from the control process env at seed time; an unset source
/// var is SKIPPED with a warning (the console degrades for that one feature
/// rather than failing the whole seed). Sourced from the console's server reads
/// (`apps/zeroship-builder/src/server/internal/env.ts` + direct `process.env`):
///   - `OPENAI_API_KEY`        — AI codegen/chat/pm/sre/wizard (credential).
///   - `SANDBOX_TOKEN`         — sandbox controller bearer token (credential).
///   - `SANDBOX_URL`           — sandbox/preview backend URL (non-secret config).
///   - `ZEROSHIP_SDK_REGISTRY` — private npm registry URL for generated apps
///                               (non-secret config; optional).
/// The console is a **pure creator app** (no control credential, zero control
/// calls), so there is NO `ZEROSHIP_CONTROL_URL` to forward. The AI model is
/// hard-coded in the console (`gpt-5.4-mini`) with no base-URL override, so there
/// is no model/base-URL env var to forward either.
const CONSOLE_RUNTIME_ENV: &[RuntimeEnvSpec] = &[
    RuntimeEnvSpec {
        app_key: "OPENAI_API_KEY",
        source_env: "OPENAI_API_KEY",
        class: EnvClass::Secret,
    },
    RuntimeEnvSpec {
        app_key: "SANDBOX_TOKEN",
        source_env: "SANDBOX_TOKEN",
        class: EnvClass::Secret,
    },
    RuntimeEnvSpec {
        app_key: "SANDBOX_URL",
        source_env: "SANDBOX_URL",
        class: EnvClass::Var,
    },
    RuntimeEnvSpec {
        app_key: "ZEROSHIP_SDK_REGISTRY",
        source_env: "ZEROSHIP_SDK_REGISTRY",
        class: EnvClass::Var,
    },
];

// ---------------------------------------------------------------------------
// Config / result / error
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ConsoleBootstrapConfig {
    /// Off by default; flipped on by `--bootstrap-console`.
    pub enabled: bool,
    /// The console host (explicit sector). Dev: `console.zeroship.localhost`;
    /// prod: `console.zeroship.ai`.
    pub console_host: String,
    /// Path to the prebuilt console `.zship`.
    pub console_zship: PathBuf,
    /// URL scheme the console serves under (`http` in dev-insecure, else
    /// `https`). Drives the OAuth redirect_uris + sector origin.
    pub scheme: String,
    /// Hydra admin API base URL (for the per-app public PKCE client).
    pub hydra_admin_url: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsoleBootstrapStatus {
    /// `enabled = false` — nothing was done.
    Disabled,
    /// The seed ran to completion (idempotent — safe whether first run or Nth).
    Seeded,
}

#[derive(Clone, Debug)]
pub struct ConsoleBootstrapResult {
    pub status: ConsoleBootstrapStatus,
    /// The derived console app id.
    pub app_id: Uuid,
    /// The per-app OAuth client id (`oac_<base62>`).
    pub client_id: String,
}

#[derive(Debug)]
pub enum ConsoleBootstrapError {
    Db(String),
    Io(String),
    Ingest(String),
    OauthClient(String),
    Env(String),
}

impl std::fmt::Display for ConsoleBootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "database: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Ingest(e) => write!(f, "zship ingest: {e}"),
            Self::OauthClient(e) => write!(f, "oauth client: {e}"),
            Self::Env(e) => write!(f, "env store: {e}"),
        }
    }
}

impl std::error::Error for ConsoleBootstrapError {}

// ---------------------------------------------------------------------------
// Deterministic identifiers (host-derived, stable across boots + dev/prod)
// ---------------------------------------------------------------------------

/// Derive a stable UUID from a fixed domain-separation label + the console host.
/// `uuid` is built without the `v5` feature in this workspace, so we hash and
/// build the bytes ourselves: SHA-256(label || host)[..16] with the RFC-4122
/// variant + version-8 nibbles stamped in. Deterministic and collision-resistant
/// across the three identity roles (each carries a distinct `label`).
fn derive_uuid(label: &str, host: &str) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(label.as_bytes());
    hasher.update([0u8]); // explicit separator so label/host can't run together
    hasher.update(host.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // Stamp version 8 (custom) + RFC-4122 variant so the value is a well-formed
    // UUID (Postgres `uuid` accepts any 128 bits, but a valid shape keeps logs /
    // tooling honest).
    bytes[6] = (bytes[6] & 0x0F) | 0x80;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    Uuid::from_bytes(bytes)
}

/// Deterministic console app id (the `control.apps` PK + OAuth client source).
#[must_use]
pub fn console_app_id(host: &str) -> Uuid {
    derive_uuid("zeroship:console:app:v1", host)
}

// ---------------------------------------------------------------------------
// Seed entry point
// ---------------------------------------------------------------------------

/// Idempotently seed the console as a platform-owned regular app.
///
/// `control_pg` talks to the `control` schema (apps, env, deploy commit,
/// oauth_clients). The console is a **pure creator app** — it holds no
/// control-plane credential — so the seed mints no PAT and touches no `auth`
/// schema.
///
/// # Errors
/// [`ConsoleBootstrapError`] on any DB / IO / ingest / Hydra / env failure.
pub async fn bootstrap_console(
    cfg: &ConsoleBootstrapConfig,
    registry: &Registry,
    env_store: &EnvStore,
    blob_store: &Arc<dyn BlobStore>,
    control_pg: &mut Client,
) -> Result<ConsoleBootstrapResult, ConsoleBootstrapError> {
    if !cfg.enabled {
        return Ok(ConsoleBootstrapResult {
            status: ConsoleBootstrapStatus::Disabled,
            app_id: console_app_id(&cfg.console_host),
            client_id: client_id_for_app(&console_app_id(&cfg.console_host)),
        });
    }

    let app_id = console_app_id(&cfg.console_host);
    let client_id = client_id_for_app(&app_id);
    let app_name = console_app_name(&cfg.console_host);

    // 1. Upsert the console.apps row on the enterprise plan. A fixed api_key /
    //    api_key_hash is fine: the console is never authenticated by its app
    //    api_key (it deploys via the seed + serves via the gateway route), but
    //    the columns are NOT NULL. We use a deterministic, clearly-marked
    //    sentinel so a re-run doesn't churn the row.
    upsert_console_app_row(control_pg, &app_id, &app_name).await?;

    // 2. Upsert the per-app public PKCE OAuth client with an EXPLICIT sector.
    //    Reusing ensure_app_client (the SAME path every creator app uses) gives
    //    us the public-PKCE client body + both DB rows (oauth_clients +
    //    app_oauth_clients) + the baseline audience. The console host is passed
    //    VERBATIM as the apex host, so the sector_identifier becomes
    //    `{scheme}://{console_host}` — explicit, NOT derived `{name}.{base}`.
    //
    //    `first_party = true`: the console IS the platform's own first-party
    //    app, so its oac_ client gets `skip_consent = true` — an owner-approved,
    //    narrow exception to spec §5.2 (a consent prompt for the platform's own
    //    console is meaningless). This applies ONLY to the console; every
    //    creator-app call passes `first_party = false`. The deployment MUST ALSO
    //    name this client id in `[auth].trusted_oauth_clients` so control marks
    //    it `skip_consent=true` and the immersive framed `/login → /consent`
    //    dance auto-accepts identity consent for the console (the `oac_<base62>`
    //    id is printed by `--bootstrap-console`; see
    //    `zeroship_core::auth::trusted_clients`).
    let hydra = zeroship_auth::hydra_client::HydraAdmin::new(cfg.hydra_admin_url.clone());
    let hosts = vec![cfg.console_host.clone()];
    app_oauth_client::ensure_app_client(
        control_pg,
        &hydra,
        &app_id,
        &app_name,
        &cfg.scheme,
        &hosts,
        &[],  // console declares no custom scopes via this seed
        true, // first-party console → skip_consent = true (narrow §5.2 exception)
    )
    .await
    .map_err(|e| ConsoleBootstrapError::OauthClient(e.to_string()))?;

    // 3. Ingest the prebuilt .zship through the SAME ingest the deploy handler
    //    uses, then commit deploy_hash + manifest_json atomically.
    ingest_and_commit_zship(registry, blob_store, &app_id, &cfg.console_zship).await?;

    // 4. Forward the console's server-side runtime env (OPENAI_API_KEY,
    //    SANDBOX_URL/SANDBOX_TOKEN, SDK registry) from the control process env
    //    into the per-app env store so the deployed console can craft + preview
    //    apps in the sandbox. Unset source vars are skipped with a warning so the
    //    seed never fails or writes an empty value. The console holds NO
    //    control-plane credential — there is no PAT mint and no service token.
    ensure_runtime_env(env_store, &app_id).await?;

    tracing::info!(
        app_id = %app_id,
        client_id = %client_id,
        host = %cfg.console_host,
        plan = CONSOLE_PLAN_ID,
        "control: console seeded as a regular app (pure creator, no control credential)"
    );

    Ok(ConsoleBootstrapResult {
        status: ConsoleBootstrapStatus::Seeded,
        app_id,
        client_id,
    })
}

// ---------------------------------------------------------------------------
// Step 1 — control.apps row
// ---------------------------------------------------------------------------

/// Fixed (deterministic) api_key sentinel for the console row. The console is
/// never authenticated by this key; it exists only to satisfy the NOT NULL
/// columns. Marked so it is obviously not a real creator key.
fn console_api_key(app_id: &Uuid) -> String {
    format!("console-seed-{}", app_id.simple())
}

async fn upsert_console_app_row(
    pg: &Client,
    app_id: &Uuid,
    app_name: &str,
) -> Result<(), ConsoleBootstrapError> {
    let api_key = console_api_key(app_id);
    let api_key_hash = zeroship_core::auth::hash_api_key(&api_key);
    // Insert with the fixed id + enterprise plan. ON CONFLICT keeps the row's
    // name/plan/key stable (so a re-run is a true no-op on these columns) while
    // leaving deploy_hash / manifest_json / env_version to the deploy-commit +
    // env steps. NOT touching updated_at here keeps the row quiet on no-op runs.
    // `name` is the console host's subdomain label (`console`) — what the
    // gateway's `lookup_by_name` resolves `console.*` to. One console per
    // deployment on a shared DB, so the `apps.name` UNIQUE constraint never
    // collides.
    pg.execute(
        "INSERT INTO zeroship.apps (id, name, plan_id, api_key, api_key_hash) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (id) DO UPDATE SET \
            name = EXCLUDED.name, \
            plan_id = EXCLUDED.plan_id",
        &[app_id, &app_name, &CONSOLE_PLAN_ID, &api_key, &api_key_hash],
    )
    .await
    .map_err(|e| ConsoleBootstrapError::Db(e.to_string()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 3 — .zship ingest + deploy commit
// ---------------------------------------------------------------------------

async fn ingest_and_commit_zship(
    registry: &Registry,
    blob_store: &Arc<dyn BlobStore>,
    app_id: &Uuid,
    zship_path: &Path,
) -> Result<(), ConsoleBootstrapError> {
    let bytes = std::fs::read(zship_path).map_err(|e| {
        ConsoleBootstrapError::Io(format!("read console .zship {}: {e}", zship_path.display()))
    })?;
    // SAME ingest the deploy() handler uses: streams the tar.zst into the
    // content-addressed BlobStore + writes the manifest, returning the
    // deploy_hash + manifest_json.
    let success = zeroship_bundle::ingest(blob_store, app_id, &bytes)
        .await
        .map_err(|e| ConsoleBootstrapError::Ingest(format!("{e:?}")))?;
    // Atomic deploy commit: deploy_hash + manifest_json land together so the
    // gateway never observes a half-applied deploy.
    let updated = registry
        .set_deploy_with_manifest(app_id, &success.deploy_hash, &success.manifest_json)
        .await
        .map_err(|e| ConsoleBootstrapError::Db(e.to_string()))?;
    if !updated {
        return Err(ConsoleBootstrapError::Db(
            "console.apps row vanished between insert and deploy commit".to_owned(),
        ));
    }
    tracing::info!(
        app_id = %app_id,
        deploy_hash = %success.deploy_hash,
        blobs_uploaded = success.blobs_uploaded,
        blobs_deduped = success.blobs_deduped,
        "control: console .zship ingested + committed"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 4 — full server-side runtime env (forwarded from the control process)
// ---------------------------------------------------------------------------

/// Forward each [`CONSOLE_RUNTIME_ENV`] entry from the **control process env**
/// (`std::env::var`) into the console app's server-only env store:
///   - [`EnvClass::Secret`] → `env_store.set_secret` + add to the `expose` list.
///   - [`EnvClass::Var`]    → `env_store.set_var` (plaintext; always in
///     `process.env`, so no expose entry).
///
/// Behaviour contract:
///   - **Unset source var ⇒ SKIP** with a logged warning. The seed does NOT
///     fail and does NOT write an empty secret/var — the console degrades for
///     that one feature instead of breaking the whole boot.
///   - **Idempotent.** Both `set_secret` and `set_var` are upserts, and the
///     `expose` list is read-modify-write (only appends a missing key). A re-run
///     with unchanged process env reproduces the same rows. (Re-running with a
///     changed value re-encrypts/overwrites in place, which is the intended
///     refresh path — same as the rest of the env store.)
///
/// Never logs the value of a secret; only its key name and presence.
async fn ensure_runtime_env(
    env_store: &EnvStore,
    app_id: &Uuid,
) -> Result<(), ConsoleBootstrapError> {
    // Read the current expose list once; append any newly-set secret keys and
    // write it back a single time so we don't churn the row per secret.
    let mut expose = env_store
        .list_expose(*app_id)
        .await
        .map_err(|e| ConsoleBootstrapError::Env(e.to_string()))?;
    let mut expose_dirty = false;

    for spec in CONSOLE_RUNTIME_ENV {
        // Source the value from the CONTROL process env at seed time.
        let value = match std::env::var(spec.source_env) {
            Ok(v) if !v.is_empty() => v,
            Ok(_) | Err(_) => {
                // Unset (or empty) ⇒ skip with a warning; never write an empty
                // value. The console degrades for this one feature.
                tracing::warn!(
                    app_key = spec.app_key,
                    source_env = spec.source_env,
                    "control: console runtime env var unset in the control process — \
                     skipping (the deployed console degrades for this feature)"
                );
                continue;
            }
        };

        match spec.class {
            EnvClass::Secret => {
                env_store
                    .set_secret(*app_id, spec.app_key, &value)
                    .await
                    .map_err(|e| ConsoleBootstrapError::Env(e.to_string()))?;
                if !expose.iter().any(|k| k == spec.app_key) {
                    expose.push(spec.app_key.to_owned());
                    expose_dirty = true;
                }
                tracing::info!(
                    app_key = spec.app_key,
                    "control: set console runtime env (secret + expose, server-only)"
                );
            }
            EnvClass::Var => {
                env_store
                    .set_var(*app_id, spec.app_key, &value)
                    .await
                    .map_err(|e| ConsoleBootstrapError::Env(e.to_string()))?;
                tracing::info!(
                    app_key = spec.app_key,
                    "control: set console runtime env (plaintext var, server-side process.env)"
                );
            }
        }
    }

    if expose_dirty {
        env_store
            .set_expose(*app_id, &expose)
            .await
            .map_err(|e| ConsoleBootstrapError::Env(e.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_app_id_is_deterministic() {
        let host = "console.zeroship.localhost";
        // Stable across calls.
        assert_eq!(console_app_id(host), console_app_id(host));
    }

    #[test]
    fn dev_and_prod_hosts_derive_distinct_app_ids() {
        assert_ne!(
            console_app_id(DEV_CONSOLE_HOST),
            console_app_id(PROD_CONSOLE_HOST)
        );
    }

    #[test]
    fn derived_uuid_has_valid_variant_and_version() {
        let u = console_app_id(PROD_CONSOLE_HOST);
        let b = u.as_bytes();
        assert_eq!(b[6] & 0xF0, 0x80, "version nibble = 8");
        assert_eq!(b[8] & 0xC0, 0x80, "RFC-4122 variant");
    }

    #[test]
    fn client_id_is_oac_prefixed_for_console() {
        let cid = client_id_for_app(&console_app_id(PROD_CONSOLE_HOST));
        assert!(cid.starts_with("oac_"), "got {cid}");
        assert_eq!(cid.len(), 26);
    }

    /// Pure-creator-app invariant (no live PG needed): the console seed forwards
    /// NO control-plane credential and NO control URL. The runtime-env spec must
    /// therefore carry neither the service-token key nor the control URL — the
    /// console makes zero control calls. (Pre-change, `ZEROSHIP_CONTROL_SERVICE_TOKEN`
    /// was minted+exposed separately and `ZEROSHIP_CONTROL_URL` was in this list.)
    #[test]
    fn runtime_env_carries_no_control_credential_or_url() {
        let keys: Vec<&str> = CONSOLE_RUNTIME_ENV.iter().map(|s| s.app_key).collect();
        assert!(
            !keys.contains(&"ZEROSHIP_CONTROL_SERVICE_TOKEN"),
            "console is a pure creator app — no service token: {keys:?}"
        );
        assert!(
            !keys.contains(&"ZEROSHIP_CONTROL_URL"),
            "console makes zero control calls — no control URL: {keys:?}"
        );
        let sources: Vec<&str> = CONSOLE_RUNTIME_ENV.iter().map(|s| s.source_env).collect();
        assert!(!sources.contains(&"ZEROSHIP_CONTROL_SERVICE_TOKEN"));
        assert!(!sources.contains(&"ZEROSHIP_CONTROL_URL"));
        // The features the console DOES need to craft + preview in the sandbox.
        for needed in ["OPENAI_API_KEY", "SANDBOX_TOKEN", "SANDBOX_URL"] {
            assert!(keys.contains(&needed), "{needed} missing: {keys:?}");
        }
    }

    #[test]
    fn console_name_is_the_subdomain_label_so_the_gateway_routes_it() {
        // The load-bearing routing invariant: the seeded `apps.name` MUST be the
        // console host's first DNS label, because the gateway resolves
        // `console.*` via `lookup_by_name(<first-label>)` keyed on
        // `control.apps.name`. Dev + prod both label `console`.
        assert_eq!(console_app_name(DEV_CONSOLE_HOST), "console");
        assert_eq!(console_app_name(PROD_CONSOLE_HOST), "console");
        // A custom console host still yields its first label.
        assert_eq!(console_app_name("dash.example.com"), "dash");
        // A bare host with no dot is its own label (defensive).
        assert_eq!(console_app_name("console"), "console");
        // Registry name rule: ≤64, alphanumeric/hyphen/underscore.
        for name in [console_app_name(DEV_CONSOLE_HOST), console_app_name(PROD_CONSOLE_HOST)] {
            assert!(name.len() <= 64, "{name}");
            assert!(name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        }
    }
}
