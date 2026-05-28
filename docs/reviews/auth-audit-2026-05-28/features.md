# Auth System — Feature Inventory

**Date:** 2026-05-28
**Branch:** `proposal/auth-server` (HEAD `1269c0c2`, Phase 8 close-out)
**Scope:** every feature shipped through Phase 1–8

This is the canonical "what's in" list. Every row is shipped code, exercised by at least one test.

---

## 1. HTTP endpoints

### 1.1 `crates/auth` (IdP, default `0.0.0.0:9092`)

| Method | Path | Handler | Purpose | Cookies |
|---|---|---|---|---|
| GET | `/healthz` | server | Liveness | — |
| GET | `/readyz` | server | Readiness (DB+hydra) | — |
| GET | `/static/style.css` | server | Embedded stylesheet | — |
| GET | `/login` | `ui::login::get` | Render password form (needs `?login_challenge`) | Set: `__Host-zsidp_csrf` |
| POST | `/login` | `ui::login::post` | Verify password → `accept_login` → 302 hydra | Set: `__Host-zsidp_session`; clear: `csrf` |
| GET | `/signup` | `ui::signup::get` | Render signup form | Set: `__Host-zsidp_csrf` |
| POST | `/signup` | `ui::signup::post` | Create account → send verification email → 302 `/login` | Set: `csrf` |
| GET | `/consent` | `ui::consent::get` | Render consent (first-party auto-skip) | Set: `csrf` |
| POST | `/consent` | `ui::consent::post` | `accept_consent` on hydra → 302 | — |
| GET | `/forgot` | `ui::forgot::get` | Password-reset request form | Set: `csrf` |
| POST | `/forgot` | `ui::forgot::post` | Issue reset token + email | — |
| GET | `/reset?t=…` | `ui::reset::get` | Render new-password form if token valid | Set: `csrf` |
| POST | `/reset` | `ui::reset::post` | Update password, consume token, log audit | — |
| GET | `/verify?t=…` | `ui::verify::get` | Email-verification redemption (sets `email_verified_at`) | — |
| POST | `/magic/start` | `ui::magic::start` | Issue magic-link token + email | — |
| GET | `/magic/await?t=…` | `ui::magic::await_code` | Poll-style "wait for redemption" page | — |
| GET | `/magic/verify?t=…&email=…` | `ui::magic::verify` | Mark token redeemed | — |
| POST | `/magic/complete` | `ui::magic::complete` | Finalize login after verify | Set: `__Host-zsidp_session` |
| GET | `/link?token=…` | `ui::link::get` | Account-linking confirmation page | Set: `csrf` |
| POST | `/link` | `ui::link::post` | Attach federated identity to existing account | — |
| GET | `/me` | `ui::me::get` | Profile page (requires session) | — |
| POST | `/me/unlink/{provider}` | `ui::me::unlink` | Remove a federated identity | — |
| GET | `/oauth/google/start` | `ui::oauth_google::start` | Begin Google authcode flow (gated on `--google-client-id`) | Set: federation stash |
| GET | `/oauth/google/callback` | `ui::oauth_google::callback` | Receive code + state, exchange, link/sign up | Clear: stash; Set: `__Host-zsidp_session` |
| GET | `/oauth/github/start` | `ui::oauth_github::start` | Begin GitHub flow (gated on `--github-client-id`) | Set: stash |
| GET | `/oauth/github/callback` | `ui::oauth_github::callback` | Same shape as Google | — |
| POST | `/webhooks/postmark` | `ui::webhooks::postmark` | Bounce/complaint webhook (Basic auth) | — |
| POST | `/webhooks/ses-sns` | `ui::webhooks::ses_sns` | AWS SES bounce/complaint (RSA-SHA1 signed) | — |

### 1.2 `crates/gateway` (per-app host, default `*:80`)

| Method | Path | Handler | Purpose |
|---|---|---|---|
| GET | `/health` | inline | Liveness |
| GET | `/__zs/auth/callback?code=…&state=…` | `router::auth` | Receive OIDC code → exchange at hydra → set `__Host-zs_app_session` |
| POST | `/__zs/auth/dpop-exchange` | `dpop_exchange::handle` | Mint DPoP-bound wrapper JWT (Phase 8) |
| POST | `/oidc/backchannel-logout` | `backchannel_logout::configure` | OIDC BCL 1.0 receiver |
| (any) | `/{tail}*` | manifest dispatch | Catch-all to worker |

### 1.3 `crates/control` (`console.zeroship.ai`, default `*:9090`)

| Method | Path | Handler | Purpose |
|---|---|---|---|
| GET | `/auth/callback?code=…&state=…` | `api::auth_callback` | OIDC RP callback → `__Host-zs_console_session` |
| POST | `/oidc/backchannel-logout` | `backchannel_logout` | Console BCL receiver |
| (control-plane CRUD lives separately — not auth-relevant) |

---

## 2. Identity flows

### 2.1 Password (Phase 2)
- **Argon2id** hash with `Params::new(19456, 2, 1, None)` (OWASP 2026)
- Stored in `auth.users.password_hash` (PHC string format)
- Verification: constant-time via the `argon2` crate
- Strength check: ≥15 char minlength on signup form (UI-side); server doesn't gate length explicitly
- Rate-limited via `auth.rate_limits` token bucket

### 2.2 Email verification (Phase 5)
- Token: 32 random bytes → URL-safe base64
- Stored hashed (SHA-256) in `auth.email_verifications`
- TTL: 24 h; single-use; deleted on redemption
- Sets `auth.users.email_verified_at`

### 2.3 Password reset (Phase 5)
- Same token model as verification, separate table
- TTL: 1 h
- Single-use; on POST redeem, password updated + all sessions revoked

### 2.4 Magic link (Phase 5)
- Token issued, emailed
- Two redemption paths:
  - `GET /magic/verify?t=…&email=…` (link clicked) → marks redeemed
  - `POST /magic/complete` (different device polls `/magic/await`)
- TTL: 10 min; single-use

### 2.5 Google OAuth (Phase 4)
- Authcode + PKCE
- Stash cookie carries `state` + PKCE verifier (HMAC-signed via `--stash-signing-key`)
- ID token verified via Google's JWKS
- Email + `email_verified=true` required to auto-link
- Linker: if existing user with same verified email → link; else create + link

### 2.6 GitHub OAuth (Phase 4)
- Authcode + PKCE (GitHub supports PKCE since 2024)
- No ID token (GitHub is OAuth-only) — userinfo via `/user` + `/user/emails`
- Primary verified email used
- `name` falls back to `login` when null

### 2.7 Federation linker (Phase 4)
- `auth.identities` table: `(user_id, provider, provider_user_id)`
- Unique on `(provider, provider_user_id)`
- Unlinking last identity allowed (user can still log in via password if set)

---

## 3. Sessions

### 3.1 IdP login session — `__Host-zsidp_session`
- Set by `crates/auth` after successful auth
- Stored in `auth.sessions`
- Hard TTL: 12 h; idle TTL: 30 min (sliding)
- Used to skip re-auth across `hydra_login_request` lifecycle
- Single-row revoke on logout

### 3.2 Per-app gateway session — `__Host-zs_app_session`
- Set by gateway `/__zs/auth/callback` after token exchange
- Stored in `auth.gateway_sessions`
- TTL: 12 h
- Validated on every request; revoked by BCL or explicit signout
- Slides on use (every request bumps `last_seen_at`)

### 3.3 Console session — `__Host-zs_console_session`
- Set by `crates/control` `/auth/callback`
- Stored in `auth.console_sessions`
- TTL: 12 h; same lifecycle as gateway sessions

### 3.4 OIDC stash cookies — `__Host-zs_oidc_stash`, `__Host-zs_console_stash`
- Short-lived (10 min)
- HMAC-signed; carry state + PKCE verifier + original path
- Cleared on successful callback

---

## 4. Tokens

### 4.1 Hydra-issued (we consume)
- **ID token:** EdDSA JWT, 1 h TTL, RS256 mirror in JWKS for legacy clients
- **Access token:** RFC 9068 JWT (`access_token_strategy="jwt"`), EdDSA, 1 h
- **Refresh token:** Opaque, hydra-managed, 30 d sliding / 90 d hard, rotation+reuse-detect (30 s grace)
- **Authorization code:** PKCE-bound, 60 s, single-use
- **Logout token (BCL):** EdDSA JWT, hydra issues to RPs

### 4.2 Gateway-issued (Phase 8 wrappers)
- **Wrapper access token:** EdDSA JWT, 1 h, `typ=at+jwt`, RFC 9068-shaped
  - Claims: `iss`, `aud`, `sub`, `exp`, `iat`, `jti`, `cnf.jkt`, `scope`, `client_id`, `email`, `name`, `email_verified`, `wraps`
  - Signed with `--signing-key-file` Ed25519 key
- **DPoP proof:** ephemeral, client-generated, verified by `core::dpop`
- **HMAC `ZeroShip-User` header:** SHA-256(worker_key, JSON({id,email,name,…})) → forwarded to worker

---

## 5. Cron jobs

| Job | File | Cadence | Purpose |
|---|---|---|---|
| JWK rotation | `cron/jwk_rotation.rs` | Configurable (default 7 d) | Publish new EdDSA + RS256 keys to hydra's `id-token` and `access-token` sets, retire old |
| Audit retention | `cron/audit_retention.rs` | Configurable (default 90 d) | DELETE FROM `auth.audit_events` WHERE `occurred_at < now() - INTERVAL` |
| DPoP jti replay-cache sweep | (in-process, gateway) | per-insert | TTL-based eviction in `crates/core::dpop` |

---

## 6. Mailer surface

`crates/auth/src/mailer/`:
- **stdout** — dev backend, prints to logs
- **smtp** — lettre's blocking transport, gated behind `tokio::task::spawn_blocking` (the one tokio touchpoint, by design)
- **resend** — Resend HTTP API
- **sns** — AWS SES via SNS (raw HTTP API, no SDK)
- **bounce.rs** — suppression-list logic
- **templates** — askama HTML templates (`verify.html`, `forgot.html`, `magic.html`, …)

Each backend implements the `Mailer` trait. Selected at boot via `--mailer-backend`.

---

## 7. Data model

`auth.*` schema in shared Postgres (migrations in `crates/auth/src/store/migrations.rs`):

| Table | Purpose |
|---|---|
| `auth.users` | Single global user pool |
| `auth.identities` | Federation provider linkages |
| `auth.sessions` | IdP login sessions |
| `auth.gateway_sessions` | Per-app sessions |
| `auth.console_sessions` | Dashboard sessions |
| `auth.magic_links` | Magic-link tokens (hashed) |
| `auth.email_verifications` | Email-verification tokens (hashed) |
| `auth.password_resets` | Password-reset tokens (hashed) |
| `auth.email_suppressions` | Mailer bounce/complaint list |
| `auth.rate_limits` | Per-(ip,bucket) token bucket state |
| `auth.audit_events` | Structured event log |

Hydra's `hydra_*` tables live in the same database (managed by `hydra migrate sql up`).

---

## 8. Configuration surface (`AuthConfig`)

The full flag list lives in `crates/auth/src/config.rs`. Notable:

- **Required:** `--db-url`
- **Hydra:** `--hydra-admin`, `--hydra-public`
- **Bootstrap:** `--bootstrap`, `--clients-config`
- **Cookies/dev:** `--insecure-dev`
- **Stash HMAC:** `--stash-signing-key`
- **Google:** `--google-client-id`, `--google-client-secret`, `--google-redirect-uri`
- **GitHub:** `--github-client-id`, `--github-client-secret`, `--github-redirect-uri`
- **Mailer:** `--mailer-backend`, `--smtp-host`, `--smtp-user`, `--smtp-password`, `--from-address`, `--resend-api-key`, `--ses-region`, `--postmark-webhook-secret`
- **SNS:** `--sns-topic-arn`
- **Crons:** `--jwk-rotation-interval-days`, `--audit-retention-days`

---

## 9. Built-in security defenses

| Defense | Where |
|---|---|
| CSRF token (form+cookie, constant-time compare) | `crates/auth/src/csrf.rs` |
| Security headers (CSP, X-Frame-Options, COOP, Permissions-Policy, HSTS) | `crates/auth/src/headers.rs` |
| Rate limiting (token bucket, per-IP + per-account) | `crates/auth/src/ratelimit.rs` |
| Audit logging | `crates/auth/src/audit.rs` + `store/audit.rs` |
| Argon2id password hashing | `crates/auth/src/identity/password.rs` |
| Constant-time HMAC | `core::auth` helpers (`subtle::ConstantTimeEq`) |
| DPoP `cnf.jkt` enforcement | `crates/gateway/src/router/auth.rs::resolve_dpop_user_header` |
| jti replay cache (DPoP) | `crates/core::dpop` |
| OIDC `state` HMAC binding | `crates/gateway/src/oidc_rp.rs`, `crates/control/src/oidc_rp.rs` |
| BCL `logout_token` signature + `jti` cache | `crates/core::logout_token` |
| RFC 9700 redirect URI exact-match | Hydra (we delegate) |
| `__Host-` cookie prefix (production) | All session/stash cookies |
| Email suppression on bounce/complaint | `crates/auth/src/mailer/bounce.rs` |
| Webhook signature verification (Postmark Basic, SNS RSA-SHA1) | `crates/auth/src/ui/webhooks.rs` |

---

## 10. SDK surface

`@zeroship/auth` (npm) — `sdks/auth/src/index.ts`:
- `auth.getUser()` — reads `env.auth.user`
- `auth.requireUser()` — throws 401-shaped Error if anonymous
- `auth.isLoggedIn()`
- `auth.signOut(returnTo?)` — returns a `Response(302)`

`env.auth.user` is populated by the runtime from the gateway's HMAC `ZeroShip-User` header. No direct hydra calls from user code.
