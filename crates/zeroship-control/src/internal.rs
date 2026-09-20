//! Internal API handlers — worker-facing endpoints.

use std::sync::Arc;

use ntex::web;
use ntex::web::types::{Path, State};
use zeroship_core::app_id::AppId;
use zeroship_core::service_assertion::{
    presented_issuer, thumbprint_key_id, AssertionError, ReplayStore, ServiceAssertionVerifier,
    ServiceIssuer, ServiceTrustBundle,
};
use zeroship_core::service_identity::{
    endpoints, verify_service_call, AuthError, ServiceEndpoint, ServiceIdentity,
};
use zeroship_core::service_peers::{
    service_issuer, CONTROL_SERVICE_NAME, WORKER_SERVICE_NAME,
};

use crate::AppState;

// ---------------------------------------------------------------------------
// Auth helpers
// ---------------------------------------------------------------------------

/// The identifier this control plane mints under, and the one callers must
/// address it as.
///
/// ONE STATEMENT of control's own name. `main`'s keyring is built on it and
/// `verify_worker_instance` compares an instance's `aud` against it, and the
/// two must not be able to disagree: control declares no
/// `ServiceKeyring::addressed_as`, so the name it mints under IS the name
/// callers address, and a second spelling of either would refuse every worker
/// instance while the role path carried on working.
///
/// # Errors
///
/// Returns [`AssertionError::MalformedIssuer`] if [`CONTROL_SERVICE_NAME`] ever
/// stops being a well-formed service path. `zeroship-core` has a test saying it
/// is one; this propagates rather than unwraps because one of its two callers
/// is an authentication path, which must refuse rather than panic.
pub fn control_service_issuer() -> Result<ServiceIssuer, AssertionError> {
    service_issuer(CONTROL_SERVICE_NAME)
}

/// The `jti` single-use cache every inbound service assertion is claimed in.
///
/// ONE STATEMENT for the same reason as [`control_service_issuer`]. This
/// process builds one verifier at boot for role assertions and one per request
/// for instance assertions; two different stores would be two different answers
/// to "has this assertion been seen", which is single-use per verifier rather
/// than single use.
#[must_use]
pub fn control_replay_store(
    control_pg: Arc<compio_postgres::Client>,
) -> Arc<dyn ReplayStore + Send + Sync> {
    Arc::new(zeroship_authn::service_replay::SharedClientReplayStore::new(control_pg))
}

/// Refuse a caller that has not proved WHICH SERVICE it is, or that holds no
/// grant on this endpoint.
///
/// The FULL assertion profile: signed, and single-use through the replay store.
/// Correct here because these endpoints fire at app-load rate - the worker calls
/// them once per app it loads and again on a version change - and at
/// account-deletion rate on the erasure preflight, which is rarer still. The
/// store write is proportional to those, never to end-user traffic. Do not reach
/// for this on a per-request path.
///
/// It replaces [`check_auth`] on the privileged reads, and the difference is
/// the whole point of the change: the shared bearer proves only that the caller
/// read the same file the control plane did, so every holder of it is every
/// other holder. An assertion names one service, under a key only that service
/// holds, and the endpoint allowlist then decides what that service may reach.
///
/// `pub(crate)` so `crate::erasure` runs THIS check rather than growing a second
/// copy, for the same reason [`check_auth`] is.
///
/// It takes the whole [`AppState`] rather than the `ServiceAuth` alone
/// because control resolves a worker INSTANCE's key from its own registry
/// before verification - see `verify_service_caller`.
pub(crate) async fn check_service_auth(
    req: &web::HttpRequest,
    state: &AppState,
    endpoint: ServiceEndpoint,
) -> Option<web::HttpResponse> {
    let header = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok());
    match verify_service_caller(state, header, endpoint).await {
        Ok(_identity) => None,
        Err(error) => {
            tracing::warn!(
                method = %req.method(),
                path = %req.path(),
                %error,
                "control-internal: service auth rejected"
            );
            Some(service_auth_refusal(&error))
        }
    }
}

/// The one response an [`AuthError`] maps to, shared by every internal guard.
///
/// A store outage refuses every caller at once and is an operator's problem,
/// so it answers 503 rather than 401 - a caller told "unauthorized" would
/// rotate a credential that is fine.
fn service_auth_refusal(error: &AuthError) -> web::HttpResponse {
    if matches!(error, AuthError::StoreUnavailable) {
        web::HttpResponse::ServiceUnavailable().json(&serde_json::json!({"error":"service unavailable"}))
    } else {
        web::HttpResponse::Unauthorized().json(&serde_json::json!({"error":"unauthorized"}))
    }
}

/// Verify the caller of an endpoint whose handler acts on the caller's OWN
/// row, and return the instance segment of the issuer it verified as.
///
/// The same guard as [`check_service_auth`], specialised to the endpoints
/// whose handlers need to know WHICH row called: `/internal/workers/retire`
/// retires the calling worker instance, `/internal/workers/renew` extends its
/// lease, and the host app reads narrow themselves to the calling instance's
/// execution zone. A [`ServiceIdentity`] deliberately carries only the caller's
/// ROLE (`identity_from` in `zeroship-core::service_assertion` builds it from
/// `issuer.principal()` alone, never the instance segment - the allowlist it
/// feeds is written against roles), so the id is not read from it. It is
/// instead re-read from the header's issuer string, which by this point is not
/// merely CLAIMED: [`verify_service_caller`] has already verified the
/// assertion's signature against a trust bundle that trusts exactly this issuer
/// identifier and no other, so the instance segment it carries names the row
/// that was actually cryptographically proven.
///
/// Which ROLE that row belongs to is settled by the endpoint's grant:
/// `svc/worker` alone holds `CONTROL_WORKER_RETIRE`, `CONTROL_WORKER_RENEW` and
/// the three host app reads, so a verified caller of any of them is a worker
/// instance. JOINING is not here: a joining process has no service identity yet.
async fn verified_instance_caller(
    req: &web::HttpRequest,
    state: &AppState,
    endpoint: ServiceEndpoint,
) -> Result<String, web::HttpResponse> {
    let header = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok());
    match verify_service_caller(state, header, endpoint).await {
        Ok(_identity) => presented_issuer(header)
            .and_then(|issuer| issuer.instance().map(str::to_owned))
            .ok_or_else(|| {
                // Unreachable in practice: both roles that hold these grants
                // authenticate at INSTANCE arity only - `verify_service_caller`
                // refuses their role arity before any verifier runs. If this
                // ever fires, refuse rather than act on no attributable row.
                tracing::error!(
                    path = endpoint.path_template(),
                    "control-internal: an instance-scoped call verified with no instance segment"
                );
                web::HttpResponse::Unauthorized().json(&serde_json::json!({"error":"unauthorized"}))
            }),
        Err(error) => {
            tracing::warn!(
                method = %req.method(),
                path = %req.path(),
                %error,
                "control-internal: instance-scoped auth rejected"
            );
            Err(service_auth_refusal(&error))
        }
    }
}

/// Verify a host app read and narrow it to the calling instance's zone.
///
/// The three host reads (`CONTROL_APP`, `CONTROL_APP_ENV`,
/// `CONTROL_APP_DATA_KEY`) are held by `svc/worker` alone, which authenticates
/// at instance arity, so every caller names a row Control enrolled. The app
/// and the instance each belong to one frozen execution zone, and a worker
/// serves only its own: an app's environment is its decrypted secrets and its
/// project data key is a decryption capability, so reaching either from
/// another zone is exactly what zones exist to prevent.
///
/// Refusing with `403` rather than `404` keeps a misconfigured deployment
/// diagnosable: the caller is authenticated and its zone is an operator fact,
/// so nothing is learned from the distinction that the operator does not
/// already hold.
///
/// It takes the path segment rather than a parsed [`AppId`] so the credential
/// is still checked before anything is read from the request path: an
/// unauthenticated caller is refused whatever it named.
async fn zone_scoped_app_read(
    req: &web::HttpRequest,
    state: &AppState,
    endpoint: ServiceEndpoint,
    app_id: &str,
    malformed: impl FnOnce() -> web::HttpResponse,
) -> Result<AppId, web::HttpResponse> {
    let instance = verified_instance_caller(req, state, endpoint).await?;
    let Ok(app) = AppId::parse(app_id) else {
        return Err(malformed());
    };
    match crate::worker_join::instance_serves_app(
        state.control_pg.as_ref(),
        &instance,
        app.as_str(),
    )
    .await
    {
        Ok(true) => Ok(app),
        Ok(false) => {
            tracing::warn!(
                path = endpoint.path_template(),
                app_id = %app.as_str(),
                "control-internal: host app read outside the caller's execution zone"
            );
            Err(web::HttpResponse::Forbidden()
                .json(&serde_json::json!({"error":"app is outside this worker's execution zone"})))
        }
        Err(error) => {
            tracing::error!(
                path = endpoint.path_template(),
                %error,
                "control-internal: execution zone lookup failed"
            );
            Err(web::HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error":"service unavailable"})))
        }
    }
}

/// Route one inbound credential to the keys that are allowed to verify it.
///
/// Two sources, chosen by the ARITY of the issuer and by nothing else:
///
/// - A ROLE identifier is verified against the operator's peer document. That
///   file is the only source of role keys and stays so.
/// - An identifier naming an INSTANCE of a role is verified against the key
///   control itself recorded at JOIN, because no peer document carries one: a
///   worker draws its instance keypair in memory at boot.
///
/// `ServiceTrustBundle::keys_for` is an exact-string lookup, so the second kind
/// resolves nothing in the first source - which is why control cannot simply
/// hand every caller to the boot-time verifier and is why this branch exists.
///
/// # The worker role has NO role arity
///
/// `svc/worker` authenticates as an INSTANCE and in no other way, and a
/// role-arity assertion naming it is refused here before any verifier runs. No
/// process holds a role signing key for it, and the operator's peer document no
/// longer publishes one - but that is a fact about the files a deployment was
/// given, and a document that still carried an old `svc/worker` key would
/// otherwise let whoever held its private half read any app's environment with
/// no join, no status, no lease and nothing a revocation reaches. Refusing the
/// arity makes the rule a property of this verifier rather than of every
/// deployment's key hygiene.
///
/// The resolution happens HERE, before any verification, and hands verification
/// a bundle. It is deliberately NOT a resolver seam a verifier calls back into:
/// `zeroship-core` holds inter-service wire types and would have to grow a
/// database-shaped trait for that, and it would buy nothing, because the role
/// and the instance are separable at parse time and "is a lookup even needed"
/// is therefore answerable before the first check runs.
pub(crate) async fn verify_service_caller(
    state: &AppState,
    authorization: Option<&str>,
    endpoint: ServiceEndpoint,
) -> Result<ServiceIdentity, AuthError> {
    match presented_instance_issuer(authorization) {
        Some(issuer) => verify_worker_instance(state, authorization, &issuer, endpoint).await,
        None => {
            if let Some(issuer) = presented_issuer(authorization) {
                if names_an_instance_only_role(&issuer)? {
                    tracing::warn!(
                        issuer = issuer.as_str(),
                        "control-internal: refusing a role-arity assertion for a role that \
                         authenticates only as an enrolled instance"
                    );
                    return Err(AuthError::CredentialRejected);
                }
            }
            state.service_auth.verify(authorization, endpoint).await
        }
    }
}

/// Whether `issuer` names the one role that authenticates only at instance
/// arity: `svc/worker`, the joined worker instances.
fn names_an_instance_only_role(issuer: &ServiceIssuer) -> Result<bool, AuthError> {
    let role = service_issuer(WORKER_SERVICE_NAME).map_err(|error| {
        tracing::error!(%error, "control-internal: the worker role issuer is malformed");
        AuthError::CredentialRejected
    })?;
    Ok(issuer.principal() == role.principal())
}

/// The issuer an unverified assertion CLAIMS, when that issuer names an
/// instance.
///
/// Read from the UNVERIFIED payload for one purpose: choosing where the
/// verification key comes from. Nothing is trusted on the strength of it. The
/// signature still has to hold under a key already bound to that identifier,
/// and the verifier re-checks the VERIFIED `iss` against the same selector, so
/// a lie here can only select a key that fails to verify.
///
/// `None` covers "no header", "not a bearer", "not a three-part JWT",
/// "unparseable payload", "no `iss`", "not an issuer identifier", and "an
/// issuer naming a role". Every one of those takes the role path, which refuses
/// the malformed ones itself; there is no arm here that admits anything.
fn presented_instance_issuer(authorization: Option<&str>) -> Option<ServiceIssuer> {
    zeroship_core::service_assertion::presented_issuer(authorization)
        .filter(|issuer| issuer.instance().is_some())
}

/// Resolve the live key an instance-arity issuer names.
///
/// `svc/worker/<wkr>` resolves from `zeroship.worker_instances`, under a
/// predicate that is BOTH `status = 'active'` and an unexpired lease, so a
/// retired instance, a purged one and an abandoned one all stop authenticating
/// through one read and no join.
///
/// An instance identifier naming any OTHER role resolves nowhere and is
/// refused: only the worker role ever mints an instance identifier, and another
/// one appearing here would be a role this dispatch does not know how to
/// verify, not a caller to trust by default. A JOIN SIGNER is deliberately not
/// resolvable here - its key verifies join tokens and nothing else, through a
/// different `typ` and a different code path.
async fn resolve_instance_public_key(
    state: &AppState,
    issuer: &ServiceIssuer,
    instance: &str,
) -> Result<[u8; 32], AuthError> {
    let worker_role = service_issuer(WORKER_SERVICE_NAME).map_err(|error| {
        tracing::error!(%error, "control-internal: the worker role issuer is malformed");
        AuthError::CredentialRejected
    })?;
    if issuer.principal() != worker_role.principal() {
        tracing::warn!(
            issuer = issuer.as_str(),
            "control-internal: instance-arity issuer names a role with no registry to resolve from"
        );
        return Err(AuthError::CredentialRejected);
    }

    match crate::worker_join::active_instance_public_key(state.control_pg.as_ref(), instance).await
    {
        Ok(Some(public)) => Ok(public),
        Ok(None) => {
            tracing::warn!(
                issuer = issuer.as_str(),
                "control-internal: no LIVE instance is registered under this issuer"
            );
            Err(AuthError::CredentialRejected)
        }
        Err(error) => {
            // The registry is a store this verification must consult, so an
            // unreachable one refuses WITHOUT judging - the same shape as the
            // replay store, and it answers 503 upstream rather than 401.
            tracing::error!(
                %error,
                issuer = issuer.as_str(),
                "control-internal: the enrolment registry could not be read"
            );
            Err(AuthError::StoreUnavailable)
        }
    }
}

/// Verify an assertion minted by a joined worker INSTANCE.
///
/// The bundle handed to verification carries exactly one key under exactly one
/// issuer: the caller's key, under the identifier it presented. That
/// is the design's "a bundle carrying that one extra key" - for an INSTANCE
/// identifier the operator document's entries are unreachable either way,
/// because `keys_for` matches the full identifier and a role's entry is a
/// different string - and it is the narrower of the two spellings, which is
/// what makes the second bullet below structural.
///
/// Two consequences, both load-bearing, and both structural rather than
/// reviewed:
///
/// - The key is published under the INSTANCE issuer and never under the role.
///   Publishing it under `svc/worker` would let one instance's key verify an
///   assertion attributed to the role itself, which is the collapse the
///   issuer/instance split exists to prevent.
/// - An instance row cannot introduce or replace a ROLE key. The row
///   contributes 32 bytes and no name; the identifier those bytes are filed
///   under is the caller's, and `ServiceIssuer::parse` admits an instance
///   identifier only at one path segment more than a role, so no row can
///   produce a role entry. (The `worker_instances_id_shape` CHECK does not make
///   this unreachable and is not what holds it: the lookup runs issuer to row,
///   never row to issuer, so the row's `id` never becomes a name here at all.)
///
/// There is NO fallback to the role's key when the lookup comes back empty. An
/// instance control has not recorded, has retired, or whose lease has lapsed
/// holds nothing here - and a fallback would also make a key an operator could
/// file in the peer document authenticate, which is a credential carrying
/// neither status nor lease and so one nothing can stop.
///
/// Nothing is cached. The proposal leaves caching open until a measurement asks
/// for it, and records that any cache needs an invalidation story for a revoked
/// instance: a cached key outliving its revocation is worse than the read.
async fn verify_worker_instance(
    state: &AppState,
    authorization: Option<&str>,
    issuer: &ServiceIssuer,
    endpoint: ServiceEndpoint,
) -> Result<ServiceIdentity, AuthError> {
    // Total: this function is reached only for an issuer that names one.
    let instance = issuer.instance().ok_or(AuthError::CredentialRejected)?;
    let public = resolve_instance_public_key(state, issuer, instance).await?;

    let audience = control_service_issuer().map_err(|error| {
        tracing::error!(%error, "control-internal: this control plane's own issuer is malformed");
        AuthError::CredentialRejected
    })?;
    let mut bundle = ServiceTrustBundle::new();
    bundle
        .trust(issuer, thumbprint_key_id(&public), public)
        .map_err(|error| {
            tracing::error!(
                %error,
                issuer = issuer.as_str(),
                "control-internal: the registered instance key was refused by the bundle"
            );
            AuthError::CredentialRejected
        })?;
    let verifier =
        ServiceAssertionVerifier::new(bundle, control_replay_store(Arc::clone(&state.control_pg)));
    verify_service_call(&verifier, authorization, audience.as_str(), endpoint).await
}

/// The shared-control-key check the `/internal/*` endpoints that are NOT
/// privileged still run.
///
/// It is deliberately not the check on the privileged reads: those take
/// [`check_service_auth`] above, which names one service under a key only that
/// service holds, where this one proves only that the caller read the same file
/// the control plane did. Every endpoint still on it is one whose caller set is
/// the four processes that hold that file; adding a fifth holder is how a
/// narrow need for one route becomes a grant on all of them, which is what
/// happened when the erasure preflight briefly landed here.
fn check_auth(req: &web::HttpRequest, state: &AppState) -> Option<web::HttpResponse> {
    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = zeroship_core::auth::extract_bearer(header);
    match token {
        // Empty control_key never authenticates. Otherwise an unauthenticated
        // GET to /internal/* could leak decrypted secrets to the network.
        Some(key)
            if !state.control_key.is_empty()
                && zeroship_core::auth::constant_time_eq(
                    key,
                    state.control_key.expose_secret(),
                ) =>
        {
            None
        }
        _ => {
            tracing::warn!(method = %req.method(), path = %req.path(), "control-internal: auth rejected");
            Some(
                web::HttpResponse::Unauthorized()
                    .json(&serde_json::json!({"error":"unauthorized"})),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Worker-authenticated: return the merged env for a given app as a
/// JSON object in the split `{ vars, secrets, expose }` shape. Workers
/// call this on bundle load and cache the result per-thread.
///
/// The split shape is the contract the runtime expects (see
/// `crates/zeroship-runtime/src/fetch_outcome.rs::EnvSnapshot`): vars are always
/// in `process.env`, secrets are NOT in `process.env` unless their name
/// is in the per-app `expose` list, and both are visible via
/// `import { env } from "zeroship"` and `env.get(name)`.
pub async fn get_app_env(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    app_id: Path<String>,
) -> web::HttpResponse {
    // Internal endpoint, no user authz: the caller is a SERVICE and there is no
    // user principal. It presents its own ed25519 assertion under the full
    // profile; a shared bearer does not open this door, which matters most
    // here because the response body is the app's DECRYPTED environment.
    let id = match zone_scoped_app_read(&req, &state, endpoints::CONTROL_APP_ENV, &app_id, || {
        web::HttpResponse::BadRequest().json(&serde_json::json!({"error": "bad app_id"}))
    })
    .await
    {
        Ok(id) => id,
        Err(response) => return response,
    };
    match state.env_store.merged_env_for_worker(&id).await {
        Ok(value) => web::HttpResponse::Ok().json(&value),
        Err(crate::env_store::EnvError::AppNotFound) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => {
            tracing::error!(app_id = %id.as_str(), error = %e, "control-internal: env fetch error");
            web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error":"internal error"}))
        }
    }
}

/// Host-only project key delivery. The app's project comes from control's
/// registry, and the key never enters the app environment response.
pub async fn get_app_data_key(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    app_id: Path<String>,
) -> web::HttpResponse {
    let id = match zone_scoped_app_read(
        &req,
        &state,
        endpoints::CONTROL_APP_DATA_KEY,
        &app_id,
        || web::HttpResponse::BadRequest().json(&serde_json::json!({"error":"bad app_id"})),
    )
    .await
    {
        Ok(id) => id,
        Err(response) => return response,
    };
    match crate::project_keys::for_app(&state.registry, state.env_store.cipher(), &id).await {
        Ok(key) => web::HttpResponse::Ok()
            .header("cache-control", "no-store")
            .json(&key),
        Err(crate::project_keys::KeyError::AppNotFound) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(crate::project_keys::KeyError::Storage(error)) => {
            tracing::error!(app_id = %id.as_str(), %error, "control-internal: project key delivery failed");
            web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error":"internal error"}))
        }
    }
}

/// Host-only database-binding delivery.
///
/// **The worker composes no part of a binding.** The database id names the
/// physical schema, the edge id and the schema epoch together name the role a
/// session narrows to, and all three are Control facts. A worker that derived
/// any of them would address a schema and assume a role no reconciler created.
///
/// Only a LIVE binding is served, and "live" is a predicate rather than
/// judgement at the caller:
/// [`zeroship_core::live_binding::LIVE_BINDINGS_FROM_WHERE`], which carries
/// each conjunct and why it is there. It is shared with the CDC relay and the
/// migration service because a binding one of them calls live and another does
/// not is a tenant-boundary disagreement.
///
/// A stale epoch is not a failure of this read: the cluster's own epoch row is
/// the authority, so composing a retired one makes `SET LOCAL ROLE` fail and
/// the caller re-resolve. That is the fail-closed direction and it is why this
/// serves a projection rather than reading the cluster.
pub async fn get_app_bindings(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    app_id: Path<String>,
) -> web::HttpResponse {
    let id = match zone_scoped_app_read(
        &req,
        &state,
        endpoints::CONTROL_APP_BINDINGS,
        &app_id,
        || web::HttpResponse::BadRequest().json(&serde_json::json!({"error":"bad app_id"})),
    )
    .await
    {
        Ok(id) => id,
        Err(response) => return response,
    };
    // EVERY live binding, not one: an app may bind many databases and
    // `env.databases` reaches all of them. A `LIMIT 1` here would leave every
    // non-primary handle unresolvable while looking like a working endpoint.
    let rows = match state
        .control_pg
        .query(
            &format!(
                "SELECT b.id AS binding_id, b.database_id, d.schema_epoch {} ORDER BY b.id",
                zeroship_core::live_binding::LIVE_BINDINGS_FROM_WHERE
            ),
            &[&id.as_str()],
        )
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(app_id = %id.as_str(), %error, "control-internal: binding read failed");
            return web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error":"internal error"}));
        }
    };
    if rows.is_empty() {
        return web::HttpResponse::NotFound()
            .json(&serde_json::json!({"error":"no live database binding"}));
    }
    let mut bindings = Vec::with_capacity(rows.len());
    for row in &rows {
        let epoch: i32 = row.get("schema_epoch");
        let Ok(epoch) = u32::try_from(epoch) else {
            tracing::error!(app_id = %id.as_str(), "control-internal: negative schema epoch");
            return web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error":"internal error"}));
        };
        bindings.push(serde_json::json!({
            "binding_id": row.get::<_, &str>("binding_id"),
            "database_id": row.get::<_, &str>("database_id"),
            "schema_epoch": epoch,
        }));
    }
    web::HttpResponse::Ok()
        .header("cache-control", "no-store")
        .json(&serde_json::json!({ "bindings": bindings }))
}

/// POST /internal/workers/join - a worker registers ONE live process.
///
/// NOT guarded by the service-assertion allowlist, and that is not an omission.
/// A joining process has no service identity yet: it presents a JOIN TOKEN a
/// trusted signer minted, which `crate::worker_join::join` verifies against
/// Control's own signer registry under its own `typ`. Nothing about that token
/// is a service assertion and nothing in the allowlist can reach this route.
///
/// The caller supplies its listening PORT, its instance PUBLIC KEY and a
/// signature over all of it made with the private half of that key. It supplies
/// no host and no zone, and this handler offers no way to: the host is derived
/// from `req.peer_addr()` - the address the TRANSPORT observed - and the zone is
/// the token's claim. There is deliberately no fallback when the transport
/// exposes no peer: that fallback is the vulnerability, and
/// `crates/zeroship-control/src/worker_join.rs` says what it would cost.
///
/// The `X-Forwarded-For` machinery this crate carries for audit and rate-limit
/// identity ([`crate::http_util::source_ip`]) is NOT reachable from here, on
/// purpose: a forwarded header is a caller-supplied host wearing a proxy's
/// clothes. Control's own `trust_proxy` instead REFUSES joining outright,
/// because behind a trusted proxy every observed peer is the proxy and the
/// derivation would place every worker at one address.
///
/// The bearer is read HERE and passed down as bytes, because the join proof is
/// a signature over the EXACT token string and two readings of a header are two
/// chances to disagree about what was signed.
pub async fn join_worker_instance(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    body: web::types::Json<crate::worker_join::WorkerJoinRequest>,
) -> web::HttpResponse {
    let token = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(zeroship_core::auth::extract_bearer)
        .unwrap_or_default()
        .to_owned();
    if token.is_empty() {
        return web::HttpResponse::Unauthorized()
            .json(&serde_json::json!({"error": "join refused", "reason": "no_join_token"}));
    }
    crate::worker_join::join(&state, req.peer_addr(), &token, body.into_inner()).await
}

/// POST /internal/workers/renew - a worker extends its OWN instance lease.
///
/// An instance identity expires, so a worker that means to keep serving renews
/// on a schedule derived from the lease. The call carries no body and no
/// selector: the instance renewed is the one whose key verified the request, so
/// no worker can hold another's identity open.
///
/// NO JOIN TOKEN IS INVOLVED, and none would help. Possession of the instance
/// key was proved at join and that proof is what renewal rests on; demanding a
/// fresh token here would make a use-capped token useless, because every worker
/// would need a new use every few minutes for as long as it ran.
pub async fn renew_worker_instance(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let instance_id =
        match verified_instance_caller(&req, &state, endpoints::CONTROL_WORKER_RENEW).await {
            Ok(id) => id,
            Err(resp) => return resp,
        };
    crate::worker_join::renew(&state, &instance_id).await
}

/// POST /internal/workers/retire - a worker declares its OWN instance gone.
///
/// A worker calls this once, after a graceful shutdown has drained it, so the
/// instance key it is about to discard stops authenticating immediately rather
/// than lingering as an `active` row with no process behind it. The call
/// carries no body and no selector: the instance retired is the one whose key
/// verified the request, so no caller can retire another instance.
///
/// This is a DECLARATION by the instance's own holder, not an observation,
/// which is why it may write `status` where the liveness monitor in
/// `crate::worker_health` must not: nothing here follows a probe. A worker
/// that crashes never calls it, and its row stays `active` exactly as before.
///
/// A repeated call cannot succeed twice: once the row is `gone`, the same key
/// no longer authenticates, so a retry after a lost reply is refused at the
/// guard and the instance stays retired.
pub async fn retire_worker_instance(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let instance_id =
        match verified_instance_caller(&req, &state, endpoints::CONTROL_WORKER_RETIRE).await {
            Ok(id) => id,
            Err(resp) => return resp,
        };
    crate::worker_join::retire(&state, &instance_id).await
}

pub async fn get_versions(req: web::HttpRequest, state: State<Arc<AppState>>) -> web::HttpResponse {
    // Internal endpoint, no user authz: workers authenticate with the
    // control-key shared secret and there is no user principal.
    if let Some(resp) = check_auth(&req, &state) {
        return resp;
    }
    match state.registry.get_versions().await {
        Ok(versions) => web::HttpResponse::Ok().json(&versions),
        Err(e) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({"error": e.to_string()})),
    }
}

pub async fn get_app_version(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    app_id: Path<String>,
) -> web::HttpResponse {
    // Same rate as the env read - once per app load, plus a refetch on a
    // version change - so the same full profile.
    let uid = match zone_scoped_app_read(&req, &state, endpoints::CONTROL_APP, &app_id, || {
        web::HttpResponse::BadRequest().json(&serde_json::json!({"error":"invalid app id"}))
    })
    .await
    {
        Ok(id) => id,
        Err(response) => return response,
    };
    match state.registry.get_versions().await {
        Ok(versions) => match versions.get(&uid) {
            Some(info) => web::HttpResponse::Ok().json(info),
            None => {
                web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
            }
        },
        Err(e) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({"error": e.to_string()})),
    }
}

pub async fn get_routes(req: web::HttpRequest, state: State<Arc<AppState>>) -> web::HttpResponse {
    // Internal endpoint, no user authz: gateways authenticate with the
    // control-key shared secret and there is no user principal.
    if let Some(resp) = check_auth(&req, &state) {
        return resp;
    }
    match state.registry.get_gateway_snapshot().await {
        Ok(snapshot) => web::HttpResponse::Ok().json(&snapshot),
        Err(e) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({"error": e.to_string()})),
    }
}

/// POST /internal/billing/reconcile?period=<unix-seconds> — operator-gated
/// on-demand trigger of the billing reconciler for a SPECIFIC closed period.
///
/// Same gate as every other `/internal/*` endpoint ([`check_auth`]): the
/// control-key shared secret. Without a valid control-key bearer it 401s exactly
/// like the operator reconcile endpoints.
///
/// The production reconcile cron only ever bills the PREVIOUS calendar month
/// (`previous_period_start_unix(now)`), which an end-to-end test cannot wait a
/// month for. This endpoint drives the SAME [`crate::cron::billing_reconcile::tick_with`]
/// sweep against a caller-chosen `period` so a harness (or an operator
/// re-running a missed close) can reconcile a specific closed period on demand.
/// Idempotency is unchanged: the `billing_runs` / `billing_run_items` guards
/// make a repeat trigger for the same period a no-op.
///
/// `period` is the unix-seconds start of the calendar month to bill. We pass it
/// as `now` to `tick_with`, which derives the period it bills as
/// `previous_period_start_unix(now)` — so the caller passes a timestamp in the
/// month AFTER the one they want billed (mirroring how the cron, ticking in
/// month M, bills M-1). The response echoes the resolved `period_start` so the
/// caller can assert which period was reconciled.
pub async fn force_reconcile(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(resp) = check_auth(&req, &state) {
        return resp;
    }
    // `?period=<unix-seconds>` — the `now` instant to reconcile against. Default
    // to the live wall clock (bills the previous calendar month, like the cron).
    let now_unix: i64 = req
        .query_string()
        .split('&')
        .find_map(|kv| kv.strip_prefix("period="))
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or_else(|| chrono::Utc::now().timestamp());

    let stripe = crate::stripe_client::StripeClient::new(crate::SecretString::new(
        state.stripe_secret_key.expose_secret().to_string(),
    ))
    .with_base_url(state.stripe_base_url.clone());

    match crate::cron::billing_reconcile::tick_with(&state, &stripe, now_unix).await {
        Ok(billed) => {
            let period_start = crate::cron::billing_reconcile::previous_period_start_unix(now_unix);
            web::HttpResponse::Ok().json(&serde_json::json!({
                "billed": billed,
                "period_start": period_start,
                "now": now_unix,
            }))
        }
        Err(e) => {
            tracing::error!(error = %e, now_unix, "control-internal: force_reconcile failed");
            web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error": e.to_string()}))
        }
    }
}

/// POST /internal/spend/reconcile — operator-gated on-demand trigger of ONE
/// spend-reconcile sweep ([`crate::cron::spend_reconcile::tick`]).
///
/// Same gate as every other `/internal/*` endpoint ([`check_auth`]). The
/// spend cron runs every ~60s on its own; this lets an operator (or an E2E)
/// force a single sweep immediately so the derived [`zeroship_core::types::SpendState`] is persisted
/// without waiting a full tick. The gateway still picks the new state up on its
/// next `/internal/routes` poll (decision D1) — this endpoint only advances the
/// CONTROL-side derivation, it does not push to the gateway. Idempotent: a
/// no-op sweep simply reports `transitions: 0`.
pub async fn force_spend_reconcile(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(resp) = check_auth(&req, &state) {
        return resp;
    }
    match crate::cron::spend_reconcile::tick(&state).await {
        Ok(transitions) => web::HttpResponse::Ok().json(&serde_json::json!({
            "transitions": transitions,
        })),
        Err(e) => {
            tracing::error!(error = %e, "control-internal: force_spend_reconcile failed");
            web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error": e.to_string()}))
        }
    }
}
