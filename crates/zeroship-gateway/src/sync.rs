use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::TcpStream;

use zeroship_core::app_id::AppId;
use zeroship_core::readiness::SyncFreshness;
use zeroship_core::types::{GatewaySnapshot, RouteEntry, RouteMap};
use zeroship_core::user_id::UserId;

use zeroship_core::types::SpendState;

use zeroship_bundle::compiled::CompiledManifest;
use crate::enforce::{ConcurrencyRegistry, RateLimitRegistry};
use crate::GateState;

const CONTROL_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl std::fmt::Debug for RouteCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteCache").finish_non_exhaustive()
    }
}

/// One route plus its pre-compiled manifest. The compiled form is built
/// once on update and shared via Arc so dispatch is allocation-free.
#[allow(missing_debug_implementations)]
pub struct CompiledRoute {
    pub entry: RouteEntry,
    pub manifest: Arc<CompiledManifest>,
}

#[derive(Debug, Default)]
struct AuthenticationSnapshot {
    denied_principals: HashSet<String>,
    denied_subjects_by_user: HashMap<UserId, HashSet<String>>,
    family_revocations: HashMap<(String, String), i64>,
}

pub struct RouteCache {
    routes: RwLock<HashMap<AppId, Arc<CompiledRoute>>>,
    name_index: RwLock<HashMap<String, AppId>>,
    authentication: RwLock<AuthenticationSnapshot>,
    /// When the control plane last served a route table this cache accepted.
    /// `/readyz` reads it instead of issuing its own control-plane request.
    sync_freshness: SyncFreshness,
}

impl Default for RouteCache {
    fn default() -> Self {
        Self::new()
    }
}

impl RouteCache {
    #[must_use]
    pub fn new() -> Self {
        Self {
            routes: RwLock::new(HashMap::new()),
            name_index: RwLock::new(HashMap::new()),
            authentication: RwLock::new(AuthenticationSnapshot::default()),
            sync_freshness: SyncFreshness::new(),
        }
    }

    /// Freshness stamp of the control-plane route pull, for `/readyz`.
    #[must_use]
    pub fn sync_freshness(&self) -> &SyncFreshness {
        &self.sync_freshness
    }

    /// Apply the complete control-plane snapshot and derive the offline
    /// principal denylist for both global and per-app pairwise subjects.
    pub fn update_snapshot(
        &self,
        snapshot: GatewaySnapshot,
        rate: &RateLimitRegistry,
        concurrency: &ConcurrencyRegistry,
    ) {
        let GatewaySnapshot {
            routes,
            principal_lifecycle,
            family_revocations,
        } = snapshot;
        let mut authentication = self.authentication.write().unwrap();
        let prior_by_user = &authentication.denied_subjects_by_user;
        let mut denied_by_user = HashMap::new();
        for lifecycle in principal_lifecycle
            .iter()
            .filter(|lifecycle| lifecycle.blocks_authentication())
        {
            let global = lifecycle.user_id.as_str().to_string();
            let mut subjects = prior_by_user
                .get(&lifecycle.user_id)
                .cloned()
                .unwrap_or_default();
            subjects.insert(global);
            subjects.extend(lifecycle.pairwise_subjects.iter().cloned());
            denied_by_user.insert(lifecycle.user_id.clone(), subjects);
        }

        let denied = denied_by_user
            .values()
            .flat_map(|subjects| subjects.iter().cloned())
            .collect();
        let family_revocations = family_revocations
            .into_iter()
            .map(|revocation| {
                (
                    (revocation.client_id, revocation.subject),
                    revocation.revoked_after,
                )
            })
            .collect();
        *authentication = AuthenticationSnapshot {
            denied_principals: denied,
            denied_subjects_by_user: denied_by_user,
            family_revocations,
        };
        drop(authentication);
        self.update(routes, rate, concurrency);
        self.sync_freshness.mark_success();
    }

    /// Decide a verified credential from the locally pushed lifecycle state.
    /// Missing or stale state rejects instead of silently authenticating.
    #[must_use]
    pub fn principal_authentication_allowed(
        &self,
        subject: &str,
        freshness_budget: std::time::Duration,
    ) -> bool {
        if !self.sync_freshness.is_fresh(freshness_budget) {
            return false;
        }
        !self
            .authentication
            .read()
            .unwrap()
            .denied_principals
            .contains(subject)
    }

    /// Decide a verified credential entirely from the pushed lifecycle and
    /// durable family-cutoff snapshot.
    #[must_use]
    pub fn credential_authentication_allowed(
        &self,
        client_id: &str,
        subject: &str,
        issued_at: i64,
        freshness_budget: std::time::Duration,
    ) -> bool {
        if !self.sync_freshness.is_fresh(freshness_budget) {
            return false;
        }
        let authentication = self.authentication.read().unwrap();
        if authentication.denied_principals.contains(subject) {
            return false;
        }
        authentication
            .family_revocations
            .get(&(client_id.to_string(), subject.to_string()))
            .is_none_or(|revoked_after| *revoked_after <= issued_at)
    }

    /// Replace the route table, recompiling each manifest.
    ///
    /// Each `CompiledRoute` carries the pulled spend state for enforcement:
    /// `entry.spend_state`. For every app whose Degrade flag flips between the
    /// previous and the new table, this calls `set_degraded`/`clear_degraded`
    /// on the rate + concurrency registries so a Degraded app is throttled
    /// (and recovers instantly) WITHOUT rebuilding any bucket. Block/Warn need
    /// no registry mutation — they are gated per-request in `check_spend`.
    pub fn update(
        &self,
        new_routes: RouteMap,
        rate: &RateLimitRegistry,
        concurrency: &ConcurrencyRegistry,
    ) {
        // Snapshot the Degrade flag of the OUTGOING table so we only flip the
        // registries on an actual change (idempotent set is cheap, but this
        // keeps the intent explicit and the logs quiet).
        let prev_degraded: HashMap<AppId, bool> = {
            let r = self.routes.read().unwrap();
            r.iter()
                .map(|(id, route)| {
                    (id.clone(), route.entry.spend_state == SpendState::Degrade)
                })
                .collect()
        };

        let mut name_idx = HashMap::new();
        let mut compiled: HashMap<AppId, Arc<CompiledRoute>> = HashMap::new();
        for (id, entry) in new_routes {
            // Validate before the app enters the table, and drop it if the
            // manifest does not validate. The manifest IS the authorization
            // policy, so a manifest we cannot interpret leaves us with no
            // policy to enforce; the only safe reading of "no policy" is to
            // stop serving the app, which makes dispatch answer 404. Falling
            // back to a permissive default here would serve every route the
            // manifest was meant to gate to anonymous callers.
            //
            // The deploy handler validates on ingest, so this does not fire on
            // a normal deploy. It needs post-ingest divergence: a validation
            // rule that tightened under an already-stored manifest, or an
            // out-of-band edit of the stored row. Both are operator-visible
            // through this log, and both are cases where refusing to serve is
            // the outcome the operator would choose.
            if let Err(e) = entry.manifest.validate() {
                tracing::error!(
                    app_id = id.as_str(),
                    app_name = %entry.name,
                    error = %e,
                    "gateway-sync: manifest validation failed, removing the app from the \
                     route table; it will 404 until a valid manifest is deployed"
                );
                continue;
            }

            name_idx.insert(entry.name.clone(), id.clone());
            let now_degraded = entry.spend_state == SpendState::Degrade;
            if prev_degraded.get(&id).copied().unwrap_or(false) != now_degraded {
                rate.set_degraded(&id, now_degraded);
                concurrency.set_degraded(&id, now_degraded);
            }
            let compiled_manifest = Arc::new(CompiledManifest::compile(&entry.manifest));
            compiled.insert(
                id,
                Arc::new(CompiledRoute {
                    entry,
                    manifest: compiled_manifest,
                }),
            );
        }
        *self.routes.write().unwrap() = compiled;
        *self.name_index.write().unwrap() = name_idx;
    }

    pub fn lookup_by_name(&self, name: &str) -> Option<(AppId, Arc<CompiledRoute>)> {
        let name_idx = self.name_index.read().unwrap();
        let app_id = name_idx.get(name)?;
        let routes = self.routes.read().unwrap();
        routes.get(app_id).map(|r| (app_id.clone(), r.clone()))
    }

    /// Resolve a route by app id.
    ///
    /// The key is an [`AppId`], carried end to end from the control plane's
    /// `RouteMap` (`HashMap<AppId, RouteEntry>`) through this table, so a
    /// lookup that misses is an unknown app, not a rendering disagreement.
    pub fn lookup_by_app_id(&self, app_id: &AppId) -> Option<Arc<CompiledRoute>> {
        let routes = self.routes.read().unwrap();
        routes.get(app_id).cloned()
    }

    /// Resolve a route by its per-app OAuth `client_id` (= `oac_<base62>`).
    ///
    /// For per-app back-channel logout, the inbound
    /// `logout_token.aud` carries the per-app `client_id`; the BCL handler uses
    /// this to find which app the token belongs to (and thus its subdomain
    /// `name`) so it revokes only that app's sessions. Returns `None` when no
    /// provisioned route binds that `client_id` (un-provisioned apps carry
    /// `oauth_client_id == None` and never match).
    ///
    /// O(N) over the route table — BCL is a low-frequency webhook surface, so a
    /// linear scan is cheaper than maintaining a third index. The table is the
    /// same handful of apps `lookup_by_name` already serves.
    pub fn lookup_by_oauth_client_id(
        &self,
        client_id: &str,
    ) -> Option<(AppId, Arc<CompiledRoute>)> {
        let routes = self.routes.read().unwrap();
        routes.iter().find_map(|(id, route)| {
            if route.entry.oauth_client_id.as_deref() == Some(client_id) {
                Some((id.clone(), route.clone()))
            } else {
                None
            }
        })
    }
}

pub fn start_sync(state: Arc<GateState>) {
    compio::runtime::spawn(async move {
        let interval = std::time::Duration::from_secs(state.config.poll_interval_secs);
        loop {
            compio::time::sleep(interval).await;
            if let Err(e) = sync_once(&state).await {
                tracing::error!(error = %e, "gateway-sync: poll cycle failed");
            }
        }
    })
    .detach();
}

async fn sync_once(state: &GateState) -> Result<(), String> {
    let url = format!("{}/internal/routes", state.config.control_url);
    let response = http_get(&url, &state.config.control_key).await?;
    let snapshot: GatewaySnapshot =
        serde_json::from_str(&response).map_err(|e| format!("parse gateway snapshot: {e}"))?;
    state.routes.update_snapshot(
        snapshot,
        &state.rate_limiters,
        &state.concurrency,
    );
    Ok(())
}

async fn http_get(url: &str, auth_key: &str) -> Result<String, String> {
    compio::time::timeout(CONTROL_REQUEST_TIMEOUT, http_get_inner(url, auth_key))
        .await
        .map_err(|_| control_timeout_error())?
}

/// Reject a `--control` URL this transport cannot actually honour.
///
/// `http_get_inner` opens a plain `TcpStream` and writes the control key as a
/// `Authorization: Bearer` header. There is no TLS on this path at all, so an
/// `https://` control URL does not get encrypted - it gets silently downgraded,
/// and the key goes out in cleartext. Worse, `Url::port()` returns `None` for a
/// scheme-default port, so `https://control` lands on port 80 rather than 443.
///
/// Until this transport speaks TLS, the only honest answer is to refuse the
/// scheme rather than pretend to serve it. `--blob-store` in the same binary
/// already validates and exits; this brings `--control` up to that bar.
pub fn validate_control_url(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("not a URL: {e}"))?;
    match parsed.scheme() {
        "http" => {}
        other => {
            return Err(format!(
                "unsupported --control-url scheme {other:?}: the control transport \
                 is plaintext HTTP and would send the control key in the clear. \
                 Use http:// (and keep the hop on a trusted network), or \
                 terminate TLS in front of the gateway."
            ));
        }
    }
    if parsed.host_str().is_none() {
        return Err("no host in --control-url URL".to_string());
    }
    Ok(())
}

fn control_timeout_error() -> String {
    format!(
        "control request timed out after {}s",
        CONTROL_REQUEST_TIMEOUT.as_secs()
    )
}

async fn http_get_inner(url: &str, auth_key: &str) -> Result<String, String> {
    let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
    let host = parsed.host_str().ok_or("no host")?;
    let port = parsed.port().unwrap_or(80);
    let path = parsed.path();

    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr).await.map_err(|e| e.to_string())?;

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {auth_key}\r\nConnection: close\r\n\r\n"
    );

    let BufResult(r, _) = stream.write_all(request.into_bytes()).await;
    r.map_err(|e| e.to_string())?;

    let mut response = Vec::new();
    loop {
        let buf = vec![0u8; 8192];
        let BufResult(r, returned) = stream.read(buf).await;
        let n = r.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        response.extend_from_slice(&returned[..n]);
    }

    let header_end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no header end")?;

    let header = std::str::from_utf8(&response[..header_end]).map_err(|e| e.to_string())?;
    if !header.starts_with("HTTP/1.1 200") && !header.starts_with("HTTP/1.0 200") {
        return Err(format!(
            "HTTP error: {}",
            header.lines().next().unwrap_or("?")
        ));
    }

    String::from_utf8(response[header_end + 4..].to_vec()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_bundle::Manifest;
    use zeroship_core::types::{
        GatewayFamilyRevocation, GatewayPrincipalLifecycle, GatewaySnapshot,
    };

    #[test]
    fn control_timeout_defaults_to_five_seconds() {
        assert_eq!(CONTROL_REQUEST_TIMEOUT, std::time::Duration::from_secs(5));
        assert_eq!(control_timeout_error(), "control request timed out after 5s");
    }

    fn route_entry(name: &str, oauth_client_id: Option<&str>, sector: Option<&str>) -> RouteEntry {
        RouteEntry {
            name: name.to_string(),
            plan_id: "free".to_string(),
            deploy_hash: None,
            manifest: Manifest::passthrough(),
            oauth_client_id: oauth_client_id.map(str::to_string),
            sector_identifier: sector.map(str::to_string),
            spend_state: zeroship_core::types::SpendState::Allow,
            account_state: zeroship_core::types::AccountState::Active,
        }
    }

    #[test]
    fn lifecycle_snapshot_blocks_every_non_authenticating_state_offline() {
        let salt = zeroship_core::auth::derive_pairwise_salt(b"lifecycle-snapshot-test-salt");
        let sector = "https://app.zeroship.test";
        let app_id = AppId::mint();
        let mut routes = RouteMap::new();
        routes.insert(
            app_id,
            route_entry("app.zeroship.test", Some("oac_test"), Some(sector)),
        );

        let disabled = UserId::mint();
        let anonymized = UserId::mint();
        let requested = UserId::mint();
        let scheduled = UserId::mint();
        let lifecycle = |user_id: UserId,
                         lifecycle: fn(UserId, Vec<String>) -> GatewayPrincipalLifecycle| {
            let pairwise = zeroship_core::auth::derive_pairwise(
                &salt,
                &user_id,
                sector,
            );
            lifecycle(user_id, vec![pairwise])
        };
        let snapshot = GatewaySnapshot {
            routes,
            principal_lifecycle: vec![
                lifecycle(disabled.clone(), GatewayPrincipalLifecycle::disabled),
                lifecycle(anonymized.clone(), GatewayPrincipalLifecycle::anonymized),
                lifecycle(requested.clone(), GatewayPrincipalLifecycle::deletion_requested),
                lifecycle(scheduled.clone(), GatewayPrincipalLifecycle::deletion_scheduled),
            ],
            family_revocations: Vec::new(),
        };

        let cache = RouteCache::new();
        cache.update_snapshot(
            snapshot,
            &RateLimitRegistry::new(1000, 2000),
            &ConcurrencyRegistry::new(100),
        );
        let budget = std::time::Duration::from_secs(60);
        for user_id in [disabled, anonymized, requested, scheduled] {
            let global = user_id.as_str();
            let pairwise = zeroship_core::auth::derive_pairwise(&salt, &user_id, sector);
            assert!(!cache.principal_authentication_allowed(global, budget));
            assert!(!cache.principal_authentication_allowed(&pairwise, budget));
        }
        assert!(cache.principal_authentication_allowed(UserId::mint().as_str(), budget));
    }

    #[test]
    fn lifecycle_authentication_fails_closed_without_a_fresh_snapshot() {
        let cache = RouteCache::new();
        assert!(!cache.principal_authentication_allowed(
            "pws_otherwise-valid",
            std::time::Duration::from_secs(60),
        ));

        cache.update_snapshot(
            GatewaySnapshot {
                routes: RouteMap::new(),
                principal_lifecycle: Vec::new(),
                family_revocations: Vec::new(),
            },
            &RateLimitRegistry::new(1000, 2000),
            &ConcurrencyRegistry::new(100),
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(!cache.principal_authentication_allowed(
            "pws_otherwise-valid",
            std::time::Duration::from_millis(1),
        ));
    }

    #[test]
    fn route_replacement_retains_persisted_pairwise_denials() {
        let salt = zeroship_core::auth::derive_pairwise_salt(b"route-replacement-denial-salt");
        let user_id = UserId::mint();
        let sector = "https://retired-route.zeroship.test";
        let pairwise = zeroship_core::auth::derive_pairwise(
            &salt,
            &user_id,
            sector,
        );
        let app_id = AppId::mint();
        let mut routes = RouteMap::new();
        routes.insert(
            app_id,
            route_entry(
                "retired-route.zeroship.test",
                Some("oac_retired_route"),
                Some(sector),
            ),
        );
        let cache = RouteCache::new();
        let rate = RateLimitRegistry::new(1000, 2000);
        let concurrency = ConcurrencyRegistry::new(100);
        let lifecycle_with_mapping = || {
            vec![GatewayPrincipalLifecycle {
                user_id: user_id.clone(),
                disabled: true,
                anonymized: false,
                deletion_requested: false,
                deletion_scheduled: false,
                pairwise_subjects: vec![pairwise.clone()],
            }]
        };
        let lifecycle_without_mapping = || {
            vec![GatewayPrincipalLifecycle::disabled(user_id.clone(), Vec::new())]
        };
        let budget = std::time::Duration::from_secs(60);

        cache.update_snapshot(
            GatewaySnapshot {
                routes,
                principal_lifecycle: lifecycle_with_mapping(),
                family_revocations: Vec::new(),
            },
            &rate,
            &concurrency,
        );
        assert!(!cache.principal_authentication_allowed(&pairwise, budget));

        cache.update_snapshot(
            GatewaySnapshot {
                routes: RouteMap::new(),
                principal_lifecycle: lifecycle_without_mapping(),
                family_revocations: Vec::new(),
            },
            &rate,
            &concurrency,
        );
        assert!(
            !cache.principal_authentication_allowed(&pairwise, budget),
            "an in-flight request using the retired route must retain its denial"
        );

        for _ in 0..3 {
            cache.update_snapshot(
                GatewaySnapshot {
                    routes: RouteMap::new(),
                    principal_lifecycle: lifecycle_without_mapping(),
                    family_revocations: Vec::new(),
                },
                &rate,
                &concurrency,
            );
            assert!(
                !cache.principal_authentication_allowed(&pairwise, budget),
                "a persisted pairwise denial must not age out with its route"
            );
        }
    }

    #[test]
    fn lifecycle_is_required_on_the_route_pull_wire() {
        let old_route_only_wire = serde_json::json!({
            "routes": {},
            "principal_lifecycle": [],
        });
        assert!(serde_json::from_value::<GatewaySnapshot>(old_route_only_wire).is_err());
    }

    #[test]
    fn durable_family_cutoff_survives_lifecycle_cancellation_offline() {
        let cache = RouteCache::new();
        let rate = RateLimitRegistry::new(1000, 2000);
        let concurrency = ConcurrencyRegistry::new(100);
        let user_id = UserId::mint();
        let subject = "pws_cancelled_deletion";
        let client_id = "oac_cancelled_deletion";
        let revocation = GatewayFamilyRevocation {
            client_id: client_id.to_string(),
            subject: subject.to_string(),
            revoked_after: 1_700_000_100,
        };

        cache.update_snapshot(
            GatewaySnapshot {
                routes: RouteMap::new(),
                principal_lifecycle: vec![GatewayPrincipalLifecycle::deletion_requested(
                    user_id,
                    vec![subject.to_string()],
                )],
                family_revocations: vec![revocation.clone()],
            },
            &rate,
            &concurrency,
        );
        cache.update_snapshot(
            GatewaySnapshot {
                routes: RouteMap::new(),
                principal_lifecycle: Vec::new(),
                family_revocations: vec![revocation],
            },
            &rate,
            &concurrency,
        );

        let budget = std::time::Duration::from_secs(60);
        assert!(!cache.credential_authentication_allowed(
            client_id,
            subject,
            1_700_000_000,
            budget,
        ));
        assert!(cache.credential_authentication_allowed(
            client_id,
            subject,
            1_700_000_200,
            budget,
        ));
    }

    #[test]
    fn route_sync_surfaces_oauth_fields_on_compiled_route() {
        // The route-sync compile path (`RouteCache::update`) must thread
        // the §1.5 OAuth identity fields from the source `RouteEntry`
        // through to the `CompiledRoute` that `lookup_by_name` returns,
        // so the gateway resolves a per-app `oauth_client_id`/sector
        // from the request `Host` without a second lookup.
        let provisioned_id = AppId::mint();
        let unprovisioned_id = AppId::mint();

        let mut routes: RouteMap = HashMap::new();
        routes.insert(
            provisioned_id.clone(),
            route_entry(
                "provisioned.zeroship.ai",
                Some("oac_provisioned"),
                Some("https://provisioned.zeroship.ai"),
            ),
        );
        routes.insert(
            unprovisioned_id,
            route_entry("unprovisioned.zeroship.ai", None, None),
        );

        let cache = RouteCache::new();
        cache.update(
            routes,
            &RateLimitRegistry::new(1000, 2000),
            &ConcurrencyRegistry::new(100),
        );

        // Provisioned host → Some(oauth_client_id) + Some(sector).
        let (id, compiled) = cache
            .lookup_by_name("provisioned.zeroship.ai")
            .expect("provisioned host resolves");
        assert_eq!(id, provisioned_id);
        assert_eq!(
            compiled.entry.oauth_client_id.as_deref(),
            Some("oac_provisioned")
        );
        assert_eq!(
            compiled.entry.sector_identifier.as_deref(),
            Some("https://provisioned.zeroship.ai")
        );

        // Un-provisioned host → None on both (hard-fail-closed source).
        let (_, compiled) = cache
            .lookup_by_name("unprovisioned.zeroship.ai")
            .expect("unprovisioned host resolves");
        assert_eq!(compiled.entry.oauth_client_id, None);
        assert_eq!(compiled.entry.sector_identifier, None);
    }

    #[test]
    fn manifest_that_fails_validation_makes_the_app_unresolvable() {
        // A stored manifest that stops validating must take the app OUT of the
        // route table, so dispatch answers 404, rather than compiling a
        // passthrough manifest that serves every route to anonymous callers.
        // The deploy handler validates on ingest, so reaching this needs
        // post-ingest divergence: a validation rule that tightened under an
        // already-stored manifest, or an out-of-band edit of the stored row.
        let good_id = AppId::mint();
        let broken_id = AppId::mint();

        let mut broken = route_entry("broken.zeroship.ai", None, None);
        broken.manifest.version = 2;
        assert!(
            broken.manifest.validate().is_err(),
            "fixture must actually fail validation, otherwise this test proves nothing"
        );

        let mut routes: RouteMap = HashMap::new();
        routes.insert(good_id, route_entry("good.zeroship.ai", None, None));
        routes.insert(broken_id, broken);

        let cache = RouteCache::new();
        cache.update(
            routes,
            &RateLimitRegistry::new(1000, 2000),
            &ConcurrencyRegistry::new(100),
        );

        assert!(
            cache.lookup_by_name("broken.zeroship.ai").is_none(),
            "an app whose manifest fails validation must not resolve; serving it \
             as anonymous-public exposes every route the manifest was meant to gate"
        );

        // A sibling app in the same sync batch is unaffected: one bad manifest
        // must not take down the rest of the table.
        assert!(
            cache.lookup_by_name("good.zeroship.ai").is_some(),
            "a valid app in the same batch must still resolve"
        );
    }

    #[test]
    fn required_scopes_plumb_from_manifest_json_through_to_compiled_route() {
        // Faithful end-to-end of the wire path control `get_routes` uses: the
        // per-route `required_scopes` ride
        // INSIDE the app manifest (`ResourceEntry.required_scopes`), which
        // is exactly the `manifest_json` column control serialises and the
        // gateway parses. We serialise a manifest carrying a scoped resource
        // to JSON (the column shape), parse it back as `registry::get_routes`
        // does, hang it on a `RouteEntry`, push it through `RouteCache::update`
        // (the compile step), and assert the compiled `EffectivePolicy`
        // carries the scopes the gateway auth gate enforces.
        use std::collections::HashMap as Map;
        use zeroship_bundle::{
            AuthConfig, ProcedureKind, RequiredPrincipal, ResourceEntry, ScopeDef,
        };

        let mut resources: Map<String, ResourceEntry> = Map::new();
        resources.insert(
            "rpc:billing.read".to_string(),
            ResourceEntry {
                kind: Some(ProcedureKind::Query),
                auth: Some(RequiredPrincipal::User),
                required_scopes: vec!["read:billing".to_string()],
                ..Default::default()
            },
        );
        let manifest = Manifest {
            version: 1,
            resources,
            // The route's `required_scopes` must reference a DECLARED scope
            // at validation time; declare it so the fixture is a
            // genuinely valid manifest, exactly what control would deploy.
            auth: AuthConfig {
                scopes: vec![ScopeDef {
                    id: "read:billing".to_string(),
                    label: "Read billing".to_string(),
                    description: None,
                }],
            },
            ..Manifest::default()
        };

        // Serialise → parse, exactly as the manifest_json column round-trips
        // through control `get_routes`.
        let manifest_json = serde_json::to_string(&manifest).expect("serialise manifest");
        let parsed: Manifest = serde_json::from_str(&manifest_json).expect("parse manifest_json");
        parsed.validate().expect("manifest valid");

        let app_id = AppId::mint();
        let mut routes: RouteMap = HashMap::new();
        routes.insert(
            app_id,
            RouteEntry {
                name: "billing-app.zeroship.localhost".to_string(),
                plan_id: "free".to_string(),
                deploy_hash: None,
                manifest: parsed,
                oauth_client_id: Some("oac_billing".to_string()),
                sector_identifier: Some("https://billing-app.zeroship.localhost".to_string()),
                spend_state: zeroship_core::types::SpendState::Allow,
                account_state: zeroship_core::types::AccountState::Active,
            },
        );

        let cache = RouteCache::new();
        cache.update(
            routes,
            &RateLimitRegistry::new(1000, 2000),
            &ConcurrencyRegistry::new(100),
        );

        let (_, compiled) = cache
            .lookup_by_name("billing-app.zeroship.localhost")
            .expect("route resolves");
        let policy = compiled
            .manifest
            .lookup_resource("/__zeroship/v1/billing.read")
            .expect("scoped resource compiles");
        assert_eq!(
            policy.required_scopes,
            vec!["read:billing".to_string()],
            "required_scopes must plumb control→CompiledRoute via the manifest"
        );
    }

    #[test]
    fn lookup_by_oauth_client_id_resolves_provisioned_app_and_skips_unprovisioned() {
        // For per-app BCL disambiguation, the handler resolves
        // the app from the `logout_token.aud` (= per-app client_id). A
        // provisioned client resolves to its app's route (and subdomain name);
        // an un-provisioned app (oauth_client_id == None) never matches; an
        // unknown client_id resolves to nothing.
        let provisioned_id = AppId::mint();
        let mut routes: RouteMap = HashMap::new();
        routes.insert(
            provisioned_id.clone(),
            route_entry(
                "myapp.zeroship.localhost",
                Some("oac_myapp"),
                Some("https://myapp.zeroship.localhost"),
            ),
        );
        routes.insert(
            AppId::mint(),
            route_entry("other.zeroship.localhost", None, None),
        );

        let cache = RouteCache::new();
        cache.update(
            routes,
            &RateLimitRegistry::new(1000, 2000),
            &ConcurrencyRegistry::new(100),
        );

        let (id, compiled) = cache
            .lookup_by_oauth_client_id("oac_myapp")
            .expect("per-app client resolves to its route");
        assert_eq!(id, provisioned_id);
        assert_eq!(compiled.entry.name, "myapp.zeroship.localhost");

        // Un-provisioned app's None oauth_client_id is never matched by a real
        // client_id, and an unknown client_id resolves to nothing.
        assert!(cache.lookup_by_oauth_client_id("oac_unknown").is_none());
        assert!(cache.lookup_by_oauth_client_id("").is_none());
    }

    #[test]
    fn control_url_rejects_https_because_this_transport_has_no_tls() {
        // `http_get_inner` opens a plain TcpStream and writes
        // `Authorization: Bearer <control_key>`. Accepting an https:// URL does
        // not encrypt that - it silently downgrades and ships the key in
        // cleartext. Refusing is the only honest answer until TLS exists here.
        let err = validate_control_url("https://control.internal:9090")
            .expect_err("https must be refused, not silently downgraded");
        assert!(
            err.contains("https"),
            "the error must name the scheme it refused, got {err:?}"
        );
    }

    #[test]
    fn control_url_rejects_https_on_the_default_port_too() {
        // The nastiest shape: `Url::port()` returns None for a scheme-default
        // port, so this would connect to 80 rather than 443 - not merely
        // unencrypted but a different port than the operator wrote.
        assert!(validate_control_url("https://control.internal").is_err());
    }

    #[test]
    fn control_url_accepts_http() {
        // POSITIVE CONTROL. The validator must reject the scheme it cannot
        // serve and nothing else - the compose and local-dev defaults are
        // http:// and must keep working.
        validate_control_url("http://localhost:9090").expect("http is the supported scheme");
        validate_control_url("http://control:9090/").expect("trailing slash is fine");
    }

    #[test]
    fn control_url_rejects_a_value_that_is_not_a_url() {
        assert!(validate_control_url("localhost:9090").is_err());
        assert!(validate_control_url("").is_err());
    }
}
