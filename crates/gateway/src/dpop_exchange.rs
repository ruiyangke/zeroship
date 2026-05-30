//! `POST /__zs/auth/dpop-exchange` — issue a wrapper token bound to
//! the client's `DPoP` key. Phase 8 U3.
//!
//! Wire shape:
//! ```text
//! POST /__zs/auth/dpop-exchange
//! Authorization: Bearer <hydra_access_token>
//! DPoP: <RFC 9449 proof JWT>
//!
//! → 200 OK + Cache-Control: no-store
//!   { "wrapper_token": "<JWT>", "expires_in": 3600, "token_type": "DPoP" }
//! ```
//!
//! Steps (see Phase 8 plan §U3):
//!
//! 1. Issuer present (else 503 — gateway booted without
//!    `--signing-key-file`).
//! 2. Parse `Authorization: Bearer <hydra_token>` (else 400).
//! 3. Parse `DPoP: <proof>` header (else 400).
//! 4. Build the expected URI for the proof's `htu` check from the
//!    request's `Host` header. Scheme is `https` unless
//!    `config.insecure_dev` is set.
//! 5. Verify the `DPoP` proof via [`zeroship_core::dpop::verify`]. The
//!    proof's `ath` claim is checked against the hydra token's
//!    SHA-256 here — three-way binding starts at this verification
//!    step (else 401).
//! 6. Insert `jti` into the in-process replay cache (else 401 on
//!    replay).
//! 7. Introspect the hydra access token (else 401 if inactive /
//!    introspection failed).
//! 8. Mint a wrapper token whose `cnf.jkt` is the `DPoP` key's
//!    thumbprint and whose `aud` is the request `Host`. The resulting
//!    binding is `(sub, app, key)` — the dispatcher (U4) enforces all
//!    three on the inbound side.
//!
//! Every response (success or failure) carries `Cache-Control:
//! no-store` so intermediaries don't retain wrapper tokens.

use std::sync::Arc;

use ntex::http::header::{AUTHORIZATION, HOST};
use ntex::web::{types::State, HttpRequest, HttpResponse};
use serde_json::json;
use uuid::Uuid;

use crate::auth_token::{relay_alias_for, resolve_route};
use crate::GateState;

/// Cache-Control value applied to every response from this handler.
const CACHE_NO_STORE: &str = "no-store";

/// Replay-cache TTL for the `DPoP` proof's `jti`. RFC 9449 §11.1 does
/// not pin a number; 120 seconds is comfortably larger than the
/// proof's wall-clock acceptance window (60s in the verifier) so an
/// attacker who captures a proof can't replay it once the original
/// has been admitted.
const JTI_TTL_SECS: i64 = 120;

/// Wrapper-token lifetime for the DPoP-exchange path, in seconds. Used
/// both as the `WrapperMint::exp_secs` the Issuer stamps into `exp` and
/// as the `expires_in` value echoed in the response envelope — bundling
/// them here keeps the JWT body and the envelope from disagreeing about
/// the token's lifetime. (The browser plain-Bearer path mints a shorter
/// 600 s wrapper; that lives in its own handler.)
const WRAPPER_EXPIRES_IN_SECS: i64 = 3600;

/// Issue a wrapper token bound to the client's `DPoP` key.
///
/// `future_not_send` mirrors the existing handlers
/// (`backchannel_logout::handle`) — `HttpRequest` / `State` aren't
/// `Send`, but ntex doesn't require the future to be `Send` either.
/// `too_many_lines` is fine here: the linear step-by-step structure
/// of the handler is the documentation; chopping it into helpers
/// would obscure rather than clarify.
#[allow(clippy::future_not_send, clippy::too_many_lines)]
pub async fn handle(req: HttpRequest, state: State<Arc<GateState>>) -> HttpResponse {
    // 1. Issuer must be configured. When the gateway booted without
    //    `--signing-key-file` there's no way to mint a token, so we
    //    serve 503 rather than silently substituting an unsigned
    //    placeholder.
    let Some(issuer) = state.wrapper_issuer.as_ref() else {
        return HttpResponse::ServiceUnavailable()
            .header("cache-control", CACHE_NO_STORE)
            .json(&json!({"error": "dpop_binding_not_configured"}));
    };

    // 2. Parse `Authorization: Bearer <hydra_token>`. The header may
    //    be absent (some clients forget) or hold a non-Bearer scheme
    //    (e.g. Basic from a misconfigured proxy) — both surface as
    //    400 with the same error code so the caller can correct.
    let auth = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let hydra_token = match auth.strip_prefix("Bearer ") {
        Some(t) if !t.is_empty() => t,
        _ => {
            return HttpResponse::BadRequest()
                .header("cache-control", CACHE_NO_STORE)
                .json(&json!({
                    "error": "missing_bearer",
                    "detail": "Authorization: Bearer <hydra_token> required",
                }));
        }
    };

    // 3. Parse the `DPoP` proof header. RFC 9449 §4 says exactly one
    //    `DPoP` header MUST be present; if it's missing we 400 (this
    //    is a client error, not an auth failure).
    let proof = match req.headers().get("dpop").and_then(|v| v.to_str().ok()) {
        Some(p) if !p.is_empty() => p,
        _ => {
            return HttpResponse::BadRequest()
                .header("cache-control", CACHE_NO_STORE)
                .json(&json!({
                    "error": "missing_dpop",
                    "detail": "DPoP header required",
                }));
        }
    };

    // 4. Build the expected URI for the proof's htu/htm check. Per
    //    RFC 9449 §4.3 the htu MUST match the actual request URI;
    //    we use the request `Host` header as-is (which is what the
    //    client sees too). Scheme: https unless the gateway is in
    //    dev mode (`config.insecure_dev`).
    let host = req
        .headers()
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let scheme = if state.config.insecure_dev { "http" } else { "https" };
    let expected_uri = format!("{scheme}://{host}/__zs/auth/dpop-exchange");

    // System clock as UNIX-seconds. A monotonic clock is wrong here
    // — DPoP is wall-clock-bound (RFC 9449 §11.1).
    let now: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
        .unwrap_or(0);

    // 5. Verify the DPoP proof. `ath = base64url(SHA-256(hydra_token))`
    //    must match — that's the proof-of-possession tying the
    //    `DPoP` proof to the access token in this same request.
    let verified = match zeroship_core::dpop::verify(
        proof,
        "POST",
        &expected_uri,
        Some(hydra_token),
        now,
    ) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "dpop-exchange: proof verification failed");
            return HttpResponse::Unauthorized()
                .header("cache-control", CACHE_NO_STORE)
                .json(&json!({
                    "error": "invalid_dpop_proof",
                    "detail": e.to_string(),
                }));
        }
    };

    // 6. jti replay check. Proofs are single-use; a duplicate within
    //    the TTL window is rejected outright.
    match state
        .dpop_jti_cache
        .insert(&verified.jti, now, JTI_TTL_SECS)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(jti = %verified.jti, "dpop-exchange: jti replay");
            return HttpResponse::Unauthorized()
                .header("cache-control", CACHE_NO_STORE)
                .json(&json!({"error": "dpop_jti_replay"}));
        }
        Err(e) => {
            tracing::warn!(error = %e, "dpop-exchange: jti replay check failed");
            return HttpResponse::ServiceUnavailable()
                .header("cache-control", CACHE_NO_STORE)
                .json(&json!({"error": "dpop_jti_cache_unavailable"}));
        }
    }

    // 7. Introspect the hydra token to confirm it's still active and
    //    resolve `sub`/`scope`/`client_id`. A 401 here covers both
    //    "active: false" and "introspection endpoint returned an
    //    error" — the wrapper would be meaningless in either case.
    let intro = match state.oidc_rp.introspect_token(hydra_token).await {
        Ok(i) if i.active => i,
        Ok(_) => {
            return HttpResponse::Unauthorized()
                .header("cache-control", CACHE_NO_STORE)
                .json(&json!({"error": "hydra_token_inactive"}));
        }
        Err(e) => {
            tracing::warn!(error = %e, "dpop-exchange: introspect failed");
            return HttpResponse::Unauthorized()
                .header("cache-control", CACHE_NO_STORE)
                .json(&json!({"error": "introspection_failed"}));
        }
    };

    // 8. Issue. `aud` is the request `Host` — per-app binding. The
    //    dispatcher (U4) compares `claims.aud == request_host` so a
    //    wrapper minted at `myapp.zeroship.ai` can't be replayed
    //    against `other.zeroship.ai`, even if the same DPoP key
    //    accompanies it.
    //
    //    Resolve `sub` from the introspection response here (the Issuer
    //    is now a free-parameter claims-builder that only rejects an
    //    empty `sub`): an inactive/userless token that introspected
    //    without a `sub` can't yield a usable wrapper.
    let aud = host.to_string();
    let Some(sub) = intro.sub.as_deref().filter(|s| !s.is_empty()) else {
        tracing::warn!("dpop-exchange: introspection response missing oauth sub");
        return HttpResponse::Unauthorized()
            .header("cache-control", CACHE_NO_STORE)
            .json(&json!({"error": "missing_oauth_sub"}));
    };

    // Resolve the per-app route (Host → app + oauth_client_id +
    // sector_identifier), mirroring `/token` (Batch A fix 1). The wrapper this
    // handler mints is JS-readable by app code, so its `sub` MUST be the
    // per-app pairwise `pws_…` (§6.2/G4) and its `email` the relay alias (§7) —
    // NEVER the global Hydra UUID / real email. Deriving the `pws_` needs the
    // route's `sector_identifier`, and the relay-alias + wrapper `client_id`
    // claim need the route's `oauth_client_id`; `resolve_route` answers
    // `503 client_not_provisioned` (kept retryable by the SDK) when the host is
    // not a provisioned app or has no OAuth client yet. We re-emit its error
    // body with this handler's `Cache-Control: no-store`.
    let route = match resolve_route(&req, &state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    // The introspected `sub` is the GLOBAL Hydra user UUID. It is stored /
    // checked server-side but NEVER projected into the JS-readable wrapper:
    // parse it as a UUID and project it to the per-app pairwise `pws_…` under
    // the route's `sector_identifier` (§6.2/G4), EXACTLY as `/token` does. An
    // end-user authorization_code token carries a UUID `sub`; a non-UUID `sub`
    // (e.g. a `client_credentials` token with a client-id subject) cannot
    // become a per-app user pseudonym, so reject it rather than ship a wrapper
    // whose `sub` is not a `pws_`.
    let Ok(global_user_id) = Uuid::parse_str(sub) else {
        tracing::warn!(sub = %sub, "dpop-exchange: oauth sub is not a global user UUID");
        return HttpResponse::Unauthorized()
            .header("cache-control", CACHE_NO_STORE)
            .json(&json!({"error": "sub_not_global_user"}));
    };
    let Some(sector) = route.sector_identifier.as_deref() else {
        // No sector_identifier yet ⇒ we cannot derive the pairwise sub. Fail
        // closed rather than ship a wrapper with the global UUID, matching
        // `/token`'s posture (the SDK treats 503 as retryable).
        tracing::warn!(app = %route.app_name, "dpop-exchange: route has no sector_identifier yet");
        return HttpResponse::ServiceUnavailable()
            .header("cache-control", CACHE_NO_STORE)
            .json(&json!({"error": "client_not_provisioned"}));
    };
    // Derive on the CANONICAL UUID string (Batch A M1) so this mint's `pws_`
    // is byte-identical to the `/signout` / control-cascade writers' marker
    // regardless of the inbound `sub` spelling. `derive_pairwise` canonicalizes
    // internally; passing `global_user_id.to_string()` makes the convention
    // explicit at the call site too.
    let pws_sub = zeroship_core::auth::derive_pairwise(
        &state.pairwise_salt,
        &global_user_id.to_string(),
        sector,
    );

    // Email-claim swap (§7): the wrapper carries the per-app relay ALIAS, never
    // the real `intro.email`. Resolved live for `(route.client_id, user)`; no
    // active alias (not yet minted at consent, or revoked) ⇒ empty email (fail
    // closed) — the real address NEVER reaches the DPoP client.
    let Some(db_cfg) = state.db.as_ref() else {
        // No DB ⇒ no alias source. The real `intro.email` must never leak, so
        // fail closed: 503 (the gateway is mis-provisioned for this surface).
        tracing::warn!("dpop-exchange: no database configured for relay-alias swap");
        return HttpResponse::ServiceUnavailable()
            .header("cache-control", CACHE_NO_STORE)
            .json(&json!({"error": "db_unavailable"}));
    };
    let relay_email = relay_alias_for(db_cfg, &route.client_id, global_user_id).await;

    // `wraps` ties the wrapper to the hydra-issued opaque access token
    // without leaking it: SHA-256 the (utf-8) bytes, base64url-encode.
    // The browser plain-Bearer path has no underlying raw token and
    // omits this; the DPoP path always carries it for revocation-by-
    // origin-token.
    let wraps = {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(hydra_token.as_bytes()))
    };

    let mint = crate::wrapper_token::WrapperMint {
        aud: &aud,
        // Per-app pairwise `pws_…` (§6.2/G4) — NOT the global Hydra UUID, so
        // app JS decoding this wrapper cannot read the global user id or
        // correlate the user across apps. The Bearer/DPoP fast-paths enforce
        // this as the self-describing-subject invariant (fix 2).
        sub: &pws_sub,
        scope: intro.scope.as_deref().unwrap_or_default(),
        // Bind the wrapper to the route's per-app `oac_` client (the relay +
        // family-marker key the gateway arms read against), mirroring `/token`.
        client_id: &route.client_id,
        // The envelope's `expires_in` and the JWT `exp` read the same
        // `i64` constant directly, so they cannot disagree (no fallback
        // that could silently substitute a different lifetime).
        exp_secs: WRAPPER_EXPIRES_IN_SECS,
        cnf: Some(verified.jkt.as_str()),
        wraps: Some(wraps.as_str()),
        // The relay alias (or None ⇒ empty), NEVER `intro.email`.
        email: relay_email.as_deref(),
        email_verified: intro.email_verified,
        name: intro.name.as_deref(),
    };
    let wrapper = match issuer.issue(&mint) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "dpop-exchange: issue failed");
            return HttpResponse::InternalServerError()
                .header("cache-control", CACHE_NO_STORE)
                .json(&json!({"error": "issuer_failed"}));
        }
    };

    // Log the per-app `pws_` (NOT the global UUID — it must not appear in logs
    // any more than in the wrapper) + the route's client.
    tracing::info!(
        pws = %pws_sub,
        client_id = %route.client_id,
        aud = %aud,
        jkt = %verified.jkt,
        "dpop-exchange: wrapper token issued"
    );

    HttpResponse::Ok()
        .header("cache-control", CACHE_NO_STORE)
        .json(&json!({
            "wrapper_token": wrapper,
            "expires_in": WRAPPER_EXPIRES_IN_SECS,
            "token_type": "DPoP",
        }))
}
