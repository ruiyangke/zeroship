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
//! custom domains). Reconciled idempotently — the desired set is computed on
//! every change and a Hydra `PUT` is issued only on a diff (a no-op deploy makes
//! no Hydra call). The set is bounded by [`MAX_REDIRECT_URIS`].
//!
//! ## Concurrency
//!
//! Reconciliation is **idempotent last-writer-wins**, NOT mutually-exclusive.
//! There is no row lock / advisory lock / `SELECT … FOR UPDATE`: `upsert_db_rows`
//! is plain `ON CONFLICT DO UPDATE` and the Hydra `PUT` is last-writer-wins. This
//! is safe today because each writer recomputes a deterministic desired set from
//! persisted state and writes it wholesale — a race cannot drop, duplicate, or
//! unboundedly grow URIs (the merge in [`ensure_app_client`] is set-union +
//! dedup + cap). It is NOT a serialization guarantee: two writers with different
//! host sets race to a last-writer-wins outcome. Once custom-domain attach lands
//! (see "Scope" below), a concurrent apex-deploy + domain-attach with *stale*
//! host snapshots would need a per-app advisory lock to avoid one clobbering the
//! other's just-added host; until then every writer's input is apex-only so the
//! union is convergent.
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
//! (apex) URIs with any URIs already on the live Hydra client rather than
//! replacing the set wholesale (see its doc + [`merge_preserving_existing`]).
//!
//! The Hydra-admin interaction sits behind the [`HydraClientAdmin`] seam so the
//! reconciliation logic ([`reconcile_redirect_uris`]) is exercised by real unit
//! tests without a live Hydra; the production seam is `zeroship_auth`'s
//! `HydraAdmin` (the same admin client `oauth_handlers` / `bootstrap_builder`
//! already use).

use compio_postgres::Client;
use uuid::Uuid;
use zeroship_auth::hydra_client::types::OAuth2Client;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_authz::Scope;
use zeroship_bundle::ScopeDef;
use zeroship_core::typed_id::{app_oauth_client_id, APP_OAUTH_CLIENT_PREFIX};

/// Per-app custom-domain cap (default 50). With 2 redirect_uris per host
/// (popup-callback + callback) the redirect_uri array is bounded at
/// `2 * (1 + 50) = 102` (apex host + up to 50 custom domains). Spec §1.1.
pub const MAX_HOSTS: usize = 51;

/// Hard upper bound on the redirect_uri array Hydra stores for a per-app
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
    /// Hydra admin API call failed (transport / non-2xx / decode).
    Hydra(String),
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
            Self::Hydra(e) => write!(f, "hydra admin: {e}"),
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

/// Build the per-app Hydra client `scope` allowlist string (spec §5.1):
/// `"openid offline_access profile email " + declared ids`, deduped and
/// order-stable (baseline first, then declared ids in manifest order). This is
/// the exact value written to BOTH the live Hydra client and the
/// `control.oauth_clients.scopes` mirror, so the two never drift.
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
/// path Hydra POSTs the `logout_token` to. The gateway handler disambiguates
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

/// Diff-then-PUT reconciliation core (spec §1.1). Compare the client's current
/// redirect_uris against the desired set and return `Some(desired)` only when
/// they differ (order-insensitively), else `None` (a no-op — caller issues no
/// Hydra `PUT`).
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
/// already on the live Hydra client with the incoming desired URIs, deduped and
/// order-stable (incoming first in `desired` order, then any pre-existing extras
/// in `existing` order).
///
/// This is why an apex-only re-provision ([`ensure_app_client`]) can't clobber
/// redirect_uris a future custom-domain attach added: the apex deploy carries
/// only its own (apex) URIs, the merge re-adds the previously-registered
/// custom-domain URIs, and the diff is a no-op. The detach (shrink) path is the
/// custom-domain attach handler's job via [`sync_app_redirect_uris`], which sets
/// the full host set wholesale — it is the only path allowed to remove URIs.
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

// ---------------------------------------------------------------------------
// Hydra-admin seam
// ---------------------------------------------------------------------------

/// The narrow set of Hydra-admin verbs the per-app client lifecycle needs.
/// Implemented for `zeroship_auth`'s [`HydraAdmin`] in production; a test
/// double implements it so [`reconcile_redirect_uris`] and the lifecycle wiring
/// are exercised without a live Hydra. We do NOT hand-roll a new admin client —
/// the production impl delegates straight to the existing `HydraAdmin` verbs.
pub trait HydraClientAdmin {
    /// `GET /admin/clients/{id}` → `Ok(None)` on 404.
    fn get_client(
        &self,
        client_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<OAuth2Client>>>;

    /// `POST /admin/clients`.
    fn create_client(
        &self,
        client: &OAuth2Client,
    ) -> impl std::future::Future<Output = Result<OAuth2Client>>;

    /// `PUT /admin/clients/{id}`.
    fn update_client(
        &self,
        client: &OAuth2Client,
    ) -> impl std::future::Future<Output = Result<OAuth2Client>>;

    /// `DELETE /admin/clients/{id}`.
    fn delete_client(&self, client_id: &str) -> impl std::future::Future<Output = Result<()>>;
}

impl HydraClientAdmin for HydraAdmin {
    async fn get_client(&self, client_id: &str) -> Result<Option<OAuth2Client>> {
        HydraAdmin::get_client(self, client_id)
            .await
            .map_err(|e| AppOauthClientError::Hydra(e.to_string()))
    }

    async fn create_client(&self, client: &OAuth2Client) -> Result<OAuth2Client> {
        HydraAdmin::create_client(self, client)
            .await
            .map_err(|e| AppOauthClientError::Hydra(e.to_string()))
    }

    async fn update_client(&self, client: &OAuth2Client) -> Result<OAuth2Client> {
        HydraAdmin::update_client(self, client)
            .await
            .map_err(|e| AppOauthClientError::Hydra(e.to_string()))
    }

    async fn delete_client(&self, client_id: &str) -> Result<()> {
        // Idempotent: Hydra returns 404 for an already-gone (or
        // never-provisioned) client. `HydraAdmin::delete()` surfaces that as a
        // `→ 404` error string; treat it as success so the app-delete path and
        // any retry are safe even when create-time provisioning never ran.
        match HydraAdmin::delete_client(self, client_id).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("→ 404") {
                    Ok(())
                } else {
                    Err(AppOauthClientError::Hydra(msg))
                }
            }
        }
    }
}

/// Build the canonical public-PKCE [`OAuth2Client`] body for an app. Shared by
/// the create and redirect-sync paths so the two never drift.
///
/// `first_party` drives `skip_consent` (spec §5.2 + the first-party-console
/// exception): `false` for every creator app (the consent prompt MUST fire),
/// `true` only for the platform's own first-party console. See
/// [`ensure_app_client`].
fn build_client_body(
    client_id: &str,
    client_name: &str,
    scheme: &str,
    apex_host: &str,
    redirect_uris: Vec<String>,
    scope: &str,
    first_party: bool,
) -> OAuth2Client {
    OAuth2Client {
        client_id: client_id.to_string(),
        client_name: Some(client_name.to_string()),
        client_secret: None,
        grant_types: vec!["authorization_code".into(), "refresh_token".into()],
        response_types: vec!["code".into()],
        redirect_uris,
        post_logout_redirect_uris: post_logout_redirect_uris(scheme, apex_host),
        scope: scope.to_string(),
        token_endpoint_auth_method: "none".to_string(),
        subject_type: "public".to_string(),
        access_token_strategy: Some("jwt".to_string()),
        id_token_signed_response_alg: Some("EdDSA".to_string()),
        audience: Vec::new(),
        // skip_consent is FALSE for every creator (per-app end-user) client
        // (spec §5.2 round-3): the single grant ledger requires the consent
        // prompt to fire. The ONLY exception is the platform's own first-party
        // console (`first_party == true`) — a consent prompt for the platform's
        // own surface is meaningless. This exception applies ONLY to the
        // console, NEVER to creator apps.
        skip_consent: first_party,
        // Deliberate divergence from clients_config::to_oauth2_client, which
        // sets require_consent = !first_party (i.e. TRUE for a non-skip client).
        // We want first-grant-then-remembered consent for SSO: the prompt fires
        // on the FIRST authorization (recording the grant in the ledger), then
        // Hydra's remembered-consent serves subsequent logins silently.
        // require_consent=true would force the prompt on EVERY login, defeating
        // SSO; skip_consent=false (creator apps) already guarantees the
        // first-grant prompt, which is all spec §5.2 mandates. The console
        // (skip_consent=true) never prompts at all. Do NOT "fix" require_consent
        // to match the first-party template — creator apps are end-user clients.
        require_consent: false,
        require_logout_consent: false,
        frontchannel_logout_uri: None,
        backchannel_logout_uri: Some(backchannel_logout_uri(scheme, apex_host)),
    }
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Idempotently create-or-sync the per-app public PKCE client and persist the
/// `control.oauth_clients` row + `control.app_oauth_clients` extension row.
///
/// Spec §1.1 lifecycle: called on app **create** AND on **deploy**. Slice 1d
/// always passes the single apex host (`{name}.{app_base_domain}`); the
/// multi-host / custom-domain reconciliation surface is Slice-N-deferred (see
/// the module-level "Scope" doc and [`sync_app_redirect_uris`]). On create the
/// Hydra client is `POST`ed; on subsequent calls the redirect_uris are
/// reconciled (diff-then-PUT). The DB rows are upserted so a re-run is a no-op.
///
/// **Non-destructive on re-provision.** This is an additive, set-union path: the
/// reconcile UNIONs the incoming (apex) URIs with whatever is already on the
/// live Hydra client (see [`merge_preserving_existing`]). An apex-only deploy
/// therefore CANNOT drop redirect_uris a future custom-domain attach added —
/// only [`sync_app_redirect_uris`] (the attach handler's path, which sets the
/// full host set wholesale) is allowed to remove URIs. The merged set is bounded
/// by [`MAX_REDIRECT_URIS`] to keep a divergent Hydra client from growing
/// unboundedly.
///
/// `hosts[0]` is treated as the apex host (sector + BCL + post-logout origin);
/// all hosts contribute redirect_uris.
///
/// **Declared scopes (Slice 3, spec §5.1 O8 mirror).** `declared_scopes` are
/// the app's manifest `auth.scopes`. They are validated ([`validate_app_scopes`]
/// — format + platform-vocab collision) BEFORE any Hydra/DB write, then mirrored
/// ATOMICALLY: the per-app Hydra client `scope` allowlist is set to
/// `"openid offline_access profile email " + declared ids`, and
/// `control.app_scope_defs` is replaced with the declared set — both in the same
/// control-plane transaction as the client rows. The allowlist diff feeds the
/// existing diff-then-PUT so a no-op-scope deploy still issues no Hydra call.
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
/// in the same transaction as the client rows, so the DB and the live Hydra
/// client never disagree.
///
/// # Errors
/// [`AppOauthClientError`] on Hydra-admin failure, DB failure, an invalid host
/// list, a merged set that would exceed [`MAX_REDIRECT_URIS`], or an invalid
/// declared scope.
pub async fn ensure_app_client(
    pg: &mut Client,
    hydra: &impl HydraClientAdmin,
    app_id: &Uuid,
    app_name: &str,
    scheme: &str,
    hosts: &[String],
    declared_scopes: &[ScopeDef],
    first_party: bool,
) -> Result<String> {
    // Validate declared scopes FIRST — reject before touching Hydra or the DB
    // so a bad manifest can never half-provision (spec §5.1).
    validate_app_scopes(declared_scopes)?;
    let scope_allowlist = build_scope_allowlist(declared_scopes);

    let apex_host = hosts.first().ok_or(AppOauthClientError::NoHosts)?.clone();
    let client_id = client_id_for_app(app_id);
    let incoming_uris = redirect_uris_for_hosts(scheme, hosts)?;

    // `effective_uris` is what ends up on Hydra + mirrored in the DB. On create
    // it's exactly the incoming set; on reconcile it's the non-destructive union
    // with the live client so an apex-only deploy can't clobber attach URIs.
    let effective_uris = match hydra.get_client(&client_id).await? {
        None => {
            let body = build_client_body(
                &client_id,
                app_name,
                scheme,
                &apex_host,
                incoming_uris.clone(),
                &scope_allowlist,
                first_party,
            );
            hydra.create_client(&body).await?;
            incoming_uris
        }
        Some(existing) => {
            let merged = merge_preserving_existing(&existing.redirect_uris, &incoming_uris);
            if merged.len() > MAX_REDIRECT_URIS {
                return Err(AppOauthClientError::TooManyHosts {
                    requested: merged.len().div_ceil(CALLBACK_PATHS.len()),
                    cap: MAX_HOSTS,
                });
            }
            // Reconcile redirect_uris (diff-then-PUT) AND the scope allowlist.
            // Either a URI delta OR a scope delta triggers the PUT, so a deploy
            // that only adds a declared scope still re-registers the client.
            let uri_delta = reconcile_redirect_uris(&existing.redirect_uris, &merged);
            let scope_changed = existing.scope != scope_allowlist;
            if uri_delta.is_some() || scope_changed {
                let next_uris = uri_delta.unwrap_or(merged.clone());
                let body = build_client_body(
                    &client_id,
                    app_name,
                    scheme,
                    &apex_host,
                    next_uris,
                    &scope_allowlist,
                    first_party,
                );
                hydra.update_client(&body).await?;
            }
            merged
        }
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
/// against the live Hydra client, and `PUT`s only on a change. Also refreshes
/// the `control.oauth_clients.redirect_uris` mirror.
///
/// Returns `true` when a Hydra `PUT` was issued, `false` on a no-op.
///
/// # Errors
/// [`AppOauthClientError`] on Hydra/DB failure or an invalid host list.
pub async fn sync_app_redirect_uris(
    pg: &mut Client,
    hydra: &impl HydraClientAdmin,
    app_id: &Uuid,
    app_name: &str,
    scheme: &str,
    hosts: &[String],
) -> Result<bool> {
    let apex_host = hosts.first().ok_or(AppOauthClientError::NoHosts)?.clone();
    let client_id = client_id_for_app(app_id);
    let desired_uris = redirect_uris_for_hosts(scheme, hosts)?;

    let Some(existing) = hydra.get_client(&client_id).await? else {
        // Not provisioned yet — caller should ensure_app_client first. Treat as
        // a full provision so deploy is self-healing. No manifest is in hand on
        // this redirect-only path, so the client gets the baseline scope
        // allowlist; the next `ensure_app_client` (deploy) re-mirrors declared
        // scopes. The scope-defs registry is owned by `ensure_app_client`, so
        // this fallback writes the empty declared set.
        let body = build_client_body(
            &client_id,
            app_name,
            scheme,
            &apex_host,
            desired_uris.clone(),
            BASE_SCOPE,
            // Redirect-only self-heal is a creator-app path; never first-party.
            false,
        );
        hydra.create_client(&body).await?;
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
            // Redirect-only path: PRESERVE the live client's scope allowlist
            // (declared scopes are mirrored by `ensure_app_client`, not here).
            // Redirect-only sync is a creator-app path; never first-party.
            let body = build_client_body(
                &client_id,
                app_name,
                scheme,
                &apex_host,
                next,
                &existing.scope,
                false,
            );
            hydra.update_client(&body).await?;
            update_redirect_uri_mirror(pg, &client_id, &desired_uris).await?;
            Ok(true)
        }
    }
}

/// Delete the per-app client from Hydra. The DB rows
/// (`control.oauth_clients` + `control.app_oauth_clients`) cascade-delete with
/// the `control.apps` row, so this only removes the Hydra-side registration.
///
/// # Errors
/// [`AppOauthClientError::Hydra`] on admin failure.
pub async fn delete_app_client(hydra: &impl HydraClientAdmin, app_id: &Uuid) -> Result<()> {
    let client_id = client_id_for_app(app_id);
    hydra.delete_client(&client_id).await
}

/// Upsert the `control.oauth_clients` identity row, the
/// `control.app_oauth_clients` extension row, AND the `control.app_scope_defs`
/// declared-scope registry — all in ONE transaction (spec §8.1 / §5.1 O8
/// mirror). The `oauth_clients.scopes` mirror and the `app_scope_defs` rows are
/// derived from the SAME validated `scope_allowlist` / `declared_scopes`, so
/// the Hydra allowlist, the mirror, and the registry can never disagree —
/// which is what keeps the consent classifier sound (no deploy-race can let
/// Hydra accept a scope the registry hasn't learned).
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
        .map_err(|e| AppOauthClientError::Db(e.to_string()))?;

    // control.oauth_clients — the FK target + skip_consent reader.
    // `skip_consent` mirrors the live Hydra client: FALSE for every creator
    // (per-app end-user) client (spec §5.2), TRUE only for the platform's own
    // first-party console (`first_party`). Mirrored here in the SAME txn as the
    // client rows so the DB and Hydra never disagree. hydra_client_id ==
    // client_id == oac_<base62>. `scopes` mirrors the Hydra allowlist (baseline
    // + declared).
    tx.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, client_uri, logo_uri, redirect_uris, scopes, \
             skip_consent, created_by, hydra_client_id, refresh_allowed, \
             token_endpoint_auth_method, brokered, backchannel_logout_uri) \
         VALUES ($1, $2, NULL, NULL, $3, $4, $6, $5, $1, TRUE, \
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
        ],
    )
    .await
    .map_err(|e| AppOauthClientError::Db(e.to_string()))?;

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
    .map_err(|e| AppOauthClientError::Db(e.to_string()))?;

    // control.app_scope_defs — REPLACE the declared-scope registry for this app
    // wholesale (delete-then-insert): a deploy that drops a previously-declared
    // scope must remove its row, so the registry exactly tracks the current
    // manifest. Same transaction ⇒ atomic with the allowlist mirror above.
    tx.execute(
        "DELETE FROM zeroship.app_scope_defs WHERE app_id = $1",
        &[app_id],
    )
    .await
    .map_err(|e| AppOauthClientError::Db(e.to_string()))?;
    for scope in declared_scopes {
        tx.execute(
            "INSERT INTO zeroship.app_scope_defs (app_id, scope_id, label, description) \
             VALUES ($1, $2, $3, $4)",
            &[app_id, &scope.id, &scope.label, &scope.description],
        )
        .await
        .map_err(|e| AppOauthClientError::Db(e.to_string()))?;
    }

    tx.commit()
        .await
        .map_err(|e| AppOauthClientError::Db(e.to_string()))?;
    Ok(())
}

/// Refresh just the `control.oauth_clients.redirect_uris` mirror after a
/// redirect-sync PUT (the single source of truth for redirect_uris is the
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
    .map_err(|e| AppOauthClientError::Db(e.to_string()))?;
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

    // ── diff-then-PUT reconciliation (the load-bearing idempotency) ──────────

    #[test]
    fn reconcile_noop_when_identical() {
        let cur = vec!["a".to_string(), "b".to_string()];
        let des = vec!["a".to_string(), "b".to_string()];
        assert_eq!(reconcile_redirect_uris(&cur, &des), None);
    }

    #[test]
    fn reconcile_noop_when_only_order_differs() {
        // Hydra may return redirect_uris in any order; an order-only delta must
        // NOT trigger a PUT (no-op deploy ⇒ no-op PUT, spec §1.1).
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
        let out = reconcile_redirect_uris(&cur, &des).expect("a diff ⇒ PUT");
        assert_eq!(out, des, "PUT carries the full desired set");
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
        let out = reconcile_redirect_uris(&cur, &des).expect("a diff ⇒ PUT");
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
        // Apex-only deploy when Hydra already has apex+custom → no new URIs, so
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

    /// Regression for the M2 clobber gap: an apex-only `ensure_app_client`
    /// re-provision over a client that already carries a custom-domain URI
    /// (e.g. added by a future attach path) MUST preserve that URI — the merge
    /// makes the deploy additive, not last-writer-replaces.
    #[compio::test]
    async fn ensure_reprovision_preserves_existing_custom_domain_uri() {
        let hydra = MockHydra::default();
        let app = Uuid::new_v4();
        let client_id = client_id_for_app(&app);

        // Seed a client whose redirect_uris include a custom-domain pair the
        // apex deploy knows nothing about (simulating a prior attach).
        let seeded = redirect_uris_for_hosts(
            "https",
            &["apex.zeroship.ai".to_string(), "custom.example.com".to_string()],
        )
        .unwrap();
        let body = build_client_body(
            &client_id,
            "app",
            "https",
            "apex.zeroship.ai",
            seeded.clone(),
            BASE_SCOPE,
            false,
        );
        (&hydra).create_client(&body).await.unwrap();
        assert_eq!(*hydra.creates.borrow(), 1);

        // Apex-only re-provision via the REAL ensure path (Hydra-only slice).
        run_hydra_only_ensure(&hydra, &client_id, "https", &["apex.zeroship.ai".into()]).await;

        // The custom-domain URI must survive.
        let after = hydra.store.borrow().get(&client_id).unwrap().clone();
        assert!(
            after
                .redirect_uris
                .iter()
                .any(|u| u.contains("custom.example.com")),
            "apex-only deploy clobbered the custom-domain URI: {:?}",
            after.redirect_uris
        );
        // And it's a no-op (no PUT) since the merged set == existing set.
        assert_eq!(*hydra.updates.borrow(), 0, "no PUT — desired ⊆ existing");
    }

    // ── client body shape (spec §1.1) ───────────────────────────────────────

    #[test]
    fn client_body_is_public_pkce_with_per_app_bcl() {
        let uris = redirect_uris_for_hosts("https", &["apex.zeroship.ai".to_string()]).unwrap();
        let body = build_client_body(
            "oac_x",
            "my app",
            "https",
            "apex.zeroship.ai",
            uris,
            BASE_SCOPE,
            false,
        );
        assert_eq!(body.token_endpoint_auth_method, "none");
        assert!(body.client_secret.is_none());
        assert_eq!(body.subject_type, "public");
        assert_eq!(body.response_types, vec!["code".to_string()]);
        assert!(body.grant_types.contains(&"authorization_code".to_string()));
        assert!(body.grant_types.contains(&"refresh_token".to_string()));
        assert!(body.scope.contains("openid"));
        assert!(body.scope.contains("offline_access"));
        assert!(!body.skip_consent, "per-app clients never skip consent");
        // Deliberate divergence from clients_config (require_consent=!first_party):
        // per-app clients want first-grant-then-remembered consent for SSO, not a
        // forced re-consent on every login. skip_consent=false already fires the
        // first-grant prompt. Pinned here so a maintainer can't silently "align"
        // it to the first-party template.
        assert!(
            !body.require_consent,
            "per-app clients use remembered consent (first-grant prompt via skip_consent=false), \
             not forced re-consent — deliberate divergence from clients_config"
        );
        assert_eq!(
            body.backchannel_logout_uri.as_deref(),
            Some("https://apex.zeroship.ai/oidc/backchannel-logout"),
            "per-app BCL identity so logout maps to the right app's sessions"
        );
        assert_eq!(
            body.post_logout_redirect_uris,
            vec!["https://apex.zeroship.ai/".to_string()]
        );
    }

    /// The first-party-console exception (owner-approved, narrow): the platform's
    /// own console (`first_party = true`) is the ONE client that skips consent —
    /// a consent prompt for the platform's own surface is meaningless. Creator
    /// apps (`first_party = false`) NEVER skip consent (asserted above + here).
    #[test]
    fn first_party_flag_drives_skip_consent() {
        let uris = redirect_uris_for_hosts("https", &["console.zeroship.ai".to_string()]).unwrap();

        // Creator app: skip_consent stays FALSE (spec §5.2, unchanged).
        let creator = build_client_body(
            "oac_creator",
            "creator app",
            "https",
            "app.zeroship.ai",
            uris.clone(),
            BASE_SCOPE,
            false,
        );
        assert!(
            !creator.skip_consent,
            "creator apps NEVER skip consent (spec §5.2)"
        );

        // First-party console: skip_consent is TRUE (the narrow exception).
        let console = build_client_body(
            "oac_console",
            "zeroship console",
            "https",
            "console.zeroship.ai",
            uris,
            BASE_SCOPE,
            true,
        );
        assert!(
            console.skip_consent,
            "the first-party console skips consent (owner-approved exception)"
        );
    }

    #[test]
    fn sector_identifier_is_apex_origin() {
        assert_eq!(
            sector_identifier("https", "apex.zeroship.ai"),
            "https://apex.zeroship.ai"
        );
    }

    // ── faithful lifecycle exercise via a recording test double ─────────────

    use std::cell::RefCell;

    #[derive(Default)]
    struct MockHydra {
        store: RefCell<std::collections::HashMap<String, OAuth2Client>>,
        creates: RefCell<usize>,
        updates: RefCell<usize>,
        deletes: RefCell<usize>,
    }

    impl HydraClientAdmin for &MockHydra {
        async fn get_client(&self, client_id: &str) -> Result<Option<OAuth2Client>> {
            Ok(self.store.borrow().get(client_id).cloned())
        }
        async fn create_client(&self, client: &OAuth2Client) -> Result<OAuth2Client> {
            *self.creates.borrow_mut() += 1;
            self.store
                .borrow_mut()
                .insert(client.client_id.clone(), client.clone());
            Ok(client.clone())
        }
        async fn update_client(&self, client: &OAuth2Client) -> Result<OAuth2Client> {
            *self.updates.borrow_mut() += 1;
            self.store
                .borrow_mut()
                .insert(client.client_id.clone(), client.clone());
            Ok(client.clone())
        }
        async fn delete_client(&self, client_id: &str) -> Result<()> {
            *self.deletes.borrow_mut() += 1;
            self.store.borrow_mut().remove(client_id);
            Ok(())
        }
    }

    /// The reconciliation logic itself (no DB): create once, no-op on re-run
    /// with the same hosts, PUT on a host change, delete removes it. This is
    /// the REAL diff-then-PUT path — the mock only stands in for the Hydra
    /// transport, not the logic.
    #[compio::test]
    async fn lifecycle_create_then_noop_then_put_then_delete() {
        let hydra = MockHydra::default();
        let app = Uuid::new_v4();
        let client_id = client_id_for_app(&app);

        // First ensure → create.
        run_hydra_only_ensure(&hydra, &client_id, "https", &["apex.zeroship.ai".into()])
            .await;
        assert_eq!(*hydra.creates.borrow(), 1);
        assert_eq!(*hydra.updates.borrow(), 0);

        // Re-ensure, same host → no Hydra write (idempotent).
        run_hydra_only_ensure(&hydra, &client_id, "https", &["apex.zeroship.ai".into()])
            .await;
        assert_eq!(*hydra.creates.borrow(), 1, "no second create");
        assert_eq!(*hydra.updates.borrow(), 0, "no PUT on unchanged hosts");

        // Add a custom domain → exactly one PUT.
        run_hydra_only_ensure(
            &hydra,
            &client_id,
            "https",
            &["apex.zeroship.ai".into(), "custom.example.com".into()],
        )
        .await;
        assert_eq!(*hydra.updates.borrow(), 1, "PUT on added host");
        assert_eq!(
            hydra.store.borrow().get(&client_id).unwrap().redirect_uris.len(),
            4
        );

        // Delete. The trait is impl'd for `&MockHydra`, so the receiver is a
        // reference to the mock.
        delete_app_client(&&hydra, &app).await.unwrap();
        assert_eq!(*hydra.deletes.borrow(), 1);
        assert!(hydra.store.borrow().get(&client_id).is_none());
    }

    /// Hydra-only slice of `ensure_app_client` (skips the DB upsert) so the
    /// reconciliation path is testable without a live Postgres. Mirrors the REAL
    /// merge-then-reconcile logic in `ensure_app_client` (non-destructive union
    /// on reconcile) — the only thing the mock stands in for is the Hydra
    /// transport, not the logic. The DB write is covered by the gated
    /// integration test (`app_oauth_client_test.rs`).
    async fn run_hydra_only_ensure(
        hydra: &MockHydra,
        client_id: &str,
        scheme: &str,
        hosts: &[String],
    ) {
        run_hydra_only_ensure_scopes(hydra, client_id, scheme, hosts, &[]).await;
    }

    /// Like [`run_hydra_only_ensure`] but threads declared scopes — mirrors the
    /// REAL `ensure_app_client` scope+URI diff (PUT on EITHER a URI or scope
    /// delta), so the Hydra-only tests faithfully exercise the scope-allowlist
    /// reconciliation, not a stub of it.
    async fn run_hydra_only_ensure_scopes(
        hydra: &MockHydra,
        client_id: &str,
        scheme: &str,
        hosts: &[String],
        declared_scopes: &[ScopeDef],
    ) {
        // The trait is impl'd for `&MockHydra`; method syntax on `hydra`
        // (`&MockHydra`) resolves to that impl.
        validate_app_scopes(declared_scopes).unwrap();
        let scope_allowlist = build_scope_allowlist(declared_scopes);
        let apex = hosts.first().unwrap().clone();
        let incoming = redirect_uris_for_hosts(scheme, hosts).unwrap();
        match hydra.get_client(client_id).await.unwrap() {
            None => {
                let body = build_client_body(
                    client_id,
                    "app",
                    scheme,
                    &apex,
                    incoming.clone(),
                    &scope_allowlist,
                    false,
                );
                hydra.create_client(&body).await.unwrap();
            }
            Some(existing) => {
                let merged = merge_preserving_existing(&existing.redirect_uris, &incoming);
                let uri_delta = reconcile_redirect_uris(&existing.redirect_uris, &merged);
                let scope_changed = existing.scope != scope_allowlist;
                if uri_delta.is_some() || scope_changed {
                    let next_uris = uri_delta.unwrap_or(merged.clone());
                    let body = build_client_body(
                        client_id,
                        "app",
                        scheme,
                        &apex,
                        next_uris,
                        &scope_allowlist,
                        false,
                    );
                    hydra.update_client(&body).await.unwrap();
                }
            }
        }
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

    /// A deploy that adds a declared scope (no URI change) STILL issues a Hydra
    /// PUT, because the scope allowlist changed — the O8 mirror invariant.
    #[compio::test]
    async fn scope_only_change_triggers_put() {
        let hydra = MockHydra::default();
        let app = Uuid::new_v4();
        let client_id = client_id_for_app(&app);

        // Create with no declared scopes (baseline allowlist).
        run_hydra_only_ensure_scopes(&hydra, &client_id, "https", &["apex.zeroship.ai".into()], &[])
            .await;
        assert_eq!(*hydra.creates.borrow(), 1);
        assert_eq!(*hydra.updates.borrow(), 0);
        assert_eq!(hydra.store.borrow().get(&client_id).unwrap().scope, BASE_SCOPE);

        // Re-ensure SAME hosts but now declaring read:billing → exactly one PUT,
        // even though no redirect_uri changed.
        let declared = vec![ScopeDef {
            id: "read:billing".into(),
            label: "View billing".into(),
            description: None,
        }];
        run_hydra_only_ensure_scopes(
            &hydra,
            &client_id,
            "https",
            &["apex.zeroship.ai".into()],
            &declared,
        )
        .await;
        assert_eq!(*hydra.updates.borrow(), 1, "scope-only delta ⇒ one PUT");
        assert_eq!(
            hydra.store.borrow().get(&client_id).unwrap().scope,
            "openid offline_access profile email read:billing"
        );

        // Re-ensure with the SAME declared scope → no further PUT (idempotent).
        run_hydra_only_ensure_scopes(
            &hydra,
            &client_id,
            "https",
            &["apex.zeroship.ai".into()],
            &declared,
        )
        .await;
        assert_eq!(*hydra.updates.borrow(), 1, "no PUT on unchanged scopes+hosts");
    }
}
