# Auth server — Phase 8 implementation plan: Full DPoP token binding

**Phase 8 status:** COMPLETE on 2026-05-27 — `auth-phase-8` tag, ~488 workspace tests green. All units U1–U6 landed; the gateway is now a JWT issuer with a `cnf.jkt`-binding wrapper-token flow and the dispatcher enforces the binding for wrapper tokens. The raw-hydra-token DPoP path from P7-U5 is preserved as an unbound fallback (a future `--strict-dpop` flag will disable it).

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development.

**Goal:** Ship real DPoP token binding (`cnf.jkt`) without forking hydra. The gateway becomes a wrapper-token issuer: clients exchange their hydra access token + DPoP proof for a gateway-signed wrapper JWT whose `cnf.jkt` claim binds the token to a specific public key. The gateway then enforces that any subsequent DPoP-Auth request's proof JWT carries a key whose thumbprint matches `cnf.jkt`.

**Honest accounting of costs:**

- **Integration friction.** Existing creator apps (the cookie-based browser flow) don't need to change. But any non-browser client that wants DPoP-bound tokens now has a two-step dance: hydra OIDC → gateway exchange. The proposal §18 accepted the DPoP gap at v1 precisely because of this friction.
- **New trust surface.** The gateway becomes a JWT issuer with its own signing key. Key rotation, storage, leak response — all new operational concerns.
- **Maintenance burden.** Wrapper tokens introduce their own lifecycle separate from hydra's (refresh, revocation cascade). Defer the cascade story to Phase 9+; v1 wrapper tokens are time-limited (1h, same as the hydra token they wrap) and don't have refresh.
- **The win.** Genuine RFC 9449 compliance. Stolen hydra access tokens (or stolen wrapper tokens) can't be used without the corresponding DPoP private key. Defense against client-storage compromise.

**Architecture:**

```
Client                          Gateway                       Hydra
  │                                │                            │
  │  1. OIDC code+PKCE flow ────────────────────────────────────►│
  │  ◄──── hydra access_token (Bearer, no cnf.jkt) ──────────────│
  │                                │                            │
  │  2. POST /__zs/auth/dpop-exchange                            │
  │       Authorization: Bearer <hydra_token>                     │
  │       DPoP: <proof signed by client key>                      │
  │  ────────────────────────────► │                            │
  │                                │ verify proof                │
  │                                │ introspect hydra token ───►│
  │                                │ ◄───── claims, active ──────│
  │                                │ issue wrapper JWT with      │
  │                                │   cnf.jkt = thumbprint(jwk) │
  │  ◄────── wrapper_token ─────── │                            │
  │                                │                            │
  │  3. GET /resource                                            │
  │       Authorization: DPoP <wrapper_token>                    │
  │       DPoP: <proof signed by client key>                     │
  │  ────────────────────────────► │                            │
  │                                │ verify wrapper sig          │
  │                                │ check cnf.jkt == proof.jkt  │
  │                                │ verify proof (htu/htm/ath)  │
  │  ◄────── 200 OK ─────────────  │                            │
```

**References:**
- RFC 9449 §6 (token-binding via cnf.jkt)
- Phase 7's `core::dpop` for the proof verifier
- Phase 7's `gateway::oidc_rp::introspect_token` for the hydra introspect call

**Pre-launch posture:** still no back-compat shims. The dispatch handler P7-U5 added stays unchanged for bearer-cookie clients; the new path is additive.

**Starting point:** worktree tip post-P7-U6 (`2284737e`, `auth-phase-7` tag). 488 workspace tests green.

---

## Phase 8 unit list

| # | Unit | Files | Time |
|---|---|---|---|
| U1 | Gateway signing key (config + key load) | gateway/{main,config,signing}.rs | 30 min |
| U2 | Wrapper-token issuer (sign + claim shape) | gateway/wrapper_token.rs | 50 min |
| U3 | `/__zs/auth/dpop-exchange` endpoint | gateway/dpop_exchange.rs | 50 min |
| U4 | Dispatch verifies wrapper tokens with cnf.jkt check | gateway/router/auth.rs (extend) | 50 min |
| U5 | E2e test: full DPoP-bound flow against live hydra | gateway/tests/dpop_binding_e2e.rs | 60 min |
| U6 | Phase 8 close-out + auth-phase-8 tag | – | 10 min |

Total: ~4h, ~8 commits.

---

## Unit U1 · Gateway signing key

- [x] Landed 2026-05-27 — `9ed2a652 gateway: signing — Ed25519 PKCS#8 key loader + JWK thumbprint helper`.

The gateway needs its own Ed25519 key pair to sign wrapper tokens. Phase 8 v1: load from disk (or env-var) at boot. Future: KMS integration (Phase 9+).

### Files

- Create `crates/gateway/src/signing.rs`
- Modify `crates/gateway/src/main.rs` — load key at boot, add to GateState
- Modify `crates/gateway/src/lib.rs` — GateState gets `signing_key: Arc<SigningKey>`

### Implementation

```rust
//! crates/gateway/src/signing.rs
//!
//! Gateway-issued JWT signing key. Phase 8 v1 loads from disk (PKCS#8 PEM
//! or DER). KMS integration is Phase 9+.

use std::path::Path;
use ed25519_dalek::SigningKey;
use crate::error::{GatewayError, Result};

/// Load an Ed25519 signing key from a PKCS#8 PEM or DER file.
///
/// File format: `openssl genpkey -algorithm ed25519 -out gw.key` produces a
/// PKCS#8 PEM. Either PEM or DER is accepted.
///
/// # Errors
///
/// `GatewayError::Config` on parse failure or wrong key type.
pub fn load_from_path(path: &Path) -> Result<SigningKey> {
    let bytes = std::fs::read(path)
        .map_err(|e| GatewayError::Config(format!("read signing key {path:?}: {e}")))?;

    // Try PEM first; fall back to DER.
    if let Ok(s) = std::str::from_utf8(&bytes) {
        if s.contains("-----BEGIN PRIVATE KEY-----") {
            return parse_pkcs8_pem(s);
        }
    }
    parse_pkcs8_der(&bytes)
}

fn parse_pkcs8_pem(pem: &str) -> Result<SigningKey> {
    use ed25519_dalek::pkcs8::DecodePrivateKey;
    SigningKey::from_pkcs8_pem(pem)
        .map_err(|e| GatewayError::Config(format!("Ed25519 PKCS#8 PEM: {e}")))
}

fn parse_pkcs8_der(der: &[u8]) -> Result<SigningKey> {
    use ed25519_dalek::pkcs8::DecodePrivateKey;
    SigningKey::from_pkcs8_der(der)
        .map_err(|e| GatewayError::Config(format!("Ed25519 PKCS#8 DER: {e}")))
}

/// Compute the JWK thumbprint (RFC 7638) for the signing key's public half.
/// This is the `kid` we emit in wrapper-token headers.
pub fn jwk_thumbprint(key: &SigningKey) -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let public = key.verifying_key();
    let x = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(public.to_bytes());
    // Canonical JSON for OKP/Ed25519: {"crv":"Ed25519","kty":"OKP","x":"<x>"}
    let canonical = format!(r#"{{"crv":"Ed25519","kty":"OKP","x":"{x}"}}"#);
    let digest = Sha256::digest(canonical.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}
```

### GatewayConfig field

```rust
#[arg(long, env = "GATEWAY_SIGNING_KEY_FILE")]
pub signing_key_file: Option<String>,
```

If unset and `--dpop-binding` (a new flag, default off) is enabled, fatal error at boot.

### main.rs

```rust
let signing_key: Option<Arc<SigningKey>> = match &cfg.signing_key_file {
    Some(path) => {
        let key = signing::load_from_path(Path::new(path))?;
        tracing::info!(kid = %signing::jwk_thumbprint(&key), "gateway signing key loaded");
        Some(Arc::new(key))
    }
    None => {
        tracing::warn!("gateway signing key not configured; /__zs/auth/dpop-exchange will 503");
        None
    }
};
```

Thread `Option<Arc<SigningKey>>` into GateState.

### Tests

Unit-test `jwk_thumbprint` is stable across calls with the same key; differs across different keys. (Mirror the DPoP thumbprint stability test.)

### Commit

```
gateway: signing — Ed25519 PKCS#8 key loader + JWK thumbprint helper
```

---

## Unit U2 · Wrapper-token issuer

- [x] Landed 2026-05-27 — `8178e6b4 gateway: wrapper_token — Ed25519-signed JWT with cnf.jkt binding + RFC 9068 at+jwt typ`.

### Files

- Create `crates/gateway/src/wrapper_token.rs`
- Modify `crates/gateway/src/lib.rs` (export)

### Wrapper-token claim shape

```json
{
  "iss": "https://api.zeroship.ai",        // gateway's public URL
  "aud": "myapp.zeroship.ai",              // the app the wrapper is for
  "sub": "usr_01H...",                     // from hydra introspection
  "exp": <unix_secs>,                      // 1h TTL
  "iat": <unix_secs>,
  "jti": "<random>",
  "cnf": { "jkt": "<rfc7638-thumbprint-of-bound-key>" },
  "scope": "openid profile email offline_access",
  "client_id": "gateway",
  "email": "...",
  "email_verified": true,
  "name": "...",
  "wraps": "<sha256 hash of original hydra token>"  // forensic link, not used at verify time
}
```

Header: `{"typ":"at+jwt","alg":"EdDSA","kid":"<thumbprint of gateway signing key>"}`. Uses RFC 9068 access-token JWT typ.

### Issuer

```rust
//! Gateway-issued wrapper access tokens. Real cnf.jkt-bound JWTs.

use jsonwebtoken::{encode, EncodingKey, Header, Algorithm};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{GatewayError, Result};

#[derive(Debug, Serialize, Deserialize)]
pub struct WrapperClaims {
    pub iss: String,
    pub aud: String,
    pub sub: String,
    pub exp: i64,
    pub iat: i64,
    pub jti: String,
    pub cnf: Cnf,
    pub scope: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email_verified: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub wraps: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Cnf {
    pub jkt: String,
}

pub struct Issuer {
    signing_key: ed25519_dalek::SigningKey,
    kid: String,             // thumbprint of signing key
    issuer_url: String,
}

impl Issuer {
    pub fn new(signing_key: ed25519_dalek::SigningKey, issuer_url: String) -> Self {
        let kid = crate::signing::jwk_thumbprint(&signing_key);
        Self { signing_key, kid, issuer_url }
    }

    /// Issue a wrapper token bound to `proof_jkt`, wrapping the hydra token's claims.
    ///
    /// # Errors
    ///
    /// `GatewayError::Internal` on JWT encoding failure.
    pub fn issue(
        &self,
        aud: &str,
        introspection: &crate::oidc_rp::IntrospectionResponse,
        proof_jkt: &str,
        hydra_token: &str,
    ) -> Result<String> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)
            .map_err(|e| GatewayError::Internal(format!("clock: {e}")))?
            .as_secs() as i64;

        use sha2::{Digest, Sha256};
        let wraps = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(hydra_token.as_bytes()));

        let claims = WrapperClaims {
            iss: self.issuer_url.clone(),
            aud: aud.to_string(),
            sub: introspection.sub.clone().unwrap_or_default(),
            exp: now + 3600,
            iat: now,
            jti: uuid::Uuid::new_v4().to_string(),
            cnf: Cnf { jkt: proof_jkt.to_string() },
            scope: introspection.scope.clone().unwrap_or_default(),
            client_id: introspection.client_id.clone().unwrap_or_default(),
            email: introspection.email.clone(),
            email_verified: introspection.email_verified,
            name: introspection.name.clone(),
            wraps,
        };

        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some("at+jwt".into());
        header.kid = Some(self.kid.clone());

        // jsonwebtoken's EncodingKey::from_ed_der accepts PKCS#8 DER bytes.
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        let der = self.signing_key.to_pkcs8_der()
            .map_err(|e| GatewayError::Internal(format!("encode key: {e}")))?;
        let key = EncodingKey::from_ed_der(der.as_bytes());

        encode(&header, &claims, &key)
            .map_err(|e| GatewayError::Internal(format!("jwt encode: {e}")))
    }

    pub fn kid(&self) -> &str { &self.kid }
}
```

### Verifier helper

The dispatch path (U4) needs to verify wrapper tokens. Build the verifier alongside the issuer:

```rust
pub struct Verifier {
    public_key: ed25519_dalek::VerifyingKey,
    kid: String,
    expected_iss: String,
}

impl Verifier {
    pub fn new(public_key: ed25519_dalek::VerifyingKey, issuer_url: String) -> Self {
        let kid = jwk_thumbprint_public(&public_key);
        Self { public_key, kid, expected_iss: issuer_url }
    }

    pub fn verify(&self, token: &str) -> Result<WrapperClaims> {
        // 1. Decode header, check kid + alg + typ.
        // 2. Verify signature.
        // 3. Verify exp + iss.
        // 4. Return claims.
        // ... use jsonwebtoken::decode with strict Validation
    }
}
```

### Tests

Unit-test issue → decode → verify roundtrip; verify rejects tampered tokens; verify rejects expired tokens.

### Commit

```
gateway: wrapper_token — Ed25519-signed JWT with cnf.jkt binding + RFC 9068 at+jwt typ
```

---

## Unit U3 · `/__zs/auth/dpop-exchange` endpoint

- [x] Landed 2026-05-27 — `b3e7f245 gateway: /__zs/auth/dpop-exchange — issue wrapper token bound to client's DPoP key`.

### Files

- Create `crates/gateway/src/dpop_exchange.rs`
- Modify `crates/gateway/src/main.rs` — register route

### Endpoint

```
POST /__zs/auth/dpop-exchange
Authorization: Bearer <hydra_access_token>
DPoP: <proof signed by client's DPoP key>

→ 200 OK
   Content-Type: application/json
   {"wrapper_token": "<JWT>", "expires_in": 3600, "token_type": "DPoP"}
```

The DPoP proof's `htu` claim must be `https://api.zeroship.ai/__zs/auth/dpop-exchange`. `htm` is `POST`. `ath` is the SHA-256 of the hydra token.

### Handler

```rust
pub async fn handle(
    req: ntex::web::HttpRequest,
    state: ntex::web::types::State<Arc<GateState>>,
) -> HttpResponse {
    // 1. Parse Bearer + DPoP headers.
    let auth_header = req.headers().get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok()).unwrap_or("");
    let hydra_token = match auth_header.strip_prefix("Bearer ") {
        Some(t) => t,
        None => return HttpResponse::BadRequest().json(&json!({"error":"missing Bearer hydra token"})),
    };
    let proof = match req.headers().get("dpop").and_then(|v| v.to_str().ok()) {
        Some(p) => p,
        None => return HttpResponse::BadRequest().json(&json!({"error":"missing DPoP header"})),
    };

    // 2. Verify the DPoP proof.
    let host = req.headers().get(http::header::HOST)
        .and_then(|h| h.to_str().ok()).unwrap_or("");
    let scheme = if state.config.insecure_dev { "http" } else { "https" };
    let expected_uri = format!("{scheme}://{host}/__zs/auth/dpop-exchange");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64).unwrap_or(0);

    let verified = match zeroship_core::dpop::verify(proof, "POST", &expected_uri, Some(hydra_token), now) {
        Ok(v) => v,
        Err(e) => return HttpResponse::Unauthorized().json(&json!({"error":"invalid DPoP proof","detail":e.to_string()})),
    };

    // 3. jti replay check.
    if !state.dpop_jti_cache.insert(&verified.jti, now, 120) {
        return HttpResponse::Unauthorized().json(&json!({"error":"DPoP jti replay"}));
    }

    // 4. Introspect the hydra token.
    let intro = match state.oidc_rp.introspect_token(hydra_token).await {
        Ok(i) if i.active => i,
        _ => return HttpResponse::Unauthorized().json(&json!({"error":"hydra token inactive"})),
    };

    // 5. Issue wrapper token.
    let issuer = match state.wrapper_issuer.as_ref() {
        Some(i) => i,
        None => return HttpResponse::ServiceUnavailable().json(&json!({"error":"DPoP binding not configured on this gateway"})),
    };

    // The audience is the per-app host (myapp.zeroship.ai), derived from Host.
    let aud = host.to_string();
    let wrapper = match issuer.issue(&aud, &intro, &verified.jkt, hydra_token) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(error = %e, "wrapper issue failed");
            return HttpResponse::InternalServerError().json(&json!({"error":"issuer failed"}));
        }
    };

    HttpResponse::Ok()
        .header("cache-control", "no-store")
        .json(&json!({
            "wrapper_token": wrapper,
            "expires_in": 3600,
            "token_type": "DPoP",
        }))
}
```

Register in `main.rs`:

```rust
.service(web::resource("/__zs/auth/dpop-exchange")
    .route(web::post().to(dpop_exchange::handle)))
```

### Tests

Live-stack test: with a live hydra + a fresh hydra access token (via password grant or e2e flow), POST to /__zs/auth/dpop-exchange with a Bearer + DPoP and assert a valid wrapper JWT comes back.

### Commit

```
gateway: /__zs/auth/dpop-exchange — issue wrapper token bound to client's DPoP key
```

---

## Unit U4 · Dispatch verifies wrapper tokens with cnf.jkt check

- [x] Landed 2026-05-27 — `841211f0 gateway: dispatch — verify wrapper tokens locally + enforce cnf.jkt binding (raw hydra path preserved)`.

### Modify `crates/gateway/src/router/auth.rs::resolve_dpop_user_header`

Currently the DPoP path in P7-U5 introspects the access token at hydra per request. Phase 8 changes this for wrapper tokens:

1. Detect that the token is a wrapper (gateway-signed JWT, not a hydra opaque/JWT)
2. Verify wrapper signature locally (no hydra introspect needed)
3. Verify `cnf.jkt == proof.jkt`
4. Use wrapper claims directly

If the token is NOT a wrapper (e.g., raw hydra token), keep the P7-U5 path: introspect + accept proof without cnf.jkt binding. This preserves backward compatibility with clients that haven't migrated to wrapper tokens yet.

```rust
async fn resolve_dpop_user_header(
    req: &HttpRequest,
    state: &Arc<GateState>,
    access_token: &str,
) -> Option<String> {
    // ... existing proof verification (same as P7-U5) ...

    let verified_proof = match zeroship_core::dpop::verify(proof, method, &expected_uri, Some(access_token), now) {
        Ok(v) => v,
        Err(_) => return None,
    };

    // jti replay (same as P7-U5)
    if !state.dpop_jti_cache.insert(&verified_proof.jti, now, 120) {
        return None;
    }

    // NEW: try wrapper-token verification first.
    if let Some(verifier) = state.wrapper_verifier.as_ref() {
        if let Ok(claims) = verifier.verify(access_token) {
            // It's a wrapper token. Enforce cnf.jkt binding.
            if claims.cnf.jkt != verified_proof.jkt {
                tracing::warn!("DPoP proof jkt does not match wrapper cnf.jkt — rejecting");
                return None;
            }
            // Build user header from wrapper claims (no introspection needed).
            return Some(encode_user_header(&user_from_wrapper_claims(&claims), &state.config.worker_key));
        }
    }

    // FALLBACK: not a wrapper. Use the P7-U5 path (introspect at hydra, no cnf.jkt enforcement).
    // This branch exists for clients that present a raw hydra token with DPoP — Phase 7 v1
    // behavior. They get DPoP proof verification but no binding (the gap the proposal §18 calls out).
    let info = state.oidc_rp.introspect_token(access_token).await.ok().filter(|i| i.active)?;
    Some(encode_user_header(&user_from_introspection(&info), &state.config.worker_key))
}
```

### `wrapper_verifier` on GateState

Build a `Verifier` at boot from the public half of the signing key:

```rust
let wrapper_verifier = signing_key.as_ref().map(|key| {
    Arc::new(wrapper_token::Verifier::new(key.verifying_key(), cfg.public_url.clone()))
});
```

Thread into GateState.

### Commit

```
gateway: dispatch — verify wrapper tokens locally + enforce cnf.jkt binding (raw hydra path preserved)
```

---

## Unit U5 · E2e test: full DPoP-bound flow

- [x] Landed 2026-05-27 — `269d62b6 gateway: e2e — DPoP-bound wrapper-token flow (exchange + dispatch + mismatch rejection)`. (Test file landed as `crates/gateway/tests/dpop_bound_e2e.rs`.)

File: `crates/gateway/tests/dpop_binding_e2e.rs`.

```rust
//! End-to-end DPoP binding flow:
//!   1. Login via password (existing e2e_password pattern), get a hydra access token
//!   2. Generate Ed25519 client DPoP keypair
//!   3. POST /__zs/auth/dpop-exchange with Bearer + DPoP → wrapper token
//!   4. GET / with Authorization: DPoP <wrapper> + DPoP proof → success
//!   5. GET / with the SAME wrapper but a DIFFERENT DPoP key → 401 (binding enforcement)
//!   6. GET / with a tampered wrapper → 401 (signature enforcement)

#[ntex::test]
async fn full_dpop_binding_roundtrip() {
    let Some(env) = require_live_env() else { eprintln!("skip"); return };
    // ... (long test; see plan)
}
```

This is the proof Phase 8 works. ~250 LOC.

### Commit

```
gateway: tests/dpop_binding_e2e — full bound-token flow against live hydra (exchange → bound request → key mismatch rejected)
```

---

## Unit U6 · Phase 8 close-out

- Workspace build + clippy
- Test sweep (auth + gateway + control + core)
- Milestone empty-commit + `auth-phase-8` tag
- Commit message acknowledges: gateway is now a JWT issuer; key custody + rotation are operator responsibilities (document the gateway-signing-key handling in the deploy runbook)

---

## Future work (post-Phase-8)

- **Phase 9** — KMS-backed gateway signing key (today's load-from-disk is OK for v1 ops; KMS is the production move)
- Wrapper-token refresh flow (today's wrapper is non-refreshable; you re-exchange when it expires)
- Mapping wrapper-token revocation back to hydra session revocation (back-channel logout cascade)
- `WWW-Authenticate: DPoP` response header on failure (RFC 9449 §7.1 niceties)

---

# Self-review

**Spec coverage** — RFC 9449 §5 (token-binding via cnf.jkt) and §6 (server-side enforcement). The wrapper-token approach is one of two RFC-conformant paths (the other being hydra-native cnf.jkt issuance, which would require forking hydra).

**Limitations honestly stated** — Phase 8 v1 keeps the raw-hydra-token DPoP path (P7-U5) as a fallback so clients can opt into binding gradually. This is a real attenuation of the DPoP guarantee; production deployments that care about DPoP should disable the raw-token fallback (a config flag `--strict-dpop` is a Phase 9+ refinement).

**Scope** — Phase 8 ships the binding primitive + the exchange endpoint + verification. Refresh, cascade-revoke, KMS integration are explicitly Phase 9+.

# Execution handoff

Plan saved to `docs/superpowers/plans/2026-05-27-auth-server-phase-8-dpop-binding.md`.
Execute via subagent-driven-development.
