//! `POST /internal/power-token` — the grant-gated, identity-bound,
//! server-side capability mint (R4).
//!
//! This is the SECURITY BOUNDARY that lets a platform-owned app (the console,
//! once it lands in R5) wield control-plane authority that ordinary creator
//! apps must NEVER obtain. The design is in
//! `docs/superpowers/specs/2026-05-30-auth-bff-session-redesign.md` §3.2.
//!
//! ## The trust chain (every step fails closed)
//!
//! 1. **Channel auth** — the worker→control `control_key` shared secret on the
//!    existing `internal.rs` router. Proves the call came from a worker, not
//!    from app JS. `control_key` is attached by the Rust runtime, never by app
//!    JS, so it is never JS-visible.
//! 2. **Identity** — control re-derives the user from the **gateway-signed
//!    `ZeroShip-User` header** (HMAC over `worker_key`, the SAME verifier the
//!    worker uses at `handler.rs:65`). Only the gateway can mint a valid header.
//!    Control NEVER trusts a worker-supplied identity. A forged / absent /
//!    expired / tampered header → `401 unauthenticated_identity`.
//! 3. **App resolution** — the `X-ZS-App-Id` header (Rust-stamped by the
//!    runtime) names the dispatching app. Control loads its `oauth_client_id`
//!    + `sector_identifier`.
//! 4. **Grant ceiling** — control resolves the `pws_`→`global_user` mapping by
//!    deriving `pws_` for each live `auth.app_session_anchors` row of this app
//!    and matching the header's `pws_`. The matched anchor's `granted_scopes`
//!    is the CEILING; a requested scope outside it → `403 scope_required`. No
//!    matching anchor → `403 consent_required`.
//! 5. **Step-up** — deploy / secret / billing / delete-class scopes require a
//!    fresh `auth_time` (≤ `STEP_UP_MAX_AGE_SECS`). Stale → `403
//!    step_up_required`.
//! 6. **Privilege boundary (THE HEADLINE)** — a `CONTROL_PLANE_AUDIENCE` mint
//!    is permitted ONLY if the app's `oauth_client_id` is in the
//!    control-config `trusted_oauth_clients` allowlist (a platform-privileged
//!    marker creator apps cannot set). An ordinary creator app → `403
//!    forbidden_audience`.
//!
//! The minted token is a short-lived ed25519-signed JWT, `sub` = the user's
//! global UUID, `aud` = the control plane's `expected_oauth_audience`,
//! `scope` = the capped intersection. It is verified by `AuthzGuard` (see
//! `authz_guard.rs` `guard_from_power_token`) statelessly — no DB row — bounded
//! by its short `exp` + capped `scope`, then runs the existing per-op
//! `require(Action, Resource)`.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use ntex::web::{self, types::State};
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use zeroship_core::power_token::{
    any_scope_requires_step_up, error_code, PowerTokenRequest, PowerTokenResponse,
    CONTROL_PLANE_AUDIENCE, POWER_TOKEN_APP_ID_HEADER, POWER_TOKEN_DEFAULT_TTL_SECS,
    STEP_UP_MAX_AGE_SECS,
};

use crate::AppState;

/// The JWT `typ` header for a minted power token. Distinct from `pat+jwt` so a
/// power token can never be mistaken for a PAT (different verification arm,
/// different lifecycle).
const POWER_TOKEN_TYP: &str = "zs-power+jwt";
/// The issuer claim. Distinct namespace marker so the verifier pins it.
const POWER_TOKEN_ISS: &str = "https://api.zeroship.ai/power";

/// Claims on a minted power token. `sub` is the GLOBAL user UUID (so it
/// composes with control authz, which keys `principal_id` on the global user);
/// `scope` is the capped intersection; `auth_time` carries the authenticating
/// event for the resource server's own freshness checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerTokenClaims {
    pub iss: String,
    pub aud: String,
    /// Global user UUID (string).
    pub sub: String,
    /// Per-app pairwise subject the user was authenticated as (audit only).
    pub pws: String,
    /// Per-app OAuth client id this token was minted for.
    pub client_id: String,
    pub jti: String,
    pub iat: i64,
    pub exp: i64,
    /// Unix seconds of the authenticating event (for step-up recency).
    pub auth_time: i64,
    /// Space-delimited capped scopes.
    pub scope: String,
    pub nonce: String,
}

/// Signs + verifies power tokens with control's ed25519 key (the SAME key the
/// PAT issuer uses; the distinct `typ`/`iss` keep the two token classes apart).
pub struct PowerTokenIssuer {
    private_der: Vec<u8>,
    decoding_key: DecodingKey,
    kid: String,
    /// The concrete OAuth audience the resource server (`AuthzGuard`) expects.
    /// The minted token's `aud` is set to this so it rides the existing
    /// introspection-equivalent audience check.
    audience: String,
}

impl std::fmt::Debug for PowerTokenIssuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PowerTokenIssuer")
            .field("kid", &self.kid)
            .field("audience", &self.audience)
            .finish_non_exhaustive()
    }
}

impl PowerTokenIssuer {
    pub fn new(
        signing_key: &ed25519_dalek::SigningKey,
        audience: String,
    ) -> Result<Self, String> {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        let private_der = signing_key
            .to_pkcs8_der()
            .map_err(|err| format!("power-token signing key PKCS#8 encode: {err}"))?
            .as_bytes()
            .to_vec();
        let decoding_key = DecodingKey::from_ed_der(signing_key.verifying_key().as_bytes());
        // Reuse the same kid scheme as the PAT issuer for parity.
        let kid = crate::token_handlers::jwk_thumbprint(signing_key);
        Ok(Self {
            private_der,
            decoding_key,
            kid,
            audience,
        })
    }

    pub fn dev_insecure(audience: String) -> Self {
        let key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        Self::new(&key, audience).expect("generated dev power-token key is valid")
    }

    /// Mint a short-lived power token bound to `global_user`/`pws_`/`client_id`
    /// with the capped `scopes`, carrying `auth_time`.
    #[allow(clippy::too_many_arguments)]
    pub fn issue(
        &self,
        global_user: Uuid,
        pws: &str,
        client_id: &str,
        scopes: &[String],
        auth_time: i64,
        ttl_secs: i64,
    ) -> Result<(String, i64), String> {
        let now = now_unix()?;
        let exp = now + ttl_secs;
        let mut nonce = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce);
        let claims = PowerTokenClaims {
            iss: POWER_TOKEN_ISS.to_owned(),
            aud: self.audience.clone(),
            sub: global_user.to_string(),
            pws: pws.to_owned(),
            client_id: client_id.to_owned(),
            jti: Uuid::new_v4().to_string(),
            iat: now,
            exp,
            auth_time,
            scope: scopes.join(" "),
            nonce,
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some(POWER_TOKEN_TYP.to_owned());
        header.kid = Some(self.kid.clone());
        let key = EncodingKey::from_ed_der(&self.private_der);
        let token =
            encode(&header, &claims, &key).map_err(|err| format!("power-token encode: {err}"))?;
        Ok((token, exp))
    }

    /// Verify a power token statelessly. Returns the claims on success. Pins
    /// `typ`, `kid`, `iss`, `aud`, and signature; `exp` is checked by
    /// `jsonwebtoken`. No DB lookup — a power token is bounded entirely by its
    /// short `exp` + capped `scope`.
    pub fn verify(&self, token: &str) -> Result<PowerTokenClaims, String> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|err| format!("power-token header decode: {err}"))?;
        if header.typ.as_deref() != Some(POWER_TOKEN_TYP) {
            return Err(format!("unexpected power-token typ: {:?}", header.typ));
        }
        match header.kid.as_deref() {
            Some(kid) if kid == self.kid => {}
            Some(kid) => return Err(format!("unknown power-token kid: {kid}")),
            None => return Err("missing power-token kid".to_owned()),
        }
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[POWER_TOKEN_ISS]);
        validation.set_audience(&[self.audience.as_str()]);
        decode::<PowerTokenClaims>(token, &self.decoding_key, &validation)
            .map(|data| data.claims)
            .map_err(|err| format!("power-token verify: {err}"))
    }
}

fn now_unix() -> Result<i64, String> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| format!("clock: {err}"))?
            .as_secs(),
    )
    .map_err(|err| format!("clock overflow: {err}"))
}

// ---------------------------------------------------------------------------
// The identity carried in the verified ZeroShip-User header.
// ---------------------------------------------------------------------------

/// The fields we read from the gateway-signed `ZeroShip-User` JSON. The header
/// carries more (email/name/avatar) but the mint only needs the pairwise `id`
/// and the per-app `scopes` (the kernel-contract grant set).
#[derive(Debug, Deserialize)]
struct VerifiedUser {
    /// The per-app pairwise subject (`pws_…`).
    id: String,
    #[serde(default)]
    scopes: Vec<String>,
}

/// A live anchor row resolved for the `(app, user)` pair, carrying the grant
/// ceiling + the authenticating-event recency for step-up.
struct ResolvedAnchor {
    global_user_id: Uuid,
    client_id: String,
    granted_scopes: Vec<String>,
    /// Unix seconds of the authenticating event (NULL → 0 → always stale for
    /// step-up scopes, fail-closed).
    auth_time: i64,
}

fn err_json(status: u16, code: &str) -> web::HttpResponse {
    let builder = match status {
        401 => web::HttpResponse::Unauthorized(),
        403 => web::HttpResponse::Forbidden(),
        400 => web::HttpResponse::BadRequest(),
        _ => web::HttpResponse::InternalServerError(),
    };
    let mut builder = builder;
    builder.json(&json!({ "error": code }))
}

/// `POST /internal/power-token` — mint a power token. Gated by `control_key`
/// on the internal router (the caller already passed `internal.rs` check_auth).
pub async fn mint_power_token(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    body: web::types::Json<PowerTokenRequest>,
) -> web::HttpResponse {
    // 1. Channel auth — the internal router's check_auth already enforced
    //    control_key before reaching here (see configure() wiring). Re-affirm
    //    by failing closed if it was somehow not enforced (insecure_dev).
    if let Some(resp) = crate::internal::check_internal_auth(&req, &state) {
        return resp;
    }

    let opts = body.into_inner();

    // 2. Identity — re-derive the user from the gateway-signed ZeroShip-User
    //    header. NEVER trust a worker-supplied identity field.
    let Some(header_value) = req
        .headers()
        .get("zeroship-user")
        .and_then(|v| v.to_str().ok())
    else {
        return err_json(401, error_code::UNAUTHENTICATED_IDENTITY);
    };
    // Verify the HMAC + freshness with the SAME worker_key the gateway signs
    // with and the worker verifies with. We do NOT bind to a specific
    // request-id here (control is not the dispatch endpoint), but freshness
    // (max-age) + the signature still gate forgery/replay; the header is
    // request-id-bound and time-bound by construction.
    let Some(user_json) = zeroship_core::auth::verify_zeroship_user_header(
        state.worker_key.expose_secret().as_bytes(),
        header_value,
    ) else {
        return err_json(401, error_code::UNAUTHENTICATED_IDENTITY);
    };
    let Ok(user) = serde_json::from_str::<VerifiedUser>(&user_json) else {
        return err_json(401, error_code::UNAUTHENTICATED_IDENTITY);
    };
    // Defense-in-depth: the identity MUST be a per-app pairwise subject, never
    // the global UUID. The gateway always projects to pws_; reject anything
    // that survived without the prefix.
    if !zeroship_core::auth::is_pairwise_subject(&user.id) {
        return err_json(401, error_code::UNAUTHENTICATED_IDENTITY);
    }

    // 3. App resolution — the Rust-stamped app id.
    let Some(app_id) = req
        .headers()
        .get(POWER_TOKEN_APP_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| Uuid::parse_str(v).ok())
    else {
        return err_json(400, "missing_app_id");
    };

    // Resolve the app's oauth_client_id + sector_identifier. An app with no
    // oauth client cannot have a grant or a pairwise identity → fail closed.
    let (oauth_client_id, sector_identifier) = match load_app_oauth(&state, app_id).await {
        Ok(Some(pair)) => pair,
        Ok(None) => return err_json(403, error_code::CONSENT_REQUIRED),
        Err(resp) => return resp,
    };

    // 6. PRIVILEGE BOUNDARY — a control-audience mint requires a
    //    platform-privileged (trusted) client. Check this BEFORE doing any
    //    grant work so an ordinary app gets a clean, uniform rejection.
    let audience_is_control = opts.audience == CONTROL_PLANE_AUDIENCE;
    if audience_is_control {
        if !state.is_trusted(&oauth_client_id) {
            tracing::warn!(
                app_id = %app_id,
                client_id = %oauth_client_id,
                "power-token: non-privileged app denied control-audience mint"
            );
            return err_json(403, error_code::FORBIDDEN_AUDIENCE);
        }
    } else {
        // v1 only mints the control audience. Any other audience is a future
        // extension (external resource servers / third-party APIs) — fail
        // closed rather than mint an unverifiable token.
        return err_json(403, error_code::UNSUPPORTED_AUDIENCE);
    }

    // 4. Grant ceiling — resolve pws_ → global_user via the live anchors for
    //    this app, deriving pws_ per row and matching. The matched anchor's
    //    granted_scopes is the ceiling.
    let resolved =
        match resolve_anchor(&state, app_id, &oauth_client_id, &sector_identifier, &user.id).await
        {
            Ok(Some(a)) => a,
            Ok(None) => return err_json(403, error_code::CONSENT_REQUIRED),
            Err(resp) => return resp,
        };

    // The ceiling is the anchor's granted_scopes. (Cross-check against the
    // header scopes as defense-in-depth: the gateway put the same grant set in
    // the signed header, so they should agree; we use the anchor copy as the
    // authoritative ceiling per the design.)
    let ceiling = &resolved.granted_scopes;
    let over_broad: Vec<&String> = opts
        .scopes
        .iter()
        .filter(|s| !ceiling.contains(s))
        .collect();
    if !over_broad.is_empty() {
        tracing::info!(
            app_id = %app_id,
            requested = ?opts.scopes,
            ceiling = ?ceiling,
            "power-token: requested scope exceeds grant ceiling"
        );
        return err_json(403, error_code::SCOPE_REQUIRED);
    }
    // Also fail closed if the gateway-signed header scopes disagree with the
    // requested set in a way that would exceed what the SESSION carries. The
    // signed header's scopes are the per-request authority; never mint beyond.
    let header_over_broad = opts
        .scopes
        .iter()
        .any(|s| !user.scopes.contains(s));
    if header_over_broad {
        return err_json(403, error_code::SCOPE_REQUIRED);
    }

    // 5. Step-up — elevated scopes require a fresh auth_time.
    if any_scope_requires_step_up(&opts.scopes) {
        let now = now_unix().unwrap_or(i64::MAX);
        let age = now.saturating_sub(resolved.auth_time);
        if resolved.auth_time <= 0 || age > STEP_UP_MAX_AGE_SECS {
            tracing::info!(
                app_id = %app_id,
                age,
                "power-token: step-up required (stale auth_time)"
            );
            return err_json(403, error_code::STEP_UP_REQUIRED);
        }
    }

    // Mint.
    let (token, exp) = match state.power_token_issuer.issue(
        resolved.global_user_id,
        &user.id,
        &resolved.client_id,
        &opts.scopes,
        resolved.auth_time,
        POWER_TOKEN_DEFAULT_TTL_SECS,
    ) {
        Ok(pair) => pair,
        Err(err) => {
            tracing::error!(error = %err, "power-token: issue failed");
            return err_json(500, "mint_failed");
        }
    };

    web::HttpResponse::Ok().json(&PowerTokenResponse {
        access_token: token,
        expires_at: exp,
        scopes: opts.scopes,
    })
}

/// Load `(oauth_client_id, sector_identifier)` for `app_id` from the control
/// schema. `None` if the app has no provisioned OAuth client.
async fn load_app_oauth(
    state: &AppState,
    app_id: Uuid,
) -> Result<Option<(String, String)>, web::HttpResponse> {
    let rows = state
        .auth_pg
        .query(
            "SELECT client_id, sector_identifier \
             FROM control.app_oauth_clients WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "power-token: app oauth lookup failed");
            err_json(500, "lookup_failed")
        })?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let client_id: String = row.get("client_id");
    let sector: String = row.get("sector_identifier");
    Ok(Some((client_id, sector)))
}

/// Resolve the live anchor for `(app_id, pws_)` by deriving `pws_` for each
/// live anchor row of this app and matching. Returns the matched anchor's
/// `(global_user_id, client_id, granted_scopes, auth_time)`.
async fn resolve_anchor(
    state: &AppState,
    app_id: Uuid,
    expected_client_id: &str,
    sector_identifier: &str,
    pws: &str,
) -> Result<Option<ResolvedAnchor>, web::HttpResponse> {
    // anchors.app_id is TEXT (consistent with gateway_sessions); bind the
    // string form.
    let app_id_text = app_id.to_string();
    let rows = state
        .auth_pg
        .query(
            "SELECT global_user_id, client_id, granted_scopes, auth_time \
             FROM auth.app_session_anchors \
             WHERE app_id = $1 AND revoked_at IS NULL AND abs_expires_at > NOW()",
            &[&app_id_text],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "power-token: anchor lookup failed");
            err_json(500, "lookup_failed")
        })?;

    for row in rows {
        let global_user_id: Uuid = row.get("global_user_id");
        let client_id: String = row.get("client_id");
        let derived = zeroship_core::auth::derive_pairwise(
            &state.pairwise_salt,
            &global_user_id.to_string(),
            sector_identifier,
        );
        if derived != pws {
            continue;
        }
        // The matched anchor must also be for the expected client (the app's
        // own oauth client) — defense-in-depth against a mismatched binding.
        if client_id != expected_client_id {
            continue;
        }
        let granted_scopes: Vec<String> = row.get("granted_scopes");
        let auth_time: Option<chrono::DateTime<chrono::Utc>> = row.get("auth_time");
        let auth_time = auth_time.map(|t| t.timestamp()).unwrap_or(0);
        return Ok(Some(ResolvedAnchor {
            global_user_id,
            client_id,
            granted_scopes,
            auth_time,
        }));
    }
    Ok(None)
}
