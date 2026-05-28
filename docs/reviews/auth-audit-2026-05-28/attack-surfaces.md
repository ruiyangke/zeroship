# Auth System — Attack Surface Inventory

**Date:** 2026-05-28
**Scope:** every input that crosses a trust boundary in the auth system

This is the threat-modeling companion to `features.md`. Each surface lists the entry, who can reach it, what they can pass, and the relevant defenses.

---

## A. Externally reachable HTTP surfaces (Internet → us)

### A.1 IdP endpoints on `auth.zeroship.ai` (proxied through gateway, but logically internet-facing)

| Surface | Reachable by | Untrusted inputs | Defenses |
|---|---|---|---|
| `GET /login` | Anonymous | `login_challenge` query | Hydra validates challenge; CSRF cookie set |
| `POST /login` | Anonymous | Form body (`email`, `password`, `csrf`, `login_challenge`) | CSRF compare, Argon2id, rate limit, audit log |
| `GET /signup` | Anonymous | `login_challenge` (optional) | CSRF cookie set |
| `POST /signup` | Anonymous | Form body (`name`, `email`, `password`, `csrf`) | CSRF compare, rate limit, suppression-list check, audit |
| `GET /consent` | Anonymous w/ challenge | `consent_challenge` | Hydra validates; first-party auto-skip |
| `POST /consent` | Anonymous w/ challenge | Form body + cookies | Hydra accept_consent |
| `GET /forgot` | Anonymous | — | CSRF set |
| `POST /forgot` | Anonymous | `email`, `csrf` | Privacy-preserving 200 (don't leak account existence), rate limit, suppression check |
| `GET /reset?t=…` | Anonymous w/ token | `t` (reset token) | Token hash lookup, TTL, single-use |
| `POST /reset` | Anonymous w/ token | `t`, `password`, `csrf` | Token consume in tx, revoke all sessions, audit |
| `GET /verify?t=…` | Anonymous w/ token | `t` (verification token) | Token hash lookup, TTL, single-use, set `email_verified_at` |
| `POST /magic/start` | Anonymous | `email`, `csrf` | Rate limit, privacy-preserving response |
| `GET /magic/await?t=…` | Anonymous w/ token | `t` | Poll-style — short TTL |
| `GET /magic/verify?t=…&email=…` | Anonymous w/ token | `t`, `email` | Token consume, audit |
| `POST /magic/complete` | Same browser as /start | Cookies + form | TTL check, single-use |
| `GET /link?token=…` | Anonymous w/ token | `token` | TTL, scope=link |
| `POST /link` | Anonymous w/ token | `token`, `csrf` | Consume token in tx |
| `GET /me` | Session holder | Cookie | Session validation, 302 to /login if absent |
| `POST /me/unlink/{provider}` | Session holder | Cookie + CSRF + path param | CSRF + session + "not last identity" guard |
| `GET /oauth/google/start` | Anonymous | — (sets stash) | Gen state+PKCE, HMAC-sign stash |
| `GET /oauth/google/callback` | Anonymous w/ code | `code`, `state` | State HMAC verify, PKCE verify, ID token verify, email-collision guard |
| `GET /oauth/github/{start,callback}` | Same as Google | Same shape | Same defenses; GitHub-specific verified-email check |
| `POST /webhooks/postmark` | Postmark IPs | Webhook JSON | Basic auth header (shared secret) |
| `POST /webhooks/ses-sns` | AWS SNS | SNS JSON | RSA-SHA1 signature verify against cert at allow-listed URL |

### A.2 Gateway endpoints on `*.zeroship.ai`

| Surface | Reachable by | Inputs | Defenses |
|---|---|---|---|
| `GET /__zs/auth/callback` | Anonymous w/ code | `code`, `state`, cookies | State HMAC verify, PKCE, ID-token verify, set session cookie |
| `POST /__zs/auth/dpop-exchange` | Holder of hydra access token + DPoP key | `Authorization: Bearer`, `DPoP:` header | Proof verify (sig, htm, htu, iat, jti, ath), hydra introspect, mint wrapper |
| `POST /oidc/backchannel-logout` | Hydra (in theory anyone) | `logout_token` JWT | Signature verify, issuer/aud check, jti cache, sub/sid extract, revoke sessions |
| `Authorization: DPoP <token>` on any request | Wrapper/raw-token holder | wrapper token + proof | Local wrapper verify → cnf.jkt enforce; raw-hydra fallback (Bearer-class) |

### A.3 Control plane endpoints on `console.zeroship.ai`

| Surface | Reachable by | Inputs | Defenses |
|---|---|---|---|
| `GET /auth/callback` | Anonymous w/ code | `code`, `state`, cookies | Same as gateway callback, mints `__Host-zs_console_session` |
| `POST /oidc/backchannel-logout` | Hydra | logout_token | Same as gateway BCL |

### A.4 Hydra's own surface (we proxy)

| Surface | Reachable by | Inputs | Defenses |
|---|---|---|---|
| `/oauth2/auth` | Anonymous | OIDC authorize params | Hydra |
| `/oauth2/token` | RP (gateway/control) w/ client_secret | code, verifier, etc. | Hydra |
| `/oauth2/introspect` | RP | bearer | Hydra |
| `/oauth2/revoke` | RP | token | Hydra |
| `/oauth2/sessions/logout` | Anonymous | id_token_hint, post_logout_redirect_uri | Hydra |
| `/userinfo` | Bearer | — | Hydra |
| `/.well-known/{openid-configuration,jwks.json,oauth-authorization-server}` | Anyone | — | Public discovery |

---

## B. Inter-service surfaces (us → hydra → us)

| Surface | Direction | Trust assumption | Risk |
|---|---|---|---|
| `crates/auth` → hydra admin (`:4445`) | Inside trust boundary | Network-loopback or firewalled | Anyone with admin-port access can accept_login arbitrary sessions |
| Gateway → hydra introspect (Phase 7-style raw bearer) | Same | Hydra trusted for `active` field | A compromised hydra grants full user impersonation |
| Hydra → gateway BCL (`/oidc/backchannel-logout`) | Inbound from hydra | Verified by `logout_token` signature | Logout-token replay (mitigated by `jti` cache + audience) |
| Hydra → control BCL | Same | Same defenses | Same |

---

## C. Crypto surfaces

| Operation | Where | Key | Failure modes |
|---|---|---|---|
| Argon2id password hash | `identity/password.rs` | per-password | Weak params would let GPU crackers win; current is OWASP 2026 (m=19456 KiB, t=2, p=1) |
| HMAC-SHA256 (CSRF cookie, stash, ZeroShip-User) | `csrf.rs`, `oauth_stash.rs`, `core::auth` | per-deployment | Default keys → forge anything; `--stash-signing-key` warns at boot if default |
| Ed25519 wrapper-token signing | `gateway/signing.rs` | `--signing-key-file` | Key leak = mint arbitrary wrappers; no rotation in v1 |
| RFC 7638 JWK thumbprint (`cnf.jkt`) | `core::dpop`, `gateway/signing.rs` | derived | Wrong canonicalization = mismatched thumbprints, blocked legit clients |
| EdDSA / RS256 ID-token verification | `core::oidc_verify` | hydra JWKS | Trusting `alg: none` would let unsigned tokens through; verify allowlist is hardcoded |
| Hydra logout_token signature | `core::logout_token` | hydra JWKS | Same alg-confusion risk |
| RSA-SHA1 SNS webhook signature | `mailer/sns.rs` | AWS cert | Wrong cert URL allowlist = MITM bypass |

---

## D. Data inputs that cross trust

| Input | From | Goes to | Sanitization |
|---|---|---|---|
| User email | Anonymous form | DB (CITEXT), email body | Lowercased server-side; CITEXT cast in queries; templates auto-escape (askama) |
| User name | Anonymous form | DB, audit, email body, ZeroShip-User HMAC | Length-bounded; templates escape |
| `state`/PKCE | URL params + cookies | OIDC exchange | HMAC verify before use |
| Provider `email`/`name` | IdP (Google/GitHub) | DB, linker | `email_verified` checked; conflict resolution explicit |
| Webhook payload | Postmark/SNS | Mailer suppression list | Signature verify first; body parsed only after auth |
| `logout_token` | Hydra | BCL handler | Signature + claims; subject/sid extracted; sessions revoked |
| DPoP proof | Anonymous over the wire | Header verifier | Full RFC 9449 check (sig, htm, htu, iat, jti, ath, cnf.jkt) |
| Hydra access token | Anonymous | Bearer check / introspect | Signature via hydra JWKS or introspect endpoint |
| `code` + `state` | Browser callback | Token exchange | State HMAC verify; code single-use at hydra |
| Cookie values | Browser | Session lookup | All session cookies validated against DB (the cookie is just an ID) |

---

## E. State machines & race conditions

| Surface | Concurrency risk |
|---|---|
| Signup → email exists check → INSERT | Race: two requests with same email get past existence check; the UNIQUE constraint catches it but the second sees a server error. Audit: `signup_collision` event ideally. |
| Magic-link redeem | Two clicks of the link → DELETE…RETURNING idempotency; second click 404s cleanly |
| Password reset redeem | Same |
| OAuth callback with reused state | State stash cookie is single-use (cleared on success); reuse rejected at HMAC step |
| BCL jti replay | In-process LRU cache; horizontal scaling = cache miss on second node → second logout succeeds redundantly. **Should be PG-backed.** |
| Wrapper token replay (without DPoP key) | cnf.jkt enforcement at dispatch; useless without private key |
| DPoP jti replay | In-process LRU cache; same horizontal-scale issue as BCL |
| JWK rotation | Two auth instances rotating concurrently — needs single-writer lock |
| Session validation + slide | Read → update of `last_seen_at` not transactional; benign drift |
| Suppression-list check + send email | TOCTOU: bounce arrives between check and send. Acceptable. |

---

## F. Side-channel risks

| Surface | Side channel | Mitigation |
|---|---|---|
| Password verify | Timing: short-circuit on missing-user vs hash-compare | `argon2::verify_password` is constant-time per-call, but the missing-user path returns instantly — measurable. Audit if leakage exploitable. |
| CSRF compare | Timing | `subtle::ConstantTimeEq` (TBD — needs verification in code review) |
| HMAC verify | Timing | Same |
| Login outcome | Error message reveals "user not found" vs "wrong password" | Both should map to one generic message |
| Account enumeration via `/forgot` | "Account exists" leak | Privacy-preserving 200 response regardless of existence |
| Account enumeration via `/signup` | "Email taken" leak | Same generic message, send-an-email-if-existing pattern |
| `iat` of issued tokens | Reveals server activity | Acceptable |
| Audit log writes | Disk-fill DoS | Retention cron mitigates; should also size-limit per request |

---

## G. Open admin / privileged paths

| Path | Who | Note |
|---|---|---|
| Hydra admin (`:4445`) | Anyone with network access | Must be loopback/firewalled in prod |
| Postgres direct (`:5441` or wherever) | DB credentials holder | Same |
| `--bootstrap` flag | Operator | Should only run on first boot; idempotent if rerun |
| `--insecure-dev` flag | Operator | Drops `Secure` flag, weakens HSTS; never set in prod |
| `auth.rate_limits` table | Anyone with DB write | Rate limits become advisory |

---

## H. DoS surfaces

| Surface | Resource | Mitigation |
|---|---|---|
| Argon2id verify | CPU | Rate limit per-IP + per-account |
| Email send (verify, reset, magic) | Mailer quota | Rate limit per-email-address |
| OAuth start | Hydra admin RPS | Indirect — rate limit on auth |
| BCL logout | DB writes per token | jti cache + audience filter |
| DPoP proof verify | CPU per request | Cheap (Ed25519 verify) but multiplied per dispatch |
| Wrapper token verify | CPU per request | Local Ed25519 verify — cheaper than introspection |
| Audit log inserts | DB writes | Retention cron + DB capacity |

---

## I. Classes of attack to actively model

1. **Session fixation** — attacker pre-creates a session, victim logs in to attacker's session → attacker has access. Mitigation: rotate session ID on auth state change.
2. **CSRF on auth forms** — same-site Lax cookies + CSRF token + Origin check.
3. **Open redirect** — `redirect_to`, `post_logout_redirect_uri`, `return_to`. Must be allow-listed.
4. **OAuth mixup** — Wrong-IdP token replay. Mitigation: bind state to issuer (RFC 9207); verify id_token iss.
5. **Account takeover via email collision** — Sign up with victim's email at provider before victim links → preempt their account. Mitigation: require email-verified=true + don't auto-link to unverified-email accounts.
6. **Password spray** — many low-frequency attempts across many accounts. Per-IP and global rate limits, lockout on threshold.
7. **Credential stuffing** — leaked-credential reuse. HIBP-style breach check is out of scope but the surface exists.
8. **Token-replay attacks** — DPoP jti / BCL jti / authcode replay. Each has a single-use cache.
9. **Downgrade attacks** — wrapper-bound → raw-bearer fallback. Must NOT happen for verified-wrapper-with-mismatched-jkt. (Today's v1: falls back only for non-wrapper tokens.)
10. **Algorithm confusion** — RS256 vs HS256 with public key as HMAC secret. Verify libs accept only the algorithm declared in JWKS.
11. **Clickjacking** — login form in iframe. `X-Frame-Options: DENY` + CSP `frame-ancestors 'none'`.
12. **MIME sniffing** — `X-Content-Type-Options: nosniff`.
13. **Referer leak of tokens** — `Referrer-Policy: no-referrer`.
14. **HSTS rollback** — preload list reliance. Don't include `preload` directive on dev origins.
15. **Cookie tossing** — sibling subdomain sets a cookie that shadows our auth cookie. Mitigation: `__Host-` prefix (currently broken in insecure_dev mode — R0 fix).
16. **Subdomain takeover** — DNS for `*.zeroship.ai` left dangling. Operations concern, not code.
17. **Email injection** — CRLF in subject/from. Mailer must sanitize.
18. **JWK rotation race** — two instances rotate at once, both publish, retire each other's new key. Single-writer lock needed.
19. **Replay across BCL nodes** — in-process jti cache misses on horizontal scale. PG-backed cache or sticky routing.
20. **Audit-log disk fill** — retention cron required; size limits per record.
