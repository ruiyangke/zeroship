//! Internal API handlers — worker-facing endpoints.

use std::sync::Arc;

use ntex::web;
use ntex::web::types::{Path, State};
use zeroship_core::app_id::AppId;
use zeroship_core::readiness::ReadinessGate;
use zeroship_core::service_assertion::{
    thumbprint_key_id, AssertionError, ReplayStore, ServiceAssertionVerifier, ServiceIssuer,
    ServiceTrustBundle,
};
use zeroship_core::service_identity::{
    endpoints, verify_service_call, AuthError, ServiceEndpoint, ServiceIdentity,
};
use zeroship_core::service_peers::{service_issuer, CONTROL_SERVICE_NAME};

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
    Arc::new(zeroship_authn::service_replay::SharedClientReplayStore::new(
        control_pg,
    ))
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
            // A store outage refuses every caller at once and is an operator's
            // problem, so it answers 503 rather than 401 - a caller told
            // "unauthorized" would rotate a credential that is fine.
            Some(if matches!(error, AuthError::StoreUnavailable) {
                web::HttpResponse::ServiceUnavailable()
                    .json(&serde_json::json!({"error":"service unavailable"}))
            } else {
                web::HttpResponse::Unauthorized()
                    .json(&serde_json::json!({"error":"unauthorized"}))
            })
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
///   control itself recorded at enrolment, because no peer document has ever
///   carried one: a worker draws its instance keypair in memory at boot and
///   only the public half ever leaves the process.
///
/// `ServiceTrustBundle::keys_for` is an exact-string lookup, so the second kind
/// resolves nothing in the first source - which is why control cannot simply
/// hand every caller to the boot-time verifier and is why this branch exists.
///
/// The resolution happens HERE, before any verification, and hands verification
/// a bundle. It is deliberately NOT a resolver seam a verifier calls back into:
/// `zeroship-core` holds inter-service wire types and would have to grow a
/// database-shaped trait for that, and it would buy nothing, because the role
/// and the instance are separable at parse time and "is a lookup even needed"
/// is therefore answerable before the first check runs.
async fn verify_service_caller(
    state: &AppState,
    authorization: Option<&str>,
    endpoint: ServiceEndpoint,
) -> Result<ServiceIdentity, AuthError> {
    match presented_instance_issuer(authorization) {
        Some(issuer) => verify_worker_instance(state, authorization, &issuer, endpoint).await,
        None => state.service_auth.verify(authorization, endpoint).await,
    }
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
    use base64::Engine as _;

    let assertion = zeroship_core::auth::extract_bearer(authorization?)?;
    let mut parts = assertion.split('.');
    let (_header, payload, signature) = (parts.next()?, parts.next()?, parts.next()?);
    if signature.is_empty() || parts.next().is_some() {
        return None;
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let issuer = ServiceIssuer::parse(claims.get("iss")?.as_str()?).ok()?;
    issuer.instance().is_some().then_some(issuer)
}

/// Verify an assertion minted by an enrolled worker INSTANCE.
///
/// The bundle handed to verification carries exactly one key under exactly one
/// issuer: this instance's key, under the identifier the caller presented. That
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
/// instance control has not enrolled, or has revoked, holds nothing here - and
/// a fallback would also make an instance key an operator could file in the
/// peer document authenticate, which is a credential carrying no status and so
/// one nothing can revoke.
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
    let public = match crate::worker_enrolment::active_instance_public_key(
        state.control_pg.as_ref(),
        instance,
    )
    .await
    {
        Ok(Some(public)) => public,
        Ok(None) => {
            tracing::warn!(
                issuer = issuer.as_str(),
                "control-internal: no ACTIVE worker instance is registered under this issuer"
            );
            return Err(AuthError::CredentialRejected);
        }
        Err(error) => {
            // The registry is a store this verification must consult, so an
            // unreachable one refuses WITHOUT judging - the same shape as the
            // replay store, and it answers 503 upstream rather than 401.
            tracing::error!(
                %error,
                issuer = issuer.as_str(),
                "control-internal: the worker instance registry could not be read"
            );
            return Err(AuthError::StoreUnavailable);
        }
    };

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
fn check_auth(
    req: &web::HttpRequest,
    state: &AppState,
) -> Option<web::HttpResponse> {
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
                && zeroship_core::auth::constant_time_eq(key, state.control_key.expose_secret()) =>
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

/// Liveness. Constant 200 by design: it answers "this process is running and
/// its event loop is not wedged" and MUST NOT touch a dependency. A liveness
/// probe that fails when Postgres blips gets the container killed for someone
/// else's outage.
pub async fn healthz() -> web::HttpResponse {
    web::HttpResponse::Ok().json(&serde_json::json!({"ok": true}))
}

/// Readiness. The control plane cannot serve a single API call without
/// Postgres, so this probes the SHARED long-lived `control_pg` client with a
/// protocol-level sync - no new connection, no query planning, no table read.
///
/// Bounded, cached and single-flighted by [`ReadinessGate`]; the body carries
/// no DSN, host, or driver error text.
///
/// A process that booted on the credential dev escape is NOT ready, whatever
/// Postgres says, and it short-circuits first so a doomed process does not also
/// generate probe traffic. The body still names nothing: `/readyz` is
/// unauthenticated, and "this control plane runs on the default key" is the
/// sentence an attacker most wants.
pub async fn readyz(
    state: State<Arc<AppState>>,
    gate: State<Arc<ReadinessGate>>,
) -> web::HttpResponse {
    if zeroship_core::config::dev_escape_active() {
        return web::HttpResponse::ServiceUnavailable().json(&serde_json::json!({"ready": false}));
    }
    let ready = gate
        .ready(|| async {
            match state.control_pg.check_connection().await {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(error = %error, "control readiness: postgres unreachable");
                    false
                }
            }
        })
        .await;
    if ready {
        web::HttpResponse::Ok().json(&serde_json::json!({"ready": true}))
    } else {
        web::HttpResponse::ServiceUnavailable().json(&serde_json::json!({"ready": false}))
    }
}

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
    // profile; a shared bearer no longer opens this door, which matters most
    // here because the response body is the app's DECRYPTED environment.
    if let Some(resp) =
        check_service_auth(&req, &state, endpoints::CONTROL_APP_ENV).await
    {
        return resp;
    }
    let Ok(id) = AppId::parse(&app_id) else {
        return web::HttpResponse::BadRequest()
            .json(&serde_json::json!({"error": "bad app_id"}));
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

/// POST /internal/workers/enrol - a worker registers ONE live process.
///
/// Guarded by the same full assertion profile as the privileged reads above,
/// and at a lower rate still: a worker enrols once per boot. The endpoint
/// grant is `CONTROL_WORKER_ENROL`, held by `svc/worker` alone.
///
/// The caller supplies its listening PORT and its instance PUBLIC KEY. It
/// supplies no host, and this handler offers no way to. The one thing this
/// function does that its callee cannot is read `req.peer_addr()` - the address
/// the TRANSPORT observed - and hand it over. There is deliberately no fallback
/// when the transport exposes none: that fallback is the vulnerability, and
/// `crates/zeroship-control/src/worker_enrolment.rs` says what it would cost.
///
/// The `X-Forwarded-For` machinery this crate carries for audit and rate-limit
/// identity ([`crate::http_util::source_ip`]) is NOT reachable from here, on
/// purpose: a forwarded header is a caller-supplied host wearing a proxy's
/// clothes. Control's own `trust_proxy` instead REFUSES enrolment outright,
/// because behind a trusted proxy every observed peer is the proxy and the
/// derivation would place every worker at one address.
pub async fn enrol_worker_instance(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    body: web::types::Json<crate::worker_enrolment::WorkerEnrolmentRequest>,
) -> web::HttpResponse {
    if let Some(resp) =
        check_service_auth(&req, &state, endpoints::CONTROL_WORKER_ENROL).await
    {
        return resp;
    }
    crate::worker_enrolment::enrol(&state, req.peer_addr(), body.into_inner()).await
}

pub async fn get_versions(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
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
    if let Some(resp) = check_service_auth(&req, &state, endpoints::CONTROL_APP).await {
        return resp;
    }
    let uid = match AppId::parse(&app_id) {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid app id"}))
        }
    };
    match state.registry.get_versions().await {
        Ok(versions) => match versions.get(&uid) {
            Some(info) => web::HttpResponse::Ok().json(info),
            None => web::HttpResponse::NotFound()
                .json(&serde_json::json!({"error":"app not found"})),
        },
        Err(e) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({"error": e.to_string()})),
    }
}

pub async fn get_routes(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
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
