# Auth server — Phase 3 implementation plan: Gateway + control as OIDC RPs

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Migrate the gateway and the control plane to be OIDC Relying Parties of `auth.zeroship.ai`. By the end of this phase, an end-user hitting `myapp.zeroship.ai` unauthenticated is redirected through hydra to our `/login` (Phase 2's password flow), signs in, and proxies through to the worker carrying the unchanged HMAC-signed `ZeroShip-User` header. The legacy in-process auth in `crates/control` and `crates/gateway` is deleted in the same PR.

**Architecture:** The gateway forwards `auth.zeroship.ai/oauth2/*` and `/.well-known/*` to hydra (manifest dispatch rule). For every other inbound request to a creator app, the gateway either (a) carries a valid `__Host-zs_app_session` cookie → proxy to worker, OR (b) lacks one → 302 to `auth.zeroship.ai/oauth2/auth?client_id=gateway&...` with PKCE. On the `/__zs/auth/callback` return, the gateway exchanges the code, verifies the ID token against cached JWKS, sets a per-origin session cookie, and proxies. Control plane is a separate OIDC client with `client_id=console.zeroship.ai` and its own session cookie at the dashboard origin.

**Tech Stack:**
- Continuing from Phases 1+2 — compio, ntex, cyper, compio-postgres, hydra v25.4.0.
- Adds `jsonwebtoken` (for ID-token signature verification on the RP side) — already a workspace dep.
- Adds a small in-memory JWKS cache in each RP.

**Reference docs to keep open:**
- `docs/proposals/auth-server.md` §2.2 (end-user login sequence), §9 (sessions), §11 (migration), §13 (threat model)
- `docs/superpowers/plans/2026-05-27-auth-server-phase-2-password-login.md` — Phase 2 task layout for pattern reference
- `crates/gateway/src/user_auth.rs` — the existing JWT path (gets deleted)
- `crates/gateway/src/router/dispatch.rs:963-971` — the call site that `extract_user` + `encode_user_header` are wired through
- `crates/control/src/auth_service.rs`, `auth_handlers.rs`, `oauth.rs` — files that get deleted

**Pre-launch posture (AGENTS.md):** no back-compat shims. The legacy auth is deleted in one PR; nothing pre-Phase-3 needs to keep working. Existing-table data migration is one-shot atomic.

**Phase 3 starting point:** worktree tip at `58abebaf` on `proposal/auth-server`. Tag `auth-phase-2` at `4918c9c3`. All 24 tests green. Hydra (with both EdDSA and RS256 keys) + Postgres docker stack still up.

---

## Phase 3 unit list (overview)

| # | Unit | Files | Time |
|---|---|---|---|
| U1 | Gateway proxy: forward `/oauth2/*` and `/.well-known/*` to hydra | dispatch.rs + supporting | 30 min |
| U2 | Shared JWKS cache + ID-token verifier (workspace util) | new crate or `core` module | 35 min |
| U3 | Gateway OIDC RP module — state, exchange, verify | gateway/src/oidc_rp.rs | 50 min |
| U4 | Gateway per-origin session store (PG schema + CRUD) | new `gateway.sessions` table in `auth.*` schema + store module | 30 min |
| U5 | Gateway: replace user_auth path with OIDC RP redirect+callback | dispatch.rs integration + main.rs config | 45 min |
| U6 | Delete legacy `crates/gateway/src/user_auth.rs` + tests | gateway | 20 min |
| U7 | Control plane OIDC RP module + wire | control/src/oidc_rp.rs + main.rs + lib.rs | 40 min |
| U8 | Delete legacy control auth — auth_service.rs, auth_handlers.rs, oauth.rs + routes | control | 30 min |
| U9 | Legacy table migration (auth_users → auth.users) + drop legacy tables | new migration | 25 min |
| U10 | `sdks/auth` reduction — drop browser fallback | sdks/auth/src/index.ts | 15 min |
| U11 | Full-stack e2e test: real browser flow through gateway+worker+auth+hydra | tests/e2e_full_stack.rs | 60 min |
| U12 | Update `docs/reference/auth.md` | docs | 15 min |
| U13 | Phase 3 close-out (clippy + milestone tag) | – | 10 min |

Total: ~6h focused work, ~15 commits.

---

# Unit U1 · Gateway proxy: forward `/oauth2/*` and `/.well-known/*` to hydra

**Files:** `crates/gateway/src/router/dispatch.rs` (or wherever the manifest dispatch lives), `crates/gateway/src/main.rs` (config to receive hydra public URL).

The gateway must serve `auth.zeroship.ai/oauth2/auth`, `/oauth2/token`, `/.well-known/openid-configuration`, etc. from hydra's public port. The browser only ever talks to the gateway; the gateway proxies.

## Task U1.1 · Gateway config gains `hydra_public` URL

**File:** `crates/gateway/src/main.rs` (the config struct)

- [ ] **Step U1.1.1:** Add `--hydra-public <URL>` flag/env-var. Default `http://hydra:4444` for docker compose, `http://127.0.0.1:4444` in dev.
- [ ] **Step U1.1.2:** Pass into the gateway's `GateState` so handlers can reach it.
- [ ] **Step U1.1.3:** Update `docker-compose.yml` gateway service to pass `--hydra-public http://hydra:4444`.
- [ ] **Step U1.1.4:** Commit `gateway: config — add --hydra-public for OIDC proxy forwarding`.

## Task U1.2 · Add proxy rule for `auth.zeroship.ai` host

The gateway's dispatch is manifest-driven. We need a rule that says: when the inbound `Host` is `auth.zeroship.ai`, forward `/oauth2/*` and `/.well-known/*` to `${hydra_public}/...` and let `crates/auth` handle everything else.

Two options:

**Option A:** Hard-code the rule in dispatch.rs (auth.zeroship.ai is a platform-internal host, not a creator app).
**Option B:** Add it as a synthetic entry to the route registry at boot.

Option A is simpler. Recommend Option A.

- [ ] **Step U1.2.1:** In `dispatch.rs::handle` (or wherever the host-routing decision happens), add a branch:

```rust
if host == "auth.zeroship.ai" {
    return route_auth_host(req, state).await;
}
```

- [ ] **Step U1.2.2:** Implement `route_auth_host`:

```rust
async fn route_auth_host(req: HttpRequest, state: GateState) -> HttpResponse {
    let path = req.uri().path();
    if path.starts_with("/oauth2/") || path.starts_with("/.well-known/") || path == "/userinfo" {
        // Forward to hydra public port.
        proxy_to(&state.config.hydra_public, req).await
    } else {
        // Forward to crates/auth on its admin port (the in-cluster hostname).
        proxy_to(&state.config.auth_public, req).await
    }
}
```

`state.config.auth_public` is a NEW config field: `--auth-public http://auth:9092` in docker compose. The gateway needs a way to reach the `crates/auth` binary inside the cluster. Add it the same way as `hydra_public`.

`proxy_to` is the gateway's existing proxy machinery (look at `crates/gateway/src/proxy.rs` for the pattern). Reuse.

- [ ] **Step U1.2.3:** Test: unit test that `host == "auth.zeroship.ai" && path == "/.well-known/openid-configuration"` routes to the hydra_public URL.
- [ ] **Step U1.2.4:** Live verify with the stack: `curl -H 'Host: auth.zeroship.ai' http://localhost:8000/.well-known/openid-configuration | jq .` → should return hydra's discovery doc.
- [ ] **Step U1.2.5:** Commit `gateway: dispatch — route auth.zeroship.ai/{oauth2,.well-known}/* to hydra; rest to crates/auth`.

---

# Unit U2 · Shared JWKS cache + ID-token verifier

Both the gateway and the control plane need to verify ID tokens issued by hydra. Each fetches `https://auth.zeroship.ai/.well-known/jwks.json`, caches it for 5 minutes, and uses the public keys to verify JWT signatures.

**Decision:** put this in `crates/core` (the existing shared-utility crate) as `core::oidc_verify`. Both RPs depend on it.

## Task U2.1 · JWKS cache module

**File:** `crates/core/src/oidc_verify.rs` (new), `crates/core/src/lib.rs` (export).

- [ ] **Step U2.1.1:** Write the failing test in `crates/core/tests/oidc_verify_test.rs`:
  - Test that `JwksCache::new(url)` works against the live hydra (skip-when-env-unset).
  - Test that `verify_id_token(&token, &expected_iss, &expected_aud, &expected_nonce)` succeeds on a valid token and rejects on signature/iss/aud/nonce mismatch.

- [ ] **Step U2.1.2:** Implement:

```rust
pub struct JwksCache {
    url: String,
    inner: parking_lot::RwLock<JwksState>,
}

struct JwksState {
    keys: Vec<jsonwebtoken::DecodingKey>,
    fetched_at: std::time::Instant,
}

impl JwksCache {
    pub fn new(url: impl Into<String>) -> Self { /* ... */ }

    /// Fetch JWKS now if not cached or stale (>5min). Returns the active keys.
    pub async fn keys(&self) -> Result<Vec<DecodingKey>, OidcError> { /* ... */ }

    /// Force-refresh (call on signature verify failure).
    pub async fn refresh(&self) -> Result<(), OidcError> { /* ... */ }
}

pub async fn verify_id_token(
    cache: &JwksCache,
    token: &str,
    expected_iss: &str,
    expected_aud: &str,
    expected_nonce: Option<&str>,
) -> Result<TokenClaims, OidcError> {
    // 1. decode header to get kid + alg
    // 2. try each key in cache.keys()
    // 3. on verify success: check iss, aud, exp, iat, nonce
    // 4. on failure: cache.refresh().await + retry once
    // ...
}
```

Use `cyper` for the HTTP fetch (workspace-standard). Use `jsonwebtoken` for the JWT verification. The decoding-key construction from a JWK is standard `jsonwebtoken::DecodingKey::from_jwk_components(...)` or `from_rsa_components(...)` depending on alg.

- [ ] **Step U2.1.3:** Live test against the running hydra. Should pass.
- [ ] **Step U2.1.4:** Commit `core: oidc_verify — JWKS cache + ID-token verification (jsonwebtoken + cyper)`.

---

# Unit U3 · Gateway OIDC RP module

**File:** `crates/gateway/src/oidc_rp.rs` (new), `crates/gateway/src/main.rs` (state wiring), `crates/gateway/src/lib.rs` (export).

The module handles:
1. **Authorize redirect** — given an unauthenticated request, build the `/oauth2/auth` URL with PKCE + state + nonce, set a short-lived cookie holding the verifier/state/nonce + original-path, return 302.
2. **Callback** — given `?code=...&state=...`, read the short-lived cookie, exchange code at `auth.zeroship.ai/oauth2/token`, verify the ID token, set the per-origin session cookie, return 302 to original-path.
3. **Cookie helpers** — set/parse the per-origin `__Host-zs_app_session` cookie.

## Task U3.1 · The module shape

- [ ] **Step U3.1.1:** Write `crates/gateway/src/oidc_rp.rs`:

```rust
//! OIDC Relying Party module. Gateway runs this against auth.zeroship.ai for
//! every hosted creator app. The gateway is the only RP zeroship-hosted apps
//! see; per-app SSO falls out for free because hydra's session cookie at
//! auth.zeroship.ai survives across apps.

use crate::config::GatewayConfig;
use zeroship_core::oidc_verify::{JwksCache, verify_id_token};

pub struct OidcRp {
    pub auth_public: String,      // https://auth.zeroship.ai
    pub client_id: String,        // "gateway"
    pub client_secret: String,    // from env
    pub jwks: JwksCache,
}

impl OidcRp {
    pub fn new(cfg: &GatewayConfig) -> Self { /* ... */ }

    /// Build the /oauth2/auth redirect URL + a short-lived stash cookie.
    /// Returns (redirect_url, set_cookie_value).
    pub fn build_authorize_redirect(&self, original_path: &str, redirect_host: &str) -> (String, String) {
        // 1. generate state, nonce, pkce verifier
        // 2. build /oauth2/auth URL with code_challenge=S256(verifier)
        // 3. encode the stash (state, verifier, nonce, original_path) as a JSON
        //    string, sign with HMAC-state.config.auth_secret, base64,
        //    set as __Host-zs_oidc_stash cookie at the redirect_host origin
        // 4. return (url, cookie value)
    }

    /// Process the callback. Extract stash, exchange code, verify token, return user claims.
    pub async fn finish_callback(
        &self,
        code: &str,
        state_param: &str,
        stash_cookie: &str,
        redirect_uri: &str,
    ) -> Result<UserClaims, OidcRpError> {
        // 1. verify+decode stash
        // 2. check state == stash.state
        // 3. POST to /oauth2/token with code + verifier + client_secret
        // 4. verify ID token signature via JwksCache
        // 5. check nonce in ID token == stash.nonce
        // 6. return decoded claims
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UserClaims {
    pub sub: String,
    pub email: Option<String>,
    pub email_verified: Option<bool>,
    pub name: Option<String>,
    pub avatar: Option<String>,
}
```

- [ ] **Step U3.1.2:** Reference shape — `crates/auth/tests/common/pkce.rs` (from Phase 2.5 helpers) has the PKCE generator. The gateway can either depend on `zeroship-auth` for that one helper OR copy it. Recommend: extract PKCE generator to `crates/core` (since both auth tests and gateway RP need it).

- [ ] **Step U3.1.3:** Tests: unit-test `build_authorize_redirect` produces a well-formed URL with `code_challenge_method=S256`, state, nonce, client_id. Stash cookie round-trips through sign+verify.

- [ ] **Step U3.1.4:** Commit `gateway: oidc_rp — authorize redirect + callback (with HMAC-signed stash)`.

## Task U3.2 · Per-origin session cookie

Per proposal §9.2, the gateway sets `__Host-zs_app_session=<opaque>` after a successful code exchange. The cookie's value is an opaque session id; server-side state lives in PG.

- [ ] **Step U3.2.1:** In `crates/gateway/src/oidc_rp.rs`, add cookie helpers:

```rust
pub const APP_SESSION_COOKIE: &str = "__Host-zs_app_session";

pub fn set_app_session_cookie(session_id: &uuid::Uuid, insecure_dev: bool) -> String {
    let secure = if insecure_dev { "" } else { "; Secure" };
    let max_age = 12 * 3600; // 12h hard
    format!("{APP_SESSION_COOKIE}={session_id}; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age={max_age}")
}

pub fn parse_app_session_cookie(cookie_header: &str) -> Option<uuid::Uuid> {
    // same shape as auth/src/sessions/login.rs
}
```

- [ ] **Step U3.2.2:** Commit `gateway: oidc_rp — __Host-zs_app_session cookie helpers`.

---

# Unit U4 · Gateway per-origin session store

**Files:** `crates/auth/src/store/migrations.rs` (add a new table), `crates/gateway/src/sessions.rs` (new), `crates/gateway/src/main.rs` (wire PG connection).

The gateway stores per-app sessions: which app, which user, when issued, idle/abs expiry.

Decision: store in the SAME PG as auth, in a new table `auth.gateway_sessions`. Keeps everything in one schema.

## Task U4.1 · Table

- [ ] **Step U4.1.1:** Add to `crates/auth/src/store/migrations.rs::STATEMENTS`:

```sql
CREATE TABLE IF NOT EXISTS auth.gateway_sessions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id         TEXT NOT NULL,                            -- typed_id (sub claim)
    app_id          TEXT NOT NULL,                            -- e.g. myapp.zeroship.ai
    email           CITEXT,
    name            TEXT,
    avatar_url      TEXT,
    email_verified  BOOLEAN NOT NULL DEFAULT FALSE,
    issued_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    idle_expires_at TIMESTAMPTZ NOT NULL,
    abs_expires_at  TIMESTAMPTZ NOT NULL,
    revoked_at      TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS auth_gateway_sessions_app_idx ON auth.gateway_sessions (app_id, user_id);
```

- [ ] **Step U4.1.2:** Run migrations smoke test — it should now create this table too.

- [ ] **Step U4.1.3:** Commit `auth: migrations — gateway_sessions table for per-origin app sessions`.

## Task U4.2 · Store CRUD in the gateway

- [ ] **Step U4.2.1:** Write `crates/gateway/src/sessions.rs`:

```rust
//! Per-origin app session store. The gateway maintains one row per
//! authenticated browser session per app.

pub struct AppSession {
    pub id: Uuid,
    pub user_id: String,
    pub app_id: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub email_verified: bool,
    pub idle_expires_at: chrono::DateTime<chrono::Utc>,
}

pub async fn create(conn: &Client, claims: &UserClaims, app_id: &str) -> Result<AppSession> {
    // INSERT INTO auth.gateway_sessions ... RETURNING id, ...
}

pub async fn validate(conn: &Client, id: Uuid, app_id: &str) -> Result<Option<AppSession>> {
    // SELECT WHERE id = $1 AND app_id = $2 AND revoked_at IS NULL AND idle_expires_at > NOW()
    // Also: UPDATE idle_expires_at = NOW() + 30min (sliding)
}

pub async fn revoke(conn: &Client, id: Uuid) -> Result<()> { /* ... */ }
```

- [ ] **Step U4.2.2:** Unit test against live PG: create + validate + revoke a session.

- [ ] **Step U4.2.3:** Commit `gateway: sessions — app session store CRUD against auth.gateway_sessions`.

---

# Unit U5 · Gateway integration: replace user_auth path with OIDC RP redirect+callback

**Files:** `crates/gateway/src/router/dispatch.rs` (the existing call site at line 963), `crates/gateway/src/main.rs` (state wiring + new `/__zs/auth/callback` route).

## Task U5.1 · Wire OIDC RP state into GateState

- [ ] **Step U5.1.1:** Add `Arc<OidcRp>` and `Arc<compio_postgres::Client>` to `GateState`.
- [ ] **Step U5.1.2:** Construct in `main.rs` after PG connects + OidcRp::new.
- [ ] **Step U5.1.3:** Commit `gateway: state — add oidc_rp + pg client to GateState`.

## Task U5.2 · Replace `user_auth::extract_user` with the OIDC RP flow

In `dispatch.rs:963` the current logic is:

```rust
let auth_header = cookie_h
    .and_then(|cookie| {
        user_auth::extract_user(cookie, &state.config.auth_secret, &app_id_str)
            .map(|u| user_auth::encode_user_header(&u, &state.config.worker_key))
    });
```

Replace with:

- [ ] **Step U5.2.1:** Look up `__Host-zs_app_session` cookie value.
- [ ] **Step U5.2.2:** If present: `sessions::validate(conn, session_id, &app_id_str).await` → `Some(AppSession)` → build UserClaims from session → `encode_user_header(&user, &state.config.worker_key)`.
- [ ] **Step U5.2.3:** If absent OR validation fails: 302 redirect to `auth.zeroship.ai/oauth2/auth?...`:
  - `OidcRp::build_authorize_redirect(original_path, app_host)` returns the URL + stash cookie.
  - Response: 302 with `Location: <url>` + `Set-Cookie: __Host-zs_oidc_stash=<value>`.
  - Browser follows.
- [ ] **Step U5.2.4:** Important: only do this for HTML requests (`Accept: text/html`). API requests should get 401 without redirect (don't redirect curl). Use the `Accept` header to decide; this matches the proposal §2.2 footnote.

- [ ] **Step U5.2.5:** Commit `gateway: dispatch — replace extract_user with OIDC RP redirect (HTML) / 401 (API)`.

## Task U5.3 · New `/__zs/auth/callback` route

- [ ] **Step U5.3.1:** Add a handler at the gateway:

```rust
#[ntex::web::get("/__zs/auth/callback")]
async fn auth_callback(req: HttpRequest, state: State<Arc<GateState>>) -> HttpResponse {
    // 1. extract ?code, ?state, ?error from query
    // 2. extract __Host-zs_oidc_stash cookie
    // 3. call state.oidc_rp.finish_callback(code, state, stash, redirect_uri).await
    // 4. on success: sessions::create(conn, claims, app_id).await → session_id
    //    set __Host-zs_app_session cookie; clear __Host-zs_oidc_stash
    //    302 to stash.original_path
    // 5. on error: render an error page (use the gateway's error template, or
    //    just plain text for v1)
}
```

- [ ] **Step U5.3.2:** Wire into `main.rs`'s server config.
- [ ] **Step U5.3.3:** Commit `gateway: handler — /__zs/auth/callback for OIDC code exchange`.

---

# Unit U6 · Delete legacy `crates/gateway/src/user_auth.rs`

After U5 is wired and tests pass, the legacy file is unused. Delete it.

- [ ] **Step U6.1:** `git rm crates/gateway/src/user_auth.rs`
- [ ] **Step U6.2:** Remove the `mod user_auth;` declaration from `crates/gateway/src/lib.rs` and `main.rs`.
- [ ] **Step U6.3:** Remove the `use crate::user_auth;` from `dispatch.rs` (and any other callers — grep first).
- [ ] **Step U6.4:** Update or remove the `__zs_session` references in `dispatch.rs` (lines 98, 166, 1177, 1196, etc. per the earlier grep). The `RateLimitPer::Session` bucket should now key on the new `__Host-zs_app_session` cookie value.
- [ ] **Step U6.5:** Build + test: `cargo build --workspace`, `cargo test -p zeroship-gateway`. Fix any breakage.
- [ ] **Step U6.6:** Commit `gateway: delete user_auth.rs — superseded by OIDC RP module`.

---

# Unit U7 · Control plane OIDC RP module

Same pattern as the gateway, but for the dashboard. Client id `console.zeroship.ai`.

**Files:** `crates/control/src/oidc_rp.rs` (new), `crates/control/src/main.rs` (state + route), `crates/control/src/lib.rs` (export).

- [ ] **Step U7.1:** Write `crates/control/src/oidc_rp.rs`. Largely mirrors `crates/gateway/src/oidc_rp.rs` but:
  - `client_id` is `console.zeroship.ai`
  - The session cookie is the dashboard's existing `__zs_dashboard_session` (or rename to `__Host-zs_console_session` for consistency).
  - Per-RP session state in `auth.console_sessions` (new table).
- [ ] **Step U7.2:** Add the table to `crates/auth/src/store/migrations.rs`.
- [ ] **Step U7.3:** Add `/auth/callback` route to the control plane. Old `/auth/*` routes don't exist anymore (deleted in U8).
- [ ] **Step U7.4:** Commit `control: oidc_rp — control plane RP module (console.zeroship.ai)`.

---

# Unit U8 · Delete legacy control auth

**Files to delete:**
- `crates/control/src/auth_service.rs`
- `crates/control/src/auth_handlers.rs`
- `crates/control/src/oauth.rs`

**Files to modify:**
- `crates/control/src/lib.rs` — remove `mod auth_*; mod oauth;`
- `crates/control/src/main.rs` — remove `/auth/*` route registrations; keep just `/auth/callback` (from U7)
- `crates/control/src/api.rs` — if it referenced auth handlers, drop those references

- [ ] **Step U8.1:** `git rm` the three files.
- [ ] **Step U8.2:** Remove mod declarations.
- [ ] **Step U8.3:** Update routing.
- [ ] **Step U8.4:** Adjust `crates/control/src/main.rs`'s `AppState` to drop the `auth: AuthService` and `google_oauth: Option<GoogleConfig>` fields (the OIDC RP replaces both).
- [ ] **Step U8.5:** `cargo build --workspace`. Fix compile errors (anything that imported `auth_service::AuthService` etc. needs to migrate to `oidc_rp::OidcRp` or be removed).
- [ ] **Step U8.6:** Commit `control: delete legacy auth — auth_service.rs, auth_handlers.rs, oauth.rs (superseded by oidc_rp)`.

---

# Unit U9 · Legacy table migration

**File:** `crates/auth/src/store/migrations.rs` (add a one-shot migration block).

The legacy auth had `auth_users`, `auth_app_consents`, `auth_sessions` in the public schema. New schema is in `auth.*`. Migrate once, then drop.

- [ ] **Step U9.1:** Add to the migrations module:

```rust
/// One-shot migration from the legacy public-schema auth_users → auth.users.
/// Idempotent: skip if auth_users doesn't exist.
pub async fn migrate_legacy_auth_users(conn: &Client) -> Result<()> {
    // Check if public.auth_users exists.
    let rows = conn.query(
        "SELECT 1 FROM information_schema.tables WHERE table_schema = 'public' AND table_name = 'auth_users'",
        &[]
    ).await.map_err(|e| AuthError::Db(format!("legacy check: {e}")))?;
    if rows.is_empty() {
        return Ok(());
    }

    // Move the data.
    conn.batch_execute(
        "INSERT INTO auth.users (id, email, name, avatar_url, password_hash, email_verified_at, created_at, last_login_at)
         SELECT id, email::citext, name, avatar_url, password_hash,
                CASE WHEN email_verified THEN created_at ELSE NULL END,
                created_at, last_login
         FROM public.auth_users
         ON CONFLICT (email) DO NOTHING;

         DROP TABLE IF EXISTS public.auth_users CASCADE;
         DROP TABLE IF EXISTS public.auth_app_consents CASCADE;
         DROP TABLE IF EXISTS public.auth_sessions CASCADE;"
    ).await.map_err(|e| AuthError::Db(format!("legacy migrate: {e}")))?;

    Ok(())
}
```

- [ ] **Step U9.2:** Call from `migrate(&client)` after the new schema is in place.

- [ ] **Step U9.3:** Test against live PG with the legacy tables seeded:
```sql
CREATE TABLE public.auth_users (id UUID PRIMARY KEY DEFAULT gen_random_uuid(), email TEXT UNIQUE, name TEXT, avatar_url TEXT, password_hash TEXT, email_verified BOOLEAN, created_at TIMESTAMPTZ DEFAULT NOW(), last_login TIMESTAMPTZ);
INSERT INTO public.auth_users (email, name) VALUES ('legacy@test.com', 'Legacy User');
```
Then run migrations and verify the row landed in `auth.users` and the public table is gone.

- [ ] **Step U9.4:** Commit `auth: migrations — one-shot migrate auth_users → auth.users + drop legacy`.

---

# Unit U10 · `sdks/auth` reduction

**File:** `sdks/auth/src/index.ts`.

The current package has a browser fallback that reads `window.__zs_user`. The new model: gateway injects `ZeroShip-User` header → worker parses it → `env.auth.getUser()` is the only API. No browser fallback.

- [ ] **Step U10.1:** Read the current `sdks/auth/src/index.ts` to see what's there.
- [ ] **Step U10.2:** Delete the `window.__zs_user` fallback path. Keep only `getUser() / requireUser() / signOut()` that read from the platform-injected `env.auth.*` namespace.
- [ ] **Step U10.3:** Update `sdks/auth/package.json` if any browser-related fields point at the deleted path.
- [ ] **Step U10.4:** Run `pnpm test -F @zeroship/auth` (or whatever the package test invocation is). Fix any test that asserted on the deleted behavior.
- [ ] **Step U10.5:** Commit `sdks/auth: delete window.__zs_user browser fallback (gateway injects ZeroShip-User now)`.

---

# Unit U11 · Full-stack e2e test

**File:** `crates/auth/tests/e2e_full_stack.rs` (or in a sibling test crate — pick wherever the cross-binary tests live).

The test boots ALL the things (postgres, hydra, control, gateway, worker, auth) and drives an unauthenticated request to a creator app. Asserts the full redirect dance + worker invocation.

This is the proof Phase 3 works end-to-end. Without it, we'd be trusting that the integration tests in each binary cover the seam.

- [ ] **Step U11.1:** Boot the stack via docker compose (or in-process, but the wiring is complex enough that compose is probably more reliable).
- [ ] **Step U11.2:** Register a test creator app in the control plane (deploy a "hello world" worker bundle).
- [ ] **Step U11.3:** With a fresh cookie jar (no session), GET `http://localhost:8000/anything` with `Host: testapp.zeroship.ai`.
- [ ] **Step U11.4:** Follow the 302 chain: gateway → /oauth2/auth → /login → (sign up + log in via /login POST) → /consent → /token → /__zs/auth/callback → /anything.
- [ ] **Step U11.5:** The final response is the worker's "hello world" output, with the user identity available via `env.auth.getUser()` (which the test app can dump as JSON).
- [ ] **Step U11.6:** Commit `auth: e2e_full_stack — gateway+worker+auth+hydra full unauth-to-served path`.

This test is large (~400 LOC). Reuse Phase 2.5's `tests/common/` helpers heavily.

---

# Unit U12 · Update `docs/reference/auth.md`

Current `docs/reference/auth.md` describes the legacy in-process auth. After Phase 3 it's wrong.

- [ ] **Step U12.1:** Rewrite to describe the new architecture:
  - Hydra is the OIDC kernel
  - `crates/auth` owns identity/UX
  - Gateway is the default RP for hosted apps
  - Control plane is an RP for the dashboard
  - `__Host-zs_app_session` per-origin cookies
  - `env.auth.*` is the worker-side namespace
- [ ] **Step U12.2:** Commit `docs: reference/auth.md — rewrite for hydra-backed OIDC architecture`.

---

# Unit U13 · Phase 3 close-out

- [ ] **Step U13.1:** `cargo build --workspace` clean.
- [ ] **Step U13.2:** `cargo clippy -p zeroship-gateway -p zeroship-control -p zeroship-auth --no-deps --tests` — note any new warnings; goal is ≤10 total new across all three.
- [ ] **Step U13.3:** Full test suite green: `AUTH_DB_URL=... AUTH_HYDRA_ADMIN=... cargo test --workspace`.
- [ ] **Step U13.4:** Milestone empty commit:
```
auth: Phase 3 complete — gateway + control as OIDC RPs

Legacy creator-auth retired in the same PR. End-user signs in via
auth.zeroship.ai/login (Phase 2 password flow); both gateway (hosted
apps) and control plane (dashboard) speak full OIDC code+PKCE.
```
- [ ] **Step U13.5:** Tag `auth-phase-3` locally.

---

# Future phases (after Phase 3)

- **Phase 4** — Federation: Google + GitHub OAuth in `crates/auth`; third-party consent UI (replaces the §10.4 placeholder); account linking decision tree.
- **Phase 5** — Magic-link, email verification, password reset; mailer abstraction (lettre/resend/stdout) + bounce webhooks.
- **Phase 6** — JWK rotation cron; DPoP at the gateway; audit-log retention sweeper; load test; security review; `docs/runbooks/auth-deploy.md`.

---

# Self-review

**Spec coverage** — every proposal §11 row maps to a Phase 3 unit:
- §11.1 (retire legacy `crates/auth/`): done in Phase 1
- §11.2 (new crate): done in Phase 1+2
- §11.3 (hydra deployment): done in Phase 1
- §11.4 (auth.* schema): done in Phase 1+2; legacy migration in U9
- §11.5 (control plane OIDC RP): U7 + U8
- §11.6 (gateway OIDC RP): U2 + U3 + U4 + U5
- §11.7 (gateway proxy for hydra prefixes): U1
- §11.8 (sdks/auth reduction): U10
- §11.9 (skew fix): falls out of U6 + U8 (the legacy code that produced the skew is deleted)

**Placeholder scan** — no TBD/TODO/"fill in later" in unit task descriptions. Some implementation snippets are skeletal (e.g., U3's OidcRp::new body) — the implementer fills in by following the pattern from Phase 2's existing code.

**Type consistency** — `OidcRp`, `UserClaims`, `AppSession`, `JwksCache` are defined once each. The gateway's `UserClaims` after U3 matches the shape `encode_user_header` already expects (sub, email, name, avatar, email_verified). No drift.

**Scope** — Phase 3 ONLY ships RP migration. No federation, no magic-link, no MFA. The new RP-side code is bounded to the seam (~600 LOC of new gateway+control code, plus deletions of ~1100 LOC of legacy).

---

# Execution handoff

Plan saved to `docs/superpowers/plans/2026-05-27-auth-server-phase-3-oidc-rps.md`.

Execute via subagent-driven-development: one subagent per unit, with U1 split into 2 sub-commits, U3 into 2, U5 into 3. Reference this plan's unit section verbatim in each brief.

Stack must remain running throughout Phase 3 execution: `docker compose up -d` (the gateway, worker, control, auth, hydra, postgres). Tests will hit it at multiple points.
