use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::TcpStream;
use uuid::Uuid;

use zeroship_bundle::Manifest;
use zeroship_core::types::{RouteEntry, RouteMap};

use crate::compiled::CompiledManifest;
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

pub struct RouteCache {
    routes: RwLock<HashMap<Uuid, Arc<CompiledRoute>>>,
    name_index: RwLock<HashMap<String, Uuid>>,
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
        }
    }

    pub fn update(&self, new_routes: RouteMap) {
        let mut name_idx = HashMap::new();
        let mut compiled: HashMap<Uuid, Arc<CompiledRoute>> = HashMap::new();
        for (id, entry) in new_routes {
            name_idx.insert(entry.name.clone(), id);
            // Validate first; on Err, fall back to passthrough for parity
            // with the rest of the platform's "always have a manifest"
            // invariant. Log so deploys with bad manifests are visible.
            let manifest = if let Err(e) = entry.manifest.validate() {
                tracing::warn!(
                    app_id = %id,
                    app_name = %entry.name,
                    error = %e,
                    "gateway-sync: manifest validation failed — falling back to passthrough"
                );
                Manifest::passthrough()
            } else {
                entry.manifest.clone()
            };
            let compiled_manifest = Arc::new(CompiledManifest::compile(&manifest));
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

    pub fn lookup_by_name(&self, name: &str) -> Option<(Uuid, Arc<CompiledRoute>)> {
        let name_idx = self.name_index.read().unwrap();
        let app_id = name_idx.get(name)?;
        let routes = self.routes.read().unwrap();
        routes.get(app_id).map(|r| (*app_id, r.clone()))
    }

    /// Resolve a route by its per-app OAuth `client_id` (= `oac_<base62>`).
    ///
    /// Per-app back-channel logout (auth-sdk Slice 1d, spec §1.2): the inbound
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
    ) -> Option<(Uuid, Arc<CompiledRoute>)> {
        let routes = self.routes.read().unwrap();
        routes.iter().find_map(|(id, route)| {
            if route.entry.oauth_client_id.as_deref() == Some(client_id) {
                Some((*id, route.clone()))
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
    let routes: RouteMap =
        serde_json::from_str(&response).map_err(|e| format!("parse routes: {e}"))?;
    state.routes.update(routes);
    Ok(())
}

async fn http_get(url: &str, auth_key: &str) -> Result<String, String> {
    compio::time::timeout(CONTROL_REQUEST_TIMEOUT, http_get_inner(url, auth_key))
        .await
        .map_err(|_| control_timeout_error())?
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

    #[test]
    fn control_timeout_defaults_to_five_seconds() {
        assert_eq!(CONTROL_REQUEST_TIMEOUT, std::time::Duration::from_secs(5));
        assert_eq!(control_timeout_error(), "control request timed out after 5s");
    }

    fn route_entry(name: &str, oauth_client_id: Option<&str>, sector: Option<&str>) -> RouteEntry {
        RouteEntry {
            name: name.to_string(),
            plan_id: "free".to_string(),
            api_key_hash: "h".to_string(),
            deploy_hash: None,
            manifest: Manifest::passthrough(),
            oauth_client_id: oauth_client_id.map(str::to_string),
            sector_identifier: sector.map(str::to_string),
        }
    }

    #[test]
    fn route_sync_surfaces_oauth_fields_on_compiled_route() {
        // The route-sync compile path (`RouteCache::update`) must thread
        // the §1.5 OAuth identity fields from the source `RouteEntry`
        // through to the `CompiledRoute` that `lookup_by_name` returns,
        // so the gateway resolves a per-app `oauth_client_id`/sector
        // from the request `Host` without a second lookup.
        let provisioned_id = Uuid::new_v4();
        let unprovisioned_id = Uuid::new_v4();

        let mut routes: RouteMap = HashMap::new();
        routes.insert(
            provisioned_id,
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
        cache.update(routes);

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
    fn required_scopes_plumb_from_manifest_json_through_to_compiled_route() {
        // Faithful end-to-end of the wire path control `get_routes` uses
        // (auth-sdk Slice 3c, §5.3): the per-route `required_scopes` ride
        // INSIDE the app manifest (`ResourceEntry.required_scopes`), which
        // is exactly the `manifest_json` column control serialises and the
        // gateway parses. We serialise a manifest carrying a scoped resource
        // to JSON (the column shape), parse it back as `registry::get_routes`
        // does, hang it on a `RouteEntry`, push it through `RouteCache::update`
        // (the compile step), and assert the compiled `EffectivePolicy`
        // carries the scopes the gateway auth gate enforces.
        use std::collections::HashMap as Map;
        use zeroship_bundle::{AuthConfig, AuthLevel, ProcedureKind, ResourceEntry, ScopeDef};

        let mut resources: Map<String, ResourceEntry> = Map::new();
        resources.insert(
            "rpc:billing.read".to_string(),
            ResourceEntry {
                kind: Some(ProcedureKind::Query),
                auth: Some(AuthLevel::User),
                required_scopes: vec!["read:billing".to_string()],
                ..Default::default()
            },
        );
        let manifest = Manifest {
            version: 1,
            resources,
            // The route's `required_scopes` must reference a DECLARED scope
            // (Slice 3c validate-time check) — declare it so the fixture is a
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

        let app_id = Uuid::new_v4();
        let mut routes: RouteMap = HashMap::new();
        routes.insert(
            app_id,
            RouteEntry {
                name: "billing-app.zeroship.localhost".to_string(),
                plan_id: "free".to_string(),
                api_key_hash: "h".to_string(),
                deploy_hash: None,
                manifest: parsed,
                oauth_client_id: Some("oac_billing".to_string()),
                sector_identifier: Some("https://billing-app.zeroship.localhost".to_string()),
            },
        );

        let cache = RouteCache::new();
        cache.update(routes);

        let (_, compiled) = cache
            .lookup_by_name("billing-app.zeroship.localhost")
            .expect("route resolves");
        let policy = compiled
            .manifest
            .lookup_resource("/_zs/v1/billing.read")
            .expect("scoped resource compiles");
        assert_eq!(
            policy.required_scopes,
            vec!["read:billing".to_string()],
            "required_scopes must plumb control→CompiledRoute via the manifest"
        );
    }

    #[test]
    fn lookup_by_oauth_client_id_resolves_provisioned_app_and_skips_unprovisioned() {
        // Per-app BCL disambiguation (Slice 1d §1.2): the BCL handler resolves
        // the app from the `logout_token.aud` (= per-app client_id). A
        // provisioned client resolves to its app's route (and subdomain name);
        // an un-provisioned app (oauth_client_id == None) never matches; an
        // unknown client_id resolves to nothing.
        let provisioned_id = Uuid::new_v4();
        let mut routes: RouteMap = HashMap::new();
        routes.insert(
            provisioned_id,
            route_entry(
                "myapp.zeroship.localhost",
                Some("oac_myapp"),
                Some("https://myapp.zeroship.localhost"),
            ),
        );
        routes.insert(
            Uuid::new_v4(),
            route_entry("other.zeroship.localhost", None, None),
        );

        let cache = RouteCache::new();
        cache.update(routes);

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
}
