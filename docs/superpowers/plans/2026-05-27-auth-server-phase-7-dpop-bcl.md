# Auth server — Phase 7 implementation plan: DPoP-ready + Back-channel logout

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan.

**Goal:** Ship the two items the proposal §18 explicitly deferred at v1: back-channel logout consumption at the gateway and control RPs, and a DPoP-ready proof-verification interface at the gateway. This phase goes beyond the proposal's planned scope; both items are real value-adds for multi-app SSO and client-side token-theft mitigation.

**Architecture in brief:**

- **Back-channel logout** — hydra POSTs a `logout_token` JWT to each RP's `backchannel_logout_uri` when a user signs out. The gateway exposes `/oidc/backchannel-logout` (one endpoint per host), verifies the token against hydra's JWKS, and revokes the user's per-app session(s). Same shape on control plane.

- **DPoP-ready** — RFC 9449 proof JWTs in the `DPoP:` request header. The gateway parses+verifies these proofs (signature against embedded JWK, `htu`/`htm`/`iat` freshness, `jti` replay protection). Full DPoP token-binding requires `cnf.jkt` on the access token, which hydra doesn't issue today — Phase 7 ships the proof-verification surface; the binding step is deferred until either (a) we fork hydra to issue `cnf.jkt` or (b) we route through a gateway-side token issuer. The verifier surface is the building block.

**Tech Stack:** Continuing — compio, ntex, cyper, compio-postgres, jsonwebtoken, aws-lc-rs (already used for SNS sig verify). DPoP uses jsonwebtoken's JWK support for verifying with embedded keys.

**References:**
- RFC 9449 (DPoP) — focus on §4 (proof JWT), §6 (validation), §11 (security considerations)
- OIDC Back-Channel Logout 1.0 (https://openid.net/specs/openid-connect-backchannel-1_0.html)
- Proposal §18 deferred-items list
- `core::oidc_verify` (Phase 3) — reuse the JwksCache pattern for logout_token verification

**Pre-launch posture:** still no back-compat shims. Existing flows continue working; these are additive.

**Starting point:** worktree tip post-P6.5 (`0b0a8197`). 452 workspace tests green. `auth-phase-6` tag remains the proposal-completion marker; Phase 7 is post-launch hardening.

---

## Phase 7 unit list

| # | Unit | Files | Time |
|---|---|---|---|
| U1 | Back-channel logout primitive + gateway integration | core/logout_token.rs + gateway/oidc_rp.rs + gateway/sessions.rs | 60 min |
| U2 | Back-channel logout on control plane | control/oidc_rp.rs + control/console_sessions | 30 min |
| U3 | DPoP proof JWT verifier (signature + iat + htu/htm) | core/dpop.rs | 60 min |
| U4 | DPoP jti replay protection (in-process LRU; PG fallback noted) | core/dpop.rs (extend) | 30 min |
| U5 | DPoP-ready gateway integration (advertise + accept the header) | gateway/dispatch.rs + gateway/oidc_rp.rs | 50 min |
| U6 | Phase 7 close-out + `auth-phase-7` tag | – | 10 min |

Total: ~4h, ~10 commits.

---

# Unit U1 · Back-channel logout — primitive + gateway

## Background

When a user signs out via `/oauth2/sessions/logout`, hydra POSTs to every RP's registered `backchannel_logout_uri`:

```
POST <backchannel_logout_uri>
Content-Type: application/x-www-form-urlencoded
Cache-Control: no-store

logout_token=<JWT>
```

The `logout_token` JWT claims (per spec):
- `iss`: hydra's issuer (must match expected)
- `aud`: RP's `client_id` (must match)
- `iat`: time of issue
- `jti`: unique id (replay defense)
- `events`: `{ "http://schemas.openid.net/event/backchannel-logout": {} }` — required marker
- `sub` and/or `sid`: identifies the user (sub) and/or session (sid)

The token **MUST NOT** contain a `nonce` claim (the spec forbids it).

## Task U1.1 · `core::logout_token` verifier

File: `crates/core/src/logout_token.rs`.

```rust
//! OIDC Back-Channel Logout 1.0 `logout_token` JWT verifier.

use serde::{Deserialize, Serialize};
use crate::oidc_verify::{JwksCache, OidcError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogoutToken {
    pub iss: String,
    pub aud: serde_json::Value,   // string or array
    pub iat: i64,
    pub jti: String,
    pub events: std::collections::BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub sub: Option<String>,
    #[serde(default)]
    pub sid: Option<String>,
    /// MUST NOT be present per OIDC BCL spec; if found, reject.
    #[serde(default)]
    pub nonce: Option<String>,
}

const BCL_EVENT: &str = "http://schemas.openid.net/event/backchannel-logout";

/// Verify a logout_token. On success returns the parsed claims.
///
/// # Errors
///
/// Various `OidcError` + this module's `LogoutError` variants.
pub async fn verify(
    cache: &JwksCache,
    token: &str,
    expected_iss: &str,
    expected_aud: &str,
) -> Result<LogoutToken, LogoutError> {
    // Reuse oidc_verify's signature + iss + aud verification machinery.
    let claims_raw = crate::oidc_verify::verify_id_token(cache, token, expected_iss, expected_aud, None)
        .await
        .map_err(LogoutError::Verify)?;
    // claims_raw is a TokenClaims; re-parse into LogoutToken via serde_json round-trip on `other`.
    // Simpler: parse the JWT payload directly and verify it ourselves.
    // ... (see below for the simpler path)

    // ALTERNATIVE: do the JWT decode locally rather than via verify_id_token (which is ID-token-specific).
    // verify_id_token already does header→kid→key→verify. We need almost the same but with different
    // claim validation (no nonce check, events check, sub/sid presence).
    todo!("see implementation note below")
}

#[derive(Debug, thiserror::Error)]
pub enum LogoutError {
    #[error("verify: {0}")]
    Verify(OidcError),
    #[error("nonce claim present (forbidden by spec)")]
    NonceForbidden,
    #[error("events claim missing the backchannel-logout marker")]
    EventsMissing,
    #[error("sub and sid both missing")]
    SubjectMissing,
    #[error("iat too old (>5 min)")]
    Stale,
}
```

**Implementation note:** the cleanest path is to factor `core::oidc_verify` to expose a more general `verify_jwt_signature<T>(cache, token, expected_iss, expected_aud, nonce: Option<&str>) -> Result<T, OidcError>` and then have `verify_id_token` be a thin wrapper. `logout_token::verify` becomes another thin wrapper that calls the generic verifier with `nonce = None` and then runs the BCL-specific claim checks (events marker, nonce-must-be-absent, sub-or-sid, iat freshness).

If refactoring `core::oidc_verify` is too invasive, just duplicate the decode-header → find-key → verify path in `core::logout_token`. ~30 LOC.

**Tests** (unit-only; no live infra needed for the verifier):
- Build a synthetic JWT signed with a test EdDSA key, verify happy path
- Reject when iat is >5min old
- Reject when events claim is missing the BCL marker
- Reject when both sub and sid are absent
- Reject when nonce is present
- Reject when aud doesn't match

Commit: `core: logout_token — OIDC BCL verifier (events check + nonce-must-be-absent + sub/sid)`

## Task U1.2 · Gateway integration

File: `crates/gateway/src/ui/backchannel_logout.rs` (new) or add a handler to existing routes.

```rust
//! POST /oidc/backchannel-logout — receives hydra's logout_token JWT,
//! verifies it, revokes the user's app session(s).

use ntex::http::HttpResponse;
use ntex::web::{self, types::{Form, State}};
use std::sync::Arc;
use serde::Deserialize;
use zeroship_core::logout_token::{self, LogoutToken};
use zeroship_core::oidc_verify::JwksCache;

use crate::sessions;
use crate::GateState;

#[derive(Debug, Deserialize)]
pub struct LogoutForm {
    pub logout_token: String,
}

pub async fn handle(
    form: Form<LogoutForm>,
    state: State<Arc<GateState>>,
) -> HttpResponse {
    let issuer = format!("{}/", state.config.auth_public.trim_end_matches('/'));
    let aud = state.oidc_rp.client_id.clone();   // for the gateway, client_id is "gateway"

    let token = match logout_token::verify(&state.oidc_rp.jwks, &form.logout_token, &issuer, &aud).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "backchannel_logout: verify failed");
            return HttpResponse::BadRequest()
                .header("cache-control", "no-store")
                .body("invalid logout_token");
        }
    };

    // Revoke sessions. Prefer sid (per-session) if present; fall back to sub (all sessions for user).
    if let Some(db) = state.db.as_ref() {
        match (&token.sid, &token.sub) {
            (Some(sid), _) => {
                // Revoke the specific session.
                // gateway::sessions doesn't store hydra's sid directly; we keep our own session ids.
                // Map via: hydra's sid → our gateway_sessions row via the user_id (sub) + recent
                // For Phase 7 minimum: revoke ALL sessions for the sub.
                if let Some(sub) = &token.sub {
                    let _ = sessions::revoke_all_for_user(db, sub).await;
                }
            }
            (None, Some(sub)) => {
                let _ = sessions::revoke_all_for_user(db, sub).await;
            }
            _ => {} // SubjectMissing was caught in verify; shouldn't reach here
        }
    }

    HttpResponse::Ok()
        .header("cache-control", "no-store")
        .finish()
}
```

Add `sessions::revoke_all_for_user(db, user_id)` to `crates/gateway/src/sessions.rs`:

```rust
pub async fn revoke_all_for_user(db: &Client, user_id: &str) -> Result<u64> {
    let affected = db.execute(
        "UPDATE auth.gateway_sessions SET revoked_at = NOW() \
         WHERE user_id = $1 AND revoked_at IS NULL",
        &[&user_id],
    ).await.map_err(|e| GatewayError::Db(format!("revoke_all_for_user: {e}")))?;
    Ok(affected)
}
```

Register route at `/oidc/backchannel-logout` in the gateway's dispatch (always available; not gated on anything). The route MUST be reachable at `https://<gateway-host>/oidc/backchannel-logout` because that's what gets registered as `backchannel_logout_uri` on each OIDC client in hydra.

**Update `ops/auth-clients.example.toml`** to add `backchannel_logout_uri = "https://api.zeroship.ai/oidc/backchannel"` to the `gateway` client config (and similar for console).

Commit: `gateway: /oidc/backchannel-logout — verify logout_token + revoke gateway_sessions for the subject`

---

# Unit U2 · Back-channel logout on control plane

Same pattern as U1 but for `console_sessions` instead of `gateway_sessions`.

Files:
- Create `crates/control/src/oidc_rp.rs::backchannel_logout` handler
- Add `console_sessions::revoke_all_for_user`
- Register `POST /oidc/backchannel-logout` route
- Update `ops/auth-clients.example.toml` console client with the backchannel URI

Commit: `control: /oidc/backchannel-logout — verify logout_token + revoke console_sessions`

---

# Unit U3 · DPoP proof JWT verifier

## RFC 9449 essentials

DPoP proofs are JWTs in the `DPoP:` request header. Required claims:
- Header: `typ = "dpop+jwt"`, `alg ∈ {ES256, ES384, ES512, RS256, RS384, RS512, PS256, EdDSA}`, `jwk` (embedded public key — NOT `kid`)
- Body: `jti` (unique), `htm` (HTTP method, e.g. "GET"), `htu` (HTTP target URI, scheme://host/path with query stripped), `iat` (seconds)

For tokens bound via `cnf.jkt`: the proof JWT's `jwk` thumbprint must equal the access token's `cnf.jkt` claim.

Optional claims:
- `ath` — base64url(SHA-256(access_token)). Required when the request carries an access token.
- `nonce` — DPoP-Nonce challenge from server (Phase 7+ may not implement).

## File

- Create `crates/core/src/dpop.rs`

```rust
//! DPoP (RFC 9449) proof JWT verifier.
//!
//! The verifier accepts a DPoP proof from a request header and validates:
//!   1. JWT header: typ=dpop+jwt, alg in the whitelist, jwk present
//!   2. Signature: verify with the embedded jwk
//!   3. Claims: htm matches the actual request method; htu matches the
//!      actual request URI (scheme + host + path, no query/fragment);
//!      iat within the freshness window (±60s); jti uniqueness (caller-supplied)
//!   4. Optional: ath matches SHA-256(access_token) if access token present
//!
//! Returns the JWK thumbprint of the proof's signing key. Callers can
//! compare this thumbprint against an access-token's `cnf.jkt` claim to
//! verify token binding.

use serde::{Deserialize, Serialize};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

#[derive(Debug, thiserror::Error)]
pub enum DpopError {
    #[error("missing DPoP header")]
    Missing,
    #[error("invalid JWT shape")]
    BadShape,
    #[error("wrong typ (expected dpop+jwt)")]
    WrongTyp,
    #[error("unsupported alg")]
    UnsupportedAlg,
    #[error("missing or invalid jwk")]
    BadJwk,
    #[error("signature verify failed")]
    SigInvalid,
    #[error("htm mismatch (expected {expected}, got {got})")]
    HtmMismatch { expected: String, got: String },
    #[error("htu mismatch")]
    HtuMismatch,
    #[error("iat outside freshness window")]
    Stale,
    #[error("ath mismatch")]
    AthMismatch,
    #[error("invalid base64")]
    Base64,
}

#[derive(Debug, Clone)]
pub struct VerifiedDpop {
    pub jkt: String,           // base64url(SHA-256(canonical JWK)) — the thumbprint
    pub jti: String,
}

#[derive(Debug, Deserialize)]
struct DpopHeader {
    typ: String,
    alg: String,
    jwk: serde_json::Value,    // raw JWK
}

#[derive(Debug, Deserialize)]
struct DpopBody {
    jti: String,
    htm: String,
    htu: String,
    iat: i64,
    #[serde(default)]
    ath: Option<String>,
}

const ALLOWED_ALGS: &[&str] = &["ES256", "ES384", "ES512", "RS256", "RS384", "PS256", "EdDSA"];

/// Verify a DPoP proof.
///
/// # Errors
///
/// Variants of `DpopError`.
pub fn verify(
    proof: &str,
    expected_method: &str,
    expected_uri: &str,
    access_token: Option<&str>,
    now_secs: i64,
) -> Result<VerifiedDpop, DpopError> {
    // 1. Split JWT.
    let parts: Vec<&str> = proof.split('.').collect();
    if parts.len() != 3 { return Err(DpopError::BadShape); }

    let header_bytes = URL_SAFE_NO_PAD.decode(parts[0]).map_err(|_| DpopError::Base64)?;
    let header: DpopHeader = serde_json::from_slice(&header_bytes).map_err(|_| DpopError::BadShape)?;

    if header.typ != "dpop+jwt" { return Err(DpopError::WrongTyp); }
    if !ALLOWED_ALGS.contains(&header.alg.as_str()) { return Err(DpopError::UnsupportedAlg); }

    let body_bytes = URL_SAFE_NO_PAD.decode(parts[1]).map_err(|_| DpopError::Base64)?;
    let body: DpopBody = serde_json::from_slice(&body_bytes).map_err(|_| DpopError::BadShape)?;

    // 2. Verify signature using the embedded JWK.
    let key = decoding_key_from_jwk(&header.jwk, &header.alg)?;
    let mut validation = jsonwebtoken::Validation::new(map_alg(&header.alg)?);
    validation.required_spec_claims.clear();
    validation.validate_exp = false;
    let _data: jsonwebtoken::TokenData<serde_json::Value> = jsonwebtoken::decode(proof, &key, &validation)
        .map_err(|_| DpopError::SigInvalid)?;

    // 3. Claim checks.
    if body.htm.to_uppercase() != expected_method.to_uppercase() {
        return Err(DpopError::HtmMismatch { expected: expected_method.into(), got: body.htm });
    }
    if !htu_matches(&body.htu, expected_uri) {
        return Err(DpopError::HtuMismatch);
    }
    let age = now_secs - body.iat;
    if !(-60..=60).contains(&age) {
        return Err(DpopError::Stale);
    }

    // 4. Optional ath check.
    if let Some(token) = access_token {
        use sha2::{Digest, Sha256};
        let expected_ath = URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()));
        match body.ath {
            Some(provided) if provided == expected_ath => {}
            _ => return Err(DpopError::AthMismatch),
        }
    }

    // 5. Compute JWK thumbprint (RFC 7638).
    let jkt = jwk_thumbprint(&header.jwk)?;

    Ok(VerifiedDpop { jkt, jti: body.jti })
}

fn htu_matches(claimed: &str, actual: &str) -> bool {
    // Both should be normalized: scheme://host[:port]/path (no query, no fragment).
    let normalize = |u: &str| u.split('?').next().unwrap_or(u).split('#').next().unwrap_or(u).to_string();
    normalize(claimed) == normalize(actual)
}

fn map_alg(alg: &str) -> Result<jsonwebtoken::Algorithm, DpopError> {
    use jsonwebtoken::Algorithm;
    match alg {
        "ES256" => Ok(Algorithm::ES256),
        "ES384" => Ok(Algorithm::ES384),
        "RS256" => Ok(Algorithm::RS256),
        "RS384" => Ok(Algorithm::RS384),
        "PS256" => Ok(Algorithm::PS256),
        "EdDSA" => Ok(Algorithm::EdDSA),
        _ => Err(DpopError::UnsupportedAlg),
    }
}

fn decoding_key_from_jwk(jwk: &serde_json::Value, alg: &str) -> Result<jsonwebtoken::DecodingKey, DpopError> {
    use jsonwebtoken::DecodingKey;
    match jwk.get("kty").and_then(|v| v.as_str()) {
        Some("RSA") => {
            let n = jwk.get("n").and_then(|v| v.as_str()).ok_or(DpopError::BadJwk)?;
            let e = jwk.get("e").and_then(|v| v.as_str()).ok_or(DpopError::BadJwk)?;
            DecodingKey::from_rsa_components(n, e).map_err(|_| DpopError::BadJwk)
        }
        Some("EC") => {
            let x = jwk.get("x").and_then(|v| v.as_str()).ok_or(DpopError::BadJwk)?;
            let y = jwk.get("y").and_then(|v| v.as_str()).ok_or(DpopError::BadJwk)?;
            DecodingKey::from_ec_components(x, y).map_err(|_| DpopError::BadJwk)
        }
        Some("OKP") => {
            let x = jwk.get("x").and_then(|v| v.as_str()).ok_or(DpopError::BadJwk)?;
            DecodingKey::from_ed_components(x).map_err(|_| DpopError::BadJwk)
        }
        _ => Err(DpopError::BadJwk),
    }
}

fn jwk_thumbprint(jwk: &serde_json::Value) -> Result<String, DpopError> {
    // RFC 7638 thumbprint: canonical-JSON of the required members for the
    // key type, then SHA-256, then base64url no-pad.
    use sha2::{Digest, Sha256};

    let canonical = match jwk.get("kty").and_then(|v| v.as_str()) {
        Some("RSA") => {
            let n = jwk.get("n").and_then(|v| v.as_str()).ok_or(DpopError::BadJwk)?;
            let e = jwk.get("e").and_then(|v| v.as_str()).ok_or(DpopError::BadJwk)?;
            format!(r#"{{"e":"{e}","kty":"RSA","n":"{n}"}}"#)
        }
        Some("EC") => {
            let crv = jwk.get("crv").and_then(|v| v.as_str()).ok_or(DpopError::BadJwk)?;
            let x = jwk.get("x").and_then(|v| v.as_str()).ok_or(DpopError::BadJwk)?;
            let y = jwk.get("y").and_then(|v| v.as_str()).ok_or(DpopError::BadJwk)?;
            format!(r#"{{"crv":"{crv}","kty":"EC","x":"{x}","y":"{y}"}}"#)
        }
        Some("OKP") => {
            let crv = jwk.get("crv").and_then(|v| v.as_str()).ok_or(DpopError::BadJwk)?;
            let x = jwk.get("x").and_then(|v| v.as_str()).ok_or(DpopError::BadJwk)?;
            format!(r#"{{"crv":"{crv}","kty":"OKP","x":"{x}"}}"#)
        }
        _ => return Err(DpopError::BadJwk),
    };

    let digest = Sha256::digest(canonical.as_bytes());
    Ok(URL_SAFE_NO_PAD.encode(digest))
}
```

Tests:
- Build a synthetic DPoP proof with `ed25519-dalek`, verify happy path → returns the JWK thumbprint
- Reject wrong typ
- Reject stale iat (>60s)
- Reject htm mismatch
- Reject htu mismatch
- Verify ath matches SHA-256(token)
- Reject ath mismatch

Commit: `core: dpop — RFC 9449 proof JWT verifier (signature, htu/htm/iat, ath, jkt thumbprint)`

---

# Unit U4 · DPoP jti replay protection

`jti` must not repeat within the freshness window. Maintain a bounded in-memory set in the gateway:

File: extend `crates/core/src/dpop.rs` with:

```rust
use std::collections::HashMap;
use std::sync::Mutex;

pub struct JtiCache {
    inner: Mutex<HashMap<String, i64>>,  // jti → expiry_secs
    max_entries: usize,
}

impl JtiCache {
    pub fn new(max_entries: usize) -> Self { ... }

    /// Insert a jti. Returns true if it's new; false if it was already present.
    pub fn insert(&self, jti: &str, now_secs: i64, ttl_secs: i64) -> bool {
        let mut g = self.inner.lock().unwrap();
        // Evict expired entries.
        g.retain(|_, exp| *exp > now_secs);
        // If at capacity, drop one at random (or oldest).
        if g.len() >= self.max_entries {
            if let Some(k) = g.keys().next().cloned() { g.remove(&k); }
        }
        // Insert if absent.
        if g.contains_key(jti) { return false; }
        g.insert(jti.to_string(), now_secs + ttl_secs);
        true
    }
}
```

PG-backed alternative (note in module docs):
- For multi-instance gateway deploys, the in-process cache is per-instance — replay across instances is possible
- Future hardening: `auth.dpop_jti` table with `(jti TEXT PRIMARY KEY, expires_at TIMESTAMPTZ)` swept by the same cron pattern as audit_retention

For Phase 7 v1, in-process is sufficient — note the limitation in module docs.

Tests:
- Insert returns true on first call, false on second
- Eviction at capacity works
- Expired entries get removed on insert

Commit: `core: dpop — JtiCache for replay protection (in-process LRU; PG-backed future hardening)`

---

# Unit U5 · DPoP-ready gateway integration

The gateway's `resolve_auth_or_redirect` already validates the `__Host-zs_app_session` cookie. DPoP-ready means:
1. If the request carries `Authorization: DPoP <token>` (RFC 9449 token-type), require a `DPoP:` proof header
2. Verify the proof, jti-dedup it, check `ath` matches the access token
3. If OK: trust the request even without our session cookie

Phase 7 v1 keeps DPoP **optional** — clients that don't send DPoP-Auth keep working with cookie-based sessions. Clients that DO send it bypass session-cookie validation in favor of DPoP proof.

Note: hydra's tokens don't carry `cnf.jkt`. So Phase 7's DPoP verifies the proof JWT's signature + freshness, but can't verify token binding. The thumbprint (`jkt`) is returned for inspection; calling code that wants binding enforces `proof.jkt == access_token.cnf.jkt` itself — which requires us to issue our own bound tokens, a Phase 8+ item.

File: extend `crates/gateway/src/router/dispatch.rs::resolve_auth_or_redirect`:

```rust
// At the top of the function, before session-cookie validation:
if let Some(auth_header) = req.headers().get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok()) {
    if let Some(token) = auth_header.strip_prefix("DPoP ") {
        // The client is presenting a DPoP-protected access token.
        return resolve_via_dpop(req, state, token).await;
    }
}
// ... existing cookie-validation path
```

`resolve_via_dpop`:
1. Extract DPoP header from request
2. Build expected_uri from req scheme+host+path (no query)
3. `dpop::verify(proof, method, uri, Some(token), now_secs)`
4. Check JtiCache for replay
5. Resolve the access token via hydra `/oauth2/introspect` to get the user identity
6. Build the `ZeroShip-User` header from the introspection result
7. Return `AuthOutcome::Authed { header }`

Add `dpop_jti_cache: Arc<JtiCache>` to `GateState`, initialized in `main.rs`.

Advertise DPoP in the OIDC discovery doc — actually, hydra owns the discovery doc, and it already advertises `dpop_signing_alg_values_supported` per Phase 1's `ops/hydra.yaml`. No change needed there.

Commit: `gateway: dispatch — accept DPoP-bound access tokens (optional path; binding enforcement deferred)`

---

# Unit U6 · Phase 7 close-out

- Workspace build + clippy
- Test sweep (auth + gateway + control + core all green)
- Milestone empty-commit:

```
auth: Phase 7 complete — back-channel logout + DPoP-ready interface

Goes beyond the proposal's planned scope (§18 deferred items). Both
features land as additive surfaces; existing flows unaffected.

Back-channel logout: gateway + control plane both expose
/oidc/backchannel-logout. hydra POSTs a verified logout_token JWT
when a user signs out anywhere; the receiving RP revokes its
{gateway,console}_sessions for the subject. Sign-out at one app
now propagates to other apps in the same SSO realm.

DPoP-ready: core::dpop verifies RFC 9449 proof JWTs (signature
against embedded JWK, htu/htm/iat freshness, optional ath against
the access token, JWK thumbprint). core::dpop::JtiCache provides
in-process replay protection. Gateway accepts DPoP-Auth headers
optionally; clients that don't send DPoP keep working with
cookie sessions.

Full DPoP token-binding (cnf.jkt on access tokens) requires
gateway-side token issuance, deferred to Phase 8+.
```

Tag `auth-phase-7`.

---

# Future work (post-Phase-7)

- **Phase 8** — Gateway-side token issuance (or hydra fork) for full DPoP `cnf.jkt` binding
- **External security review** — outstanding from the start
- **Multi-instance load test** — current load test is single-instance
- **PG-backed JtiCache** for multi-instance DPoP replay protection

---

# Self-review

**Spec coverage** — RFC 9449 §4-§6 (DPoP); OIDC BCL 1.0 §2.4-§2.6 (logout_token verification). Both addressed.

**Placeholder scan** — Two implementation outlines (DPoP key extraction from JWK; ath comparison) are sketchy. Implementer fills in.

**Scope** — Phase 7 ships verifier surfaces + back-channel logout. Full DPoP token-binding is post-Phase-7 because it requires hydra-side or gateway-side changes that are too invasive for a single phase.

# Execution handoff

Plan saved to `docs/superpowers/plans/2026-05-27-auth-server-phase-7-dpop-bcl.md`.
Execute via subagent-driven-development.
