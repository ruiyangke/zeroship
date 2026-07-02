//! Per-app brokered OAuth client lifecycle (auth-sdk Slice 1d, spec §1.1).
//!
//! Each hosted creator app gets its own stable `oac_<base62-app-id>` OAuth
//! client. The platform OP treats these clients as gateway-brokered: app code
//! and browsers never hold a client secret, while the gateway derives and
//! presents the per-app broker secret from the shared platform broker master.
//!
//! Storage (spec §1.1 round-5): the client identity lives in the EXISTING
//! `control.oauth_clients` (the `control.oauth_grants` FK target + the
//! `skip_consent` flag the consent fast path reads), written exactly like
//! `bootstrap_builder.rs::insert_oauth_client` does for the builder client.
//! `control.app_oauth_clients` is a thin per-app extension carrying only the
//! `app_id ↔ client_id` link and the `sector_identifier` (apex origin) for
//! pairwise/relay scoping. `ensure_app_client` writes BOTH rows in one
//! transaction.
//!
//! redirect_uris (§1.1): `{scheme}://{host}/__zeroship/auth/popup-callback` AND
//! `{scheme}://{host}/__zeroship/auth/callback` for every host the app serves (apex +
//! custom domains). Reconciled idempotently from the native DB row — the desired
//! set is computed on every change and a no-op deploy makes no DB change. The set
//! is bounded by [`MAX_REDIRECT_URIS`].
//!
//! ## Concurrency
//!
//! Reconciliation is **idempotent last-writer-wins**, NOT mutually-exclusive.
//! There is no row lock / advisory lock / `SELECT … FOR UPDATE`: `upsert_db_rows`
//! is plain `ON CONFLICT DO UPDATE`. This is safe today because each writer
//! recomputes a deterministic desired set from persisted state and writes it
//! wholesale — a race cannot drop, duplicate, or unboundedly grow URIs (the merge
//! in [`ensure_app_client`] is set-union + dedup + cap). It is NOT a
//! serialization guarantee: two writers with different host sets race to a
//! last-writer-wins outcome. Once custom-domain attach lands (see "Scope"
//! below), a concurrent apex-deploy + domain-attach with *stale* host snapshots
//! would need a per-app advisory lock to avoid one clobbering the other's
//! just-added host; until then every writer's input is apex-only so the union is
//! convergent.
//!
//! ## Scope (Slice 1d vs. Slice N)
//!
//! Slice 1d ships **apex-host-only** provisioning end-to-end: `create` and
//! `deploy` both call [`ensure_app_client`] with a single apex host
//! (`{name}.{app_base_domain}`). The multi-host surface —
//! [`sync_app_redirect_uris`], [`MAX_HOSTS`], the >1-host branch of
//! [`redirect_uris_for_hosts`], and the `≤102` cap — is **Slice-N-deferred**:
//! there is no custom-domain attach handler in the codebase yet, so these are
//! not yet wired to a production caller. They are kept (and unit-tested) because
//! the custom-domain attach path (a future slice) will call
//! [`sync_app_redirect_uris` ] / a multi-host [`ensure_app_client`] directly.
//! To keep apex-only deploys from clobbering URIs a future attach path adds,
//! [`ensure_app_client`] is **non-destructive**: it UNIONs the incoming
//! (apex) URIs with any URIs already on the native client row rather than
//! replacing the set wholesale (see its doc + [`merge_preserving_existing`]).

use compio_postgres::Client;
use rand::RngCore as _;
use uuid::Uuid;
use zeroship_core::auth::hash_api_key;
use zeroship_authz::Scope;
use zeroship_bundle::ScopeDef;
use zeroship_core::typed_id::{app_oauth_client_id, APP_OAUTH_CLIENT_PREFIX};

/// Per-app custom-domain cap (default 50). With 2 redirect_uris per host
/// (popup-callback + callback) the redirect_uri array is bounded at
/// `2 * (1 + 50) = 102` (apex host + up to 50 custom domains). Spec §1.1.
pub const MAX_HOSTS: usize = 51;

/// Hard upper bound on the redirect_uri array stored for a per-app
/// client: 2 per host × [`MAX_HOSTS`]. Spec §1.1 ("≤102 redirect_uris").
pub const MAX_REDIRECT_URIS: usize = MAX_HOSTS * 2;

/// Baseline scope allowlist for every per-app client. App-declared custom
/// scopes are appended by Subsystem 3 (Slice 3); the baseline always includes
/// `openid` + `offline_access` (refresh tokens) + `profile` + `email`.
pub const BASE_SCOPE: &str = "openid offline_access profile email";

/// The OAuth `client_id` prefix for per-app clients. Re-exported from
/// `zeroship_core::typed_id` — the SINGLE source of truth shared with the auth
/// consent classifier's decoder, so the two can never drift (spec §1.1 round-5).
/// Distinct from the `app_` *entity* typed_id namespace on purpose: the OAuth
/// `client_id` is a derived identifier, not a typed_id.
pub const APP_CLIENT_PREFIX: &str = APP_OAUTH_CLIENT_PREFIX;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum AppOauthClientError {
    /// Database write failed.
    Db(String),
    /// More hosts than [`MAX_HOSTS`] were requested.
    TooManyHosts { requested: usize, cap: usize },
    /// No hosts at all — a per-app client must have at least one redirect host.
    NoHosts,
    /// A declared `auth.scopes` id is malformed or collides with the closed
    /// platform-delegated vocabulary / a reserved identity scope (spec §5.1).
    InvalidScope { id: String, reason: String },
}

impl std::fmt::Display for AppOauthClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "database: {e}"),
            Self::TooManyHosts { requested, cap } => {
                write!(f, "too many hosts: {requested} > cap {cap}")
            }
            Self::NoHosts => write!(f, "no hosts supplied"),
            Self::InvalidScope { id, reason } => {
                write!(f, "invalid declared scope {id:?}: {reason}")
            }
        }
    }
}

impl std::error::Error for AppOauthClientError {}

type Result<T> = std::result::Result<T, AppOauthClientError>;

fn db_error(err: compio_postgres::Error) -> AppOauthClientError {
    AppOauthClientError::Db(error_with_source_chain(&err))
}

fn error_with_source_chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut cur = err.source();
    while let Some(source) = cur {
        out.push_str(": ");
        out.push_str(&source.to_string());
        cur = source.source();
    }
    out
}

// ---------------------------------------------------------------------------
// Pure derivations (no I/O — directly unit-testable)
// ---------------------------------------------------------------------------

/// Deterministic, stable-for-app-life OAuth `client_id`: `oac_<base62-app-id>`.
/// Delegates to the shared `zeroship_core::typed_id` minter so the auth-side
/// decoder (`app_id_from_oauth_client_id`) is the exact inverse.
#[must_use]
pub fn client_id_for_app(app_id: &Uuid) -> String {
    app_oauth_client_id(app_id)
}

/// The app's apex origin, used as the `sector_identifier` (pairwise/relay
/// scoping, spec §6.2) and as the `post_logout_redirect_uri` origin. Custom
/// domains do NOT change the sector — all of an app's hosts share the apex.
#[must_use]
pub fn sector_identifier(scheme: &str, apex_host: &str) -> String {
    format!("{scheme}://{apex_host}")
}

/// Reserved OIDC identity scopes (namespace-(b), platform-defined). An app may
/// NOT redeclare these in `auth.scopes` — they are always present in the
/// baseline allowlist ([`BASE_SCOPE`]) and rendered by the consent screen with
/// fixed labels (spec §5.1).
const RESERVED_IDENTITY_SCOPES: [&str; 4] = ["openid", "profile", "email", "offline_access"];

/// Reserved scope-id prefixes for future platform/org delegation (spec §5.1).
/// An app-declared scope may not start with one of these.
const RESERVED_SCOPE_PREFIXES: [&str; 2] = ["platform:", "org:"];

/// Validate the app's manifest-declared scopes (spec §5.1). Each id must:
/// 1. be well-formed (`verb:resource`, lowercase — [`ScopeDef::validate_id_format`]),
/// 2. NOT parse as a closed platform-delegated scope ([`Scope::parse`] —
///    `apps:*`, `env:*`, `secrets:*`, `billing:*`, `team:*`, `account:*`,
///    `deployments:*`), and
/// 3. NOT use a reserved prefix (`platform:`, `org:`) or be a bare reserved
///    identity scope (`openid`/`profile`/`email`/`offline_access`).
///
/// Binding the guard to `Scope::parse` (not a hand-listed prefix set) makes the
/// app-scope namespace and the platform-scope vocabulary provably disjoint, so
/// a declared scope can never be silently classified self-grantable while it
/// also parses as a real platform scope.
///
/// # Errors
/// [`AppOauthClientError::InvalidScope`] on the first offending id.
pub fn validate_app_scopes(scopes: &[ScopeDef]) -> Result<()> {
    for scope in scopes {
        let id = scope.id.as_str();
        // (1) format.
        if let Err(reason) = ScopeDef::validate_id_format(id) {
            return Err(AppOauthClientError::InvalidScope {
                id: id.to_string(),
                reason,
            });
        }
        // (2) closed platform-delegated vocabulary collision.
        if Scope::parse(id).is_ok() {
            return Err(AppOauthClientError::InvalidScope {
                id: id.to_string(),
                reason: "collides with a platform-delegated scope".to_string(),
            });
        }
        // (3) reserved prefixes + bare reserved identity scopes.
        if RESERVED_SCOPE_PREFIXES.iter().any(|p| id.starts_with(p)) {
            return Err(AppOauthClientError::InvalidScope {
                id: id.to_string(),
                reason: "uses a reserved scope prefix (platform:/org:)".to_string(),
            });
        }
        if RESERVED_IDENTITY_SCOPES.contains(&id) {
            return Err(AppOauthClientError::InvalidScope {
                id: id.to_string(),
                reason: "collides with a reserved identity scope".to_string(),
            });
        }
    }
    Ok(())
}

/// Build the per-app client `scope` allowlist string (spec §5.1):
/// `"openid offline_access profile email " + declared ids`, deduped and
/// order-stable (baseline first, then declared ids in manifest order). This is
/// the exact value written to the `control.oauth_clients.scopes` mirror.
///
/// Caller MUST have run [`validate_app_scopes`] first — a declared id here can
/// never duplicate a baseline scope (the validator rejects the reserved
/// identity scopes), so dedup only guards against a repeated declared id.
#[must_use]
pub fn build_scope_allowlist(declared: &[ScopeDef]) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<&str> = Vec::new();
    for s in BASE_SCOPE
        .split_whitespace()
        .chain(declared.iter().map(|s| s.id.as_str()))
    {
        if seen.insert(s) {
            out.push(s);
        }
    }
    out.join(" ")
}

/// The two same-origin OAuth callback paths registered per host (spec §1.1).
const CALLBACK_PATHS: [&str; 2] = ["/__zeroship/auth/popup-callback", "/__zeroship/auth/callback"];

/// Compute the full desired redirect_uri set for a host list. Two URIs per host
/// (popup-callback + callback). Order is stable (host order, then
/// popup-callback before callback) so the diff is deterministic.
///
/// # Errors
/// [`AppOauthClientError::NoHosts`] when `hosts` is empty;
/// [`AppOauthClientError::TooManyHosts`] when it exceeds [`MAX_HOSTS`].
pub fn redirect_uris_for_hosts(scheme: &str, hosts: &[String]) -> Result<Vec<String>> {
    if hosts.is_empty() {
        return Err(AppOauthClientError::NoHosts);
    }
    if hosts.len() > MAX_HOSTS {
        return Err(AppOauthClientError::TooManyHosts {
            requested: hosts.len(),
            cap: MAX_HOSTS,
        });
    }
    let mut uris = Vec::with_capacity(hosts.len() * CALLBACK_PATHS.len());
    for host in hosts {
        for path in CALLBACK_PATHS {
            uris.push(format!("{scheme}://{host}{path}"));
        }
    }
    Ok(uris)
}

/// The per-app `backchannel_logout_uri` (spec §1.1 / §1.2): a stable per-host
/// path the OP POSTs the `logout_token` to. The gateway handler disambiguates
/// *which app* from the token's `aud` (= this client's `client_id`). Anchored
/// at the apex host.
#[must_use]
pub fn backchannel_logout_uri(scheme: &str, apex_host: &str) -> String {
    format!("{scheme}://{apex_host}/oidc/backchannel-logout")
}

/// The per-app `post_logout_redirect_uris` (spec §1.1): the apex origin root.
#[must_use]
pub fn post_logout_redirect_uris(scheme: &str, apex_host: &str) -> Vec<String> {
    vec![format!("{scheme}://{apex_host}/")]
}

/// Redirect reconciliation core (spec §1.1). Compare the client's current
/// redirect_uris against the desired set and return `Some(desired)` only when
/// they differ (order-insensitively), else `None` (a no-op).
///
/// This is the load-bearing idempotency logic. Both the add path (new host),
/// the remove path (detached host), and the no-op path (unchanged deploy) flow
/// through here. It is pure (no I/O) so it is unit-tested directly.
#[must_use]
pub fn reconcile_redirect_uris(current: &[String], desired: &[String]) -> Option<Vec<String>> {
    let mut cur_sorted: Vec<&str> = current.iter().map(String::as_str).collect();
    let mut des_sorted: Vec<&str> = desired.iter().map(String::as_str).collect();
    cur_sorted.sort_unstable();
    cur_sorted.dedup();
    des_sorted.sort_unstable();
    des_sorted.dedup();
    if cur_sorted == des_sorted {
        None
    } else {
        Some(desired.to_vec())
    }
}

/// Non-destructive merge for the create/deploy path: the union of the URIs
/// already on the native client row with the incoming desired URIs, deduped and
/// order-stable (incoming first in `desired` order, then any pre-existing extras
/// in `existing` order).
///
/// This is why an apex-only re-provision ([`ensure_app_client`]) can't clobber
/// redirect_uris a future custom-domain attach added: the apex deploy carries
/// only its own (apex) URIs, the merge re-adds the previously-registered
/// custom-domain URIs. The detach (shrink) path is the custom-domain attach
/// handler's job via [`sync_app_redirect_uris`], which sets the full host set
/// wholesale — it is the only path allowed to remove URIs.
#[must_use]
pub fn merge_preserving_existing(existing: &[String], desired: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(desired.len() + existing.len());
    for uri in desired.iter().chain(existing.iter()) {
        if seen.insert(uri.as_str()) {
            out.push(uri.clone());
        }
    }
    out
}

#[derive(Clone, Debug)]
struct ExistingClient {
    redirect_uris: Vec<String>,
}

async fn existing_client(pg: &Client, client_id: &str) -> Result<Option<ExistingClient>> {
    let rows = pg
        .query(
            "SELECT redirect_uris FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .map_err(db_error)?;
    Ok(rows.first().map(|row| ExistingClient {
        redirect_uris: row.get("redirect_uris"),
    }))
}

fn generate_client_secret_hash() -> String {
    let mut bytes = [0_u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hash_api_key(&hex::encode(bytes))
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Idempotently create-or-sync the per-app client and persist the
/// `control.oauth_clients` row + `control.app_oauth_clients` extension row.
///
/// Spec §1.1 lifecycle: called on app **create** AND on **deploy**. Slice 1d
/// always passes the single apex host (`{name}.{app_base_domain}`); the
/// multi-host / custom-domain reconciliation surface is Slice-N-deferred (see
/// the module-level "Scope" doc and [`sync_app_redirect_uris`]). The DB rows
/// are upserted so a re-run is a no-op.
///
/// **Non-destructive on re-provision.** This is an additive, set-union path: the
/// reconcile UNIONs the incoming (apex) URIs with whatever is already on the
/// native client row (see [`merge_preserving_existing`]). An apex-only deploy
/// therefore CANNOT drop redirect_uris a future custom-domain attach added —
/// only [`sync_app_redirect_uris`] (the attach handler's path, which sets the
/// full host set wholesale) is allowed to remove URIs. The merged set is bounded
/// by [`MAX_REDIRECT_URIS`] to keep a divergent client from growing unboundedly.
///
/// `hosts[0]` is treated as the apex host (sector + BCL + post-logout origin);
/// all hosts contribute redirect_uris.
///
/// **Declared scopes (Slice 3, spec §5.1 O8 mirror).** `declared_scopes` are
/// the app's manifest `auth.scopes`. They are validated ([`validate_app_scopes`]
/// — format + platform-vocab collision) BEFORE any DB write, then mirrored
/// ATOMICALLY: `control.oauth_clients.scopes` is set to `"openid offline_access
/// profile email " + declared ids`, and `control.app_scope_defs` is replaced
/// with the declared set — both in the same control-plane transaction as the
/// client rows.
/// Pass `&[]` for an app declaring no custom scopes (the baseline allowlist).
///
/// **`first_party` / `skip_consent` (spec §5.2 + the first-party-console
/// exception).** Every creator app passes `first_party = false` ⇒
/// `skip_consent = false`: the consent prompt MUST fire (the single grant
/// ledger depends on it). Spec §5.2's "per-app clients NEVER skip consent"
/// targets THIRD-PARTY creator apps. The ONE deliberate, narrow exception is
/// the platform's own **first-party console**, which passes
/// `first_party = true` ⇒ `skip_consent = true` (see
/// [`crate::bootstrap_console`]) — a consent prompt for the platform's own
/// surface is meaningless. This exception applies ONLY to the console, NEVER to
/// creator apps. The flag is mirrored to `control.oauth_clients.skip_consent`
/// in the same transaction as the client rows.
///
/// # Errors
/// [`AppOauthClientError`] on DB failure, an invalid host list, a merged set
/// that would exceed [`MAX_REDIRECT_URIS`], or an invalid declared scope.
pub async fn ensure_app_client(
    pg: &mut Client,
    app_id: &Uuid,
    app_name: &str,
    scheme: &str,
    hosts: &[String],
    declared_scopes: &[ScopeDef],
    first_party: bool,
) -> Result<String> {
    // Validate declared scopes FIRST — reject before touching the DB
    // so a bad manifest can never half-provision (spec §5.1).
    validate_app_scopes(declared_scopes)?;
    let scope_allowlist = build_scope_allowlist(declared_scopes);

    let apex_host = hosts.first().ok_or(AppOauthClientError::NoHosts)?.clone();
    let client_id = client_id_for_app(app_id);
    let incoming_uris = redirect_uris_for_hosts(scheme, hosts)?;

    // `effective_uris` is what is mirrored in the DB. On create it's exactly
    // the incoming set; on reconcile it's the non-destructive union with the
    // existing native row so an apex-only deploy can't clobber attach URIs.
    let effective_uris = if let Some(existing) = existing_client(pg, &client_id).await? {
        let merged = merge_preserving_existing(&existing.redirect_uris, &incoming_uris);
        if merged.len() > MAX_REDIRECT_URIS {
            return Err(AppOauthClientError::TooManyHosts {
                requested: merged.len().div_ceil(CALLBACK_PATHS.len()),
                cap: MAX_HOSTS,
            });
        }
        merged
    } else {
        incoming_uris
    };

    let sector = sector_identifier(scheme, &apex_host);
    upsert_db_rows(
        pg,
        app_id,
        &client_id,
        app_name,
        &sector,
        &effective_uris,
        &scope_allowlist,
        declared_scopes,
        first_party,
    )
    .await?;
    Ok(client_id)
}

/// Reconcile just the redirect_uris for an already-provisioned client (deploy /
/// domain attach fast path). Computes the desired set from `hosts`, diffs
/// against the native client row, and updates only on a change.
///
/// Returns `true` when a DB update was issued, `false` on a no-op.
///
/// # Errors
/// [`AppOauthClientError`] on DB failure or an invalid host list.
pub async fn sync_app_redirect_uris(
    pg: &mut Client,
    app_id: &Uuid,
    app_name: &str,
    scheme: &str,
    hosts: &[String],
) -> Result<bool> {
    let apex_host = hosts.first().ok_or(AppOauthClientError::NoHosts)?.clone();
    let client_id = client_id_for_app(app_id);
    let desired_uris = redirect_uris_for_hosts(scheme, hosts)?;

    let Some(existing) = existing_client(pg, &client_id).await? else {
        // Not provisioned yet — caller should ensure_app_client first. Treat as
        // a full provision so deploy is self-healing. No manifest is in hand on
        // this redirect-only path, so the client gets the baseline scope
        // allowlist; the next `ensure_app_client` (deploy) re-mirrors declared
        // scopes. The scope-defs registry is owned by `ensure_app_client`, so
        // this fallback writes the empty declared set.
        let sector = sector_identifier(scheme, &apex_host);
        upsert_db_rows(
            pg,
            app_id,
            &client_id,
            app_name,
            &sector,
            &desired_uris,
            BASE_SCOPE,
            &[],
            false,
        )
        .await?;
        return Ok(true);
    };

    match reconcile_redirect_uris(&existing.redirect_uris, &desired_uris) {
        None => Ok(false),
        Some(next) => {
            update_redirect_uri_mirror(pg, &client_id, &next).await?;
            Ok(true)
        }
    }
}

/// Upsert the `control.oauth_clients` identity row, the
/// `control.app_oauth_clients` extension row, AND the `control.app_scope_defs`
/// declared-scope registry — all in ONE transaction (spec §8.1 / §5.1 O8
/// mirror). The `oauth_clients.scopes` mirror and the `app_scope_defs` rows are
/// derived from the SAME validated `scope_allowlist` / `declared_scopes`, so
/// the mirror and the registry can never disagree — which is what keeps the
/// consent classifier sound.
async fn upsert_db_rows(
    pg: &mut Client,
    app_id: &Uuid,
    client_id: &str,
    client_name: &str,
    sector: &str,
    redirect_uris: &[String],
    scope_allowlist: &str,
    declared_scopes: &[ScopeDef],
    first_party: bool,
) -> Result<()> {
    let scopes: Vec<&str> = scope_allowlist.split_whitespace().collect();
    let redirect_uris: Vec<&str> = redirect_uris.iter().map(String::as_str).collect();
    let created_by: Option<Uuid> = None;
    let client_secret_hash = generate_client_secret_hash();
    let bcl_uri = backchannel_logout_uri(
        sector
            .split_once("://")
            .map(|(scheme, _)| scheme)
            .unwrap_or("https"),
        sector
            .split_once("://")
            .map(|(_, host)| host)
            .unwrap_or(sector),
    );

    let tx = pg
        .transaction()
        .await
        .map_err(db_error)?;

    // control.oauth_clients — the FK target + skip_consent reader.
    // `skip_consent` is FALSE for every creator (per-app end-user) client
    // (spec §5.2), TRUE only for the platform's own first-party console
    // (`first_party`). `scopes` mirrors the baseline + declared allowlist.
    // TODO(P6): drop oauth_clients.hydra_client_id column; placeholder write until then.
    tx.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, client_uri, logo_uri, redirect_uris, scopes, \
             skip_consent, created_by, hydra_client_id, client_secret_hash, refresh_allowed, \
             token_endpoint_auth_method, brokered, backchannel_logout_uri) \
         VALUES ($1, $2, NULL, NULL, $3, $4, $6, $5, $1, $8, TRUE, \
                 'client_secret_basic', TRUE, $7) \
         ON CONFLICT (client_id) DO UPDATE SET \
            client_name = EXCLUDED.client_name, \
            redirect_uris = EXCLUDED.redirect_uris, \
            scopes = EXCLUDED.scopes, \
            skip_consent = EXCLUDED.skip_consent, \
            refresh_allowed = TRUE, \
            token_endpoint_auth_method = 'client_secret_basic', \
            brokered = TRUE, \
            backchannel_logout_uri = EXCLUDED.backchannel_logout_uri",
        &[
            &client_id,
            &client_name,
            &redirect_uris,
            &scopes,
            &created_by,
            &first_party,
            &bcl_uri,
            &client_secret_hash,
        ],
    )
    .await
    .map_err(db_error)?;

    // control.app_oauth_clients — the per-app extension (app link + sector).
    tx.execute(
        "INSERT INTO zeroship.app_oauth_clients \
            (app_id, client_id, sector_identifier) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (app_id) DO UPDATE SET \
            sector_identifier = EXCLUDED.sector_identifier, \
            updated_at = NOW()",
        &[app_id, &client_id, &sector],
    )
    .await
    .map_err(db_error)?;

    // control.app_scope_defs — REPLACE the declared-scope registry for this app
    // wholesale (delete-then-insert): a deploy that drops a previously-declared
    // scope must remove its row, so the registry exactly tracks the current
    // manifest. Same transaction ⇒ atomic with the allowlist mirror above.
    tx.execute(
        "DELETE FROM zeroship.app_scope_defs WHERE app_id = $1",
        &[app_id],
    )
    .await
    .map_err(db_error)?;
    for scope in declared_scopes {
        tx.execute(
            "INSERT INTO zeroship.app_scope_defs (app_id, scope_id, label, description) \
             VALUES ($1, $2, $3, $4)",
            &[app_id, &scope.id, &scope.label, &scope.description],
        )
        .await
        .map_err(db_error)?;
    }

    tx.commit()
        .await
        .map_err(db_error)?;
    Ok(())
}

/// Refresh just the `control.oauth_clients.redirect_uris` mirror after a
/// redirect sync (the single source of truth for redirect_uris is the
/// `oauth_clients` row, spec §8.1).
async fn update_redirect_uri_mirror(
    pg: &Client,
    client_id: &str,
    redirect_uris: &[String],
) -> Result<()> {
    let redirect_uris: Vec<&str> = redirect_uris.iter().map(String::as_str).collect();
    pg.execute(
        "UPDATE zeroship.oauth_clients SET redirect_uris = $2 WHERE client_id = $1",
        &[&client_id, &redirect_uris],
    )
    .await
    .map_err(db_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── client_id derivation ────────────────────────────────────────────────

    #[test]
    fn client_id_is_deterministic_and_oac_prefixed() {
        let app = Uuid::parse_str("018f3a4b-5c6d-7e8f-9a0b-1c2d3e4f5a6b").unwrap();
        let a = client_id_for_app(&app);
        let b = client_id_for_app(&app);
        assert_eq!(a, b, "stable for app life");
        assert!(a.starts_with("oac_"), "got {a}");
        // oac_ + 22 base62 chars.
        assert_eq!(a.len(), 26, "got {a}");
        // Distinct from the app_ entity typed_id namespace.
        assert!(!a.starts_with("app_"));
    }

    #[test]
    fn distinct_apps_get_distinct_client_ids() {
        let a = client_id_for_app(&Uuid::new_v4());
        let b = client_id_for_app(&Uuid::new_v4());
        assert_ne!(a, b);
    }

    // ── redirect_uri computation ────────────────────────────────────────────

    #[test]
    fn redirect_uris_are_two_per_host_popup_then_callback() {
        let hosts = vec!["app.zeroship.localhost".to_string()];
        let uris = redirect_uris_for_hosts("http", &hosts).unwrap();
        assert_eq!(
            uris,
            vec![
                "http://app.zeroship.localhost/__zeroship/auth/popup-callback".to_string(),
                "http://app.zeroship.localhost/__zeroship/auth/callback".to_string(),
            ]
        );
    }

    #[test]
    fn redirect_uris_cover_every_host() {
        let hosts = vec![
            "apex.zeroship.ai".to_string(),
            "custom.example.com".to_string(),
        ];
        let uris = redirect_uris_for_hosts("https", &hosts).unwrap();
        assert_eq!(uris.len(), 4);
        assert!(uris.contains(&"https://apex.zeroship.ai/__zeroship/auth/callback".to_string()));
        assert!(uris.contains(&"https://custom.example.com/__zeroship/auth/popup-callback".to_string()));
    }

    #[test]
    fn redirect_uris_reject_empty_host_list() {
        assert!(matches!(
            redirect_uris_for_hosts("https", &[]),
            Err(AppOauthClientError::NoHosts)
        ));
    }

    #[test]
    fn redirect_uris_enforce_host_cap() {
        let hosts: Vec<String> = (0..=MAX_HOSTS).map(|i| format!("h{i}.zeroship.ai")).collect();
        assert_eq!(hosts.len(), MAX_HOSTS + 1);
        let err = redirect_uris_for_hosts("https", &hosts).unwrap_err();
        assert!(matches!(
            err,
            AppOauthClientError::TooManyHosts { requested, cap }
                if requested == MAX_HOSTS + 1 && cap == MAX_HOSTS
        ));
    }

    #[test]
    fn redirect_uris_at_cap_yield_102() {
        let hosts: Vec<String> = (0..MAX_HOSTS).map(|i| format!("h{i}.zeroship.ai")).collect();
        let uris = redirect_uris_for_hosts("https", &hosts).unwrap();
        assert_eq!(uris.len(), MAX_REDIRECT_URIS);
        assert_eq!(uris.len(), 102);
    }

    // ── redirect reconciliation (the load-bearing idempotency) ──────────────

    #[test]
    fn reconcile_noop_when_identical() {
        let cur = vec!["a".to_string(), "b".to_string()];
        let des = vec!["a".to_string(), "b".to_string()];
        assert_eq!(reconcile_redirect_uris(&cur, &des), None);
    }

    #[test]
    fn reconcile_noop_when_only_order_differs() {
        // Stored redirect_uris may be in any order; an order-only delta must
        // be a no-op (spec §1.1).
        let cur = vec!["b".to_string(), "a".to_string()];
        let des = vec!["a".to_string(), "b".to_string()];
        assert_eq!(reconcile_redirect_uris(&cur, &des), None);
    }

    #[test]
    fn reconcile_puts_on_added_host() {
        let cur = vec![
            "https://apex.zeroship.ai/__zeroship/auth/popup-callback".to_string(),
            "https://apex.zeroship.ai/__zeroship/auth/callback".to_string(),
        ];
        let des = redirect_uris_for_hosts(
            "https",
            &[
                "apex.zeroship.ai".to_string(),
                "custom.example.com".to_string(),
            ],
        )
        .unwrap();
        let out = reconcile_redirect_uris(&cur, &des).expect("a diff");
        assert_eq!(out, des, "diff carries the full desired set");
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn reconcile_puts_on_removed_host() {
        let cur = redirect_uris_for_hosts(
            "https",
            &[
                "apex.zeroship.ai".to_string(),
                "custom.example.com".to_string(),
            ],
        )
        .unwrap();
        let des =
            redirect_uris_for_hosts("https", &["apex.zeroship.ai".to_string()]).unwrap();
        let out = reconcile_redirect_uris(&cur, &des).expect("a diff");
        assert_eq!(out, des);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn reconcile_treats_duplicates_as_a_set() {
        let cur = vec!["a".to_string(), "a".to_string(), "b".to_string()];
        let des = vec!["a".to_string(), "b".to_string()];
        assert_eq!(reconcile_redirect_uris(&cur, &des), None);
    }

    // ── non-destructive merge (apex deploy must not clobber attach URIs) ─────

    #[test]
    fn merge_is_union_dedup_desired_first() {
        let existing = vec![
            "https://custom.example.com/__zeroship/auth/popup-callback".to_string(),
            "https://apex.zeroship.ai/__zeroship/auth/popup-callback".to_string(),
        ];
        let desired = vec![
            "https://apex.zeroship.ai/__zeroship/auth/popup-callback".to_string(),
            "https://apex.zeroship.ai/__zeroship/auth/callback".to_string(),
        ];
        let merged = merge_preserving_existing(&existing, &desired);
        // Desired first (in order), then existing extras, deduped.
        assert_eq!(
            merged,
            vec![
                "https://apex.zeroship.ai/__zeroship/auth/popup-callback".to_string(),
                "https://apex.zeroship.ai/__zeroship/auth/callback".to_string(),
                "https://custom.example.com/__zeroship/auth/popup-callback".to_string(),
            ]
        );
    }

    #[test]
    fn merge_noop_when_desired_subset_of_existing() {
        // Apex-only deploy when the DB row already has apex+custom → no new URIs, so
        // reconcile against the merged set is a no-op (order-insensitive).
        let existing = redirect_uris_for_hosts(
            "https",
            &["apex.zeroship.ai".to_string(), "custom.example.com".to_string()],
        )
        .unwrap();
        let desired =
            redirect_uris_for_hosts("https", &["apex.zeroship.ai".to_string()]).unwrap();
        let merged = merge_preserving_existing(&existing, &desired);
        assert_eq!(
            reconcile_redirect_uris(&existing, &merged),
            None,
            "apex-only re-provision over an apex+custom client is a no-op — \
             custom-domain URIs are preserved, never clobbered"
        );
    }

    #[test]
    fn sector_identifier_is_apex_origin() {
        assert_eq!(
            sector_identifier("https", "apex.zeroship.ai"),
            "https://apex.zeroship.ai"
        );
    }

    // ── declared-scope validation (spec §5.1) ───────────────────────────────

    #[test]
    fn validate_rejects_platform_vocab_collision() {
        // billing:read is in the closed Scope::parse vocabulary — it MUST be
        // rejected even though it is a well-formed verb:resource id (the
        // round-1 hole this guard closes).
        let scopes = vec![ScopeDef {
            id: "billing:read".to_string(),
            label: "x".to_string(),
            description: None,
        }];
        let err = validate_app_scopes(&scopes).unwrap_err();
        assert!(
            matches!(&err, AppOauthClientError::InvalidScope { id, .. } if id == "billing:read"),
            "got {err}"
        );
    }

    #[test]
    fn validate_rejects_every_closed_platform_scope() {
        // Every member of the closed vocabulary must be rejected — pins the
        // disjointness invariant against the full set, not just one example.
        for id in [
            "apps:read", "apps:deploy", "env:read", "env:write", "secrets:read",
            "secrets:write", "billing:read", "billing:write", "team:read", "team:write",
            "account:read", "account:write", "deployments:read", "deployments:rollback",
        ] {
            let scopes = vec![ScopeDef { id: id.to_string(), label: "x".into(), description: None }];
            assert!(
                validate_app_scopes(&scopes).is_err(),
                "platform scope {id} must be rejected as an app-declared scope"
            );
        }
    }

    #[test]
    fn validate_rejects_reserved_prefixes_and_identity() {
        for id in ["platform:admin", "org:owner", "openid", "profile", "email", "offline_access"] {
            let scopes = vec![ScopeDef { id: id.to_string(), label: "x".into(), description: None }];
            assert!(validate_app_scopes(&scopes).is_err(), "reserved {id} must be rejected");
        }
    }

    #[test]
    fn validate_rejects_malformed_ids() {
        for id in ["Read:Billing", "read billing", "1read", ":read", "read:"] {
            let scopes = vec![ScopeDef { id: id.to_string(), label: "x".into(), description: None }];
            assert!(validate_app_scopes(&scopes).is_err(), "malformed {id} must be rejected");
        }
    }

    #[test]
    fn validate_accepts_noncolliding_app_scopes() {
        // The mirror image of the platform-collision test: read:billing (note
        // the order) is NOT in the platform vocabulary, so it is accepted.
        let scopes = vec![
            ScopeDef { id: "read:billing".into(), label: "View billing".into(), description: None },
            ScopeDef { id: "write:projects".into(), label: "Manage".into(), description: None },
        ];
        validate_app_scopes(&scopes).expect("non-colliding app scopes accepted");
    }

    // ── allowlist string (baseline + declared, deduped) ─────────────────────

    #[test]
    fn allowlist_appends_declared_after_baseline() {
        let declared = vec![
            ScopeDef { id: "read:billing".into(), label: "x".into(), description: None },
            ScopeDef { id: "write:projects".into(), label: "y".into(), description: None },
        ];
        assert_eq!(
            build_scope_allowlist(&declared),
            "openid offline_access profile email read:billing write:projects"
        );
    }

    #[test]
    fn allowlist_empty_declared_is_baseline() {
        assert_eq!(build_scope_allowlist(&[]), BASE_SCOPE);
    }

}
