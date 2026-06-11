# Auth

Zeroship's authentication is an OpenID Connect 1.0 / OAuth 2.1 deployment with two cooperating components:

- **`crates/auth`** — login UI, identity flows (password, Google/GitHub federation, magic-link, email verification, password reset), user store, audit log, hydra admin client.
- **`oryd/hydra`** — the OIDC kernel (sidecar). Issues ID tokens, access tokens, and refresh tokens; owns the OAuth/OIDC protocol surface. We do not own the protocol implementation.

Every authenticated zeroship surface is an OIDC Relying Party (RP) of `auth.zeroship.ai`:

- **The gateway** runs the RP flow on behalf of every hosted creator app at `*.zeroship.ai`.
- **The console** (creator dashboard) at `console.zeroship.ai` is itself a hosted app — it gets the same gateway RP flow as every other `*.zeroship.ai` host; the control plane does not run its own RP.

This means: one identity per human, one global user pool, one place that holds raw credentials.

## Topology

```
End user
   │
   │  GET https://myapp.zeroship.ai/anything
   ▼
┌──────────────┐    no __Host-zeroship_app_session cookie → 302 to /oauth2/auth
│   gateway    │ ─────────────────────────────────────────────────────────┐
└──────────────┘                                                          │
       │ also proxies auth.zeroship.ai/{oauth2,.well-known,userinfo}/*    │
       ▼ to hydra; everything else at that host to crates/auth            │
┌──────────────┐  302 to /login   ┌──────────────┐                        │
│    hydra     │ ───────────────► │  crates/auth │  password verify       │
│ public :4444 │ ◄── accept_login─│   /login     │  + zeroship.idp_sessions│
│ admin  :4445 │                  └──────────────┘                        │
└──────────────┘                                                          │
   │                                                                      │
   │ 302 to https://myapp.zeroship.ai/__zeroship/auth/callback?code=…&state=…   │
   ▼                                                                      │
   gateway exchanges code at /oauth2/token, validates the ID token,      │
   inserts zeroship.gateway_sessions, sets __Host-zeroship_app_session,        │
   then proxies to the worker with ZeroShip-User (HMAC-signed) ◄──────────┘
```

The console at `console.zeroship.ai` is deployed as a regular hosted app (seeded by control's `--bootstrap-console`), so the same gateway flow above covers it — there is no separate control-plane RP.

## Endpoints

### Owned by hydra (proxied through the gateway at `auth.zeroship.ai`)

| Path | Notes |
|---|---|
| `/oauth2/auth` | OIDC authorize endpoint |
| `/oauth2/token` | Code exchange |
| `/oauth2/revoke` | RFC 7009 revocation |
| `/oauth2/introspect` | RFC 7662 introspection |
| `/oauth2/sessions/logout` | RP-initiated logout |
| `/userinfo` | OIDC UserInfo |
| `/.well-known/openid-configuration` | Discovery |
| `/.well-known/oauth-authorization-server` | RFC 8414 metadata |
| `/.well-known/jwks.json` | Signing keys (EdDSA + RS256) |

### Owned by `crates/auth` (also on `auth.zeroship.ai`, behind the gateway)

Grouped by flow. All registered in `crates/auth/src/server.rs::configure`.

**Password login + account**

| Method | Path | Purpose |
|---|---|---|
| GET, POST | `/login?login_challenge=…` | Render password form / accept_login against hydra |
| GET, POST | `/signup?login_challenge=…` | Create account (carries challenge if entered mid-flow) |

**Federation** — `/oauth/google/*` registered only when Google OIDC creds are configured (`google_enabled`); `/oauth/github/*` only when a GitHub client id is set (`github_enabled`). No upstream creds ⇒ no upstream route.

| Method | Path | Purpose |
|---|---|---|
| GET | `/oauth/google/start` | Begin Google OIDC dance (sets `__Host-zsidp_google_stash`) |
| GET | `/oauth/google/callback` | Google callback; mints/links identity |
| GET | `/oauth/github/start` | Begin GitHub OAuth dance (sets `__Host-zsidp_github_stash`) |
| GET | `/oauth/github/callback` | GitHub callback; mints/links identity |
| GET, POST | `/link` | Link a federated identity to an existing account via a pending token issued by a callback (always registered) |

**Magic-link** — universal, always registered.

| Method | Path | Purpose |
|---|---|---|
| POST | `/magic/start` | Issue a magic-link token + email it (sets `__Host-zsidp_magic_csrf`) |
| GET | `/magic/await` | Poll page while the link is clicked elsewhere |
| GET | `/magic/verify` | Landing for the emailed link; same-device vs cross-device branch |
| POST | `/magic/verify/redeem` | Cross-device redeem confirmation |
| POST | `/magic/complete` | Consume the token, establish the IdP session |

**Email verification + password reset**

| Method | Path | Purpose |
|---|---|---|
| GET | `/verify` | Landing for the signup email-verification token |
| POST | `/verify/redeem` | Consume the token; sets `zeroship.users.email_verified_at` |
| GET, POST | `/forgot` | Issue a 1 h reset token (enumeration-resistant) |
| GET, POST | `/reset` | Redeem the reset token; update `zeroship.users.password_hash` |

**Profile + logout**

| Method | Path | Purpose |
|---|---|---|
| GET | `/me` | Signed-in user's profile page (requires `__Host-zsidp_session`) |
| POST | `/me/unlink/{provider}` | Unlink a federated identity |
| GET, POST | `/logout` | RP-initiated logout confirmation (hydra's `urls.logout` points here) |

**Consent + device**

| Method | Path | Purpose |
|---|---|---|
| GET | `/consent?consent_challenge=…` | First-party skip-consent fast path |
| POST | `/consent/accept` | Accept the consent challenge against hydra |
| POST | `/consent/deny` | Deny the consent challenge |
| GET, POST | `/device` | OAuth 2.0 device-authorization user-code entry |

**Webhooks** — always registered; each handler authenticates per request (Basic-auth for Postmark/relay, RSA-SHA1 signature for SES-SNS) and rejects misrouted/unauthenticated traffic rather than 404ing.

| Method | Path | Purpose |
|---|---|---|
| POST | `/webhooks/postmark` | Postmark bounce/complaint receiver |
| POST | `/webhooks/ses-sns` | SES → SNS bounce/complaint receiver |
| POST | `/webhooks/relay-inbound` | Postmark Inbound parsed-mail receiver (relay forwarding) |

**Health + static**

| Method | Path | Purpose |
|---|---|---|
| GET | `/healthz`, `/readyz` | Health checks |
| GET | `/static/style.css` | Bundled login-UI stylesheet (compiled into the binary) |

The design rationale for these flows lives in `docs/archive/auth-server.md` §6.

### Owned by `crates/gateway` (per hosted-app host)

| Method | Path | Purpose |
|---|---|---|
| GET | `/__zeroship/auth/callback?code=…&state=…` | Receives the OIDC code, exchanges at hydra's `/oauth2/token`, sets `__Host-zeroship_app_session`, redirects to the original path |
| POST | `/oidc/backchannel-logout` | OIDC BCL 1.0 receiver — verifies a `logout_token` and revokes the subject's gateway sessions. |

## Session cookies

All cookies are `HttpOnly`, `SameSite=Lax`, `Path=/`. `Secure` is set unless the process is running with `insecure_dev = true` (localhost). The `__Host-` prefix (RFC 6265bis §4.1.3) requires `Path=/`, no `Domain=`, and `Secure` — the dev override drops `Secure` only.

Two cookies — the anchor (`__Host-zeroship_app_anchor`) and the federation/magic stashes — use `SameSite=Strict`/`Lax` deliberately as noted below; the table calls out where the default does not apply.

| Cookie | Set by | Host | Max-Age |
|---|---|---|---|
| `__Host-zsidp_session` | `crates/auth` | `auth.zeroship.ai` | 12 h hard / 30 min idle |
| `__Host-zsidp_csrf` | `crates/auth` | `auth.zeroship.ai` | per-form |
| `__Host-zsidp_google_stash` | `crates/auth` | `auth.zeroship.ai` | 10 min (600 s) — pending the Google OIDC dance |
| `__Host-zsidp_github_stash` | `crates/auth` | `auth.zeroship.ai` | 10 min (600 s) — pending the GitHub OAuth dance |
| `__Host-zsidp_magic_csrf` | `crates/auth` | `auth.zeroship.ai` | 15 min (900 s) — matches the magic-link token TTL |
| `__Host-zeroship_app_session` | gateway | each `*.zeroship.ai` app origin | 12 h (43200 s) |
| `__Host-zeroship_app_anchor` | gateway | each app origin | 30 d (2592000 s) — `SameSite=Strict`; SDK reload-recovery anchor (distinct table from the session cookie) |
| `__Host-zs_oidc_stash` | gateway | each app origin | 10 min (600 s) — pending the OIDC redirect |
| `__Host-zs_console_session` | control | `console.zeroship.ai` | 12 h |
| `__Host-zs_console_stash` | control | `console.zeroship.ai` | 10 min |
| `ory_hydra_session_*` | hydra | `auth.zeroship.ai` | hydra-configured |

The `*_stash` cookies carry the HMAC-signed PKCE verifier + state + original request path so the callback can complete the dance without server-side scratch storage. They are cleared on a successful callback. The federation stashes (`__Host-zsidp_{google,github}_stash`) get a separate cookie name per provider so concurrent dances in different tabs don't clobber each other. The magic-link CSRF cookie pins the redeem to the originating device. In `insecure_dev` (localhost), each of these drops the `Secure` flag and the `__Host-` prefix together (e.g. `zeroship_app_anchor`, `zsidp_magic_csrf`), per RFC 6265bis §4.1.3.2.

## SDK surface — `@zeroship/auth`

The npm package exposes:

- `auth.getUser()` — the authenticated user, or `null` if anonymous
- `auth.requireUser()` — the user, or throws a 401-shaped Error
- `auth.isLoggedIn()` — convenience boolean
- `auth.signOut(returnTo?)` — returns a 302 `Response` to `/__zeroship/auth/signout`

All read from `env.auth.user`, populated by the runtime from the gateway's HMAC-signed `ZeroShip-User` request header. The gateway is the single source of truth — the previous `window.__zs_user` browser fallback is gone.

```javascript
import { auth } from "@zeroship/auth";

export default {
  async fetch(req, env) {
    const user = auth.requireUser();
    return Response.json({ hello: user.name ?? user.id });
  }
};
```

Client code that needs the user calls back through a server fetch handler or RPC.

## Token formats

All token operations live in hydra; the platform just consumes what it issues.

- **ID token:** EdDSA-signed JWT, 1 h TTL. Standard OIDC claims (`iss`, `sub`, `aud`, `iat`, `exp`, `nonce`, `acr`, `amr`) plus optional `email`, `email_verified`, `name`, `picture`. RS256 also published in JWKS for back-compat with non-EdDSA clients.
- **Access token:** RFC 9068 JWT (per-client `access_token_strategy = "jwt"`), EdDSA-signed, 1 h TTL.
- **Refresh token:** Opaque, hydra-managed, 30 d sliding / 90 d hard, rotated with reuse detection (30 s grace window).
- **Authorization code:** Single-use, PKCE-bound, 60 s TTL.

The canonical issuer URL is `https://auth.zeroship.ai/` (trailing slash; matches `urls.self.issuer` in `ops/hydra.yaml`).

## OIDC clients (relying parties)

Declared in TOML and reconciled into hydra's `/admin/clients` registry at boot. See `ops/auth-clients.example.toml`. Two first-party clients ship at v1:

- `console.zeroship.ai` — creator dashboard. Fixed `redirect_uris`.
- `gateway` — fans out per-hosted-app callbacks. The control plane appends one `https://<app>.zeroship.ai/__zeroship/auth/callback` to `redirect_uris` per deploy, via `PUT /admin/clients/gateway`. Exact-string match (RFC 9700 §4.1) — no wildcards.

Adding or rotating a client is an edit to the TOML; `crates/auth/src/bootstrap` upserts on startup.

## Data model

The auth tables live in the single `zeroship` PostgreSQL schema (owned by Liquibase — `db/changelog/changesets/0002_auth.sql` and follow-ups; see [Database migrations](../runbooks/db-migrations.md)):

| Table | Purpose |
|---|---|
| `zeroship.users` | Single global user pool |
| `zeroship.federated_identities` | Federation provider linkages (Google/GitHub) |
| `zeroship.idp_sessions` | `crates/auth`'s IdP login session (`__Host-zsidp_session`) |
| `zeroship.gateway_sessions` | Per-app sessions for hosted creator apps |
| `zeroship.magic_links` | Magic-link tokens, plus password-reset tokens (`purpose='reset'`) |
| `zeroship.magic_completions` | Cross-device magic-link completion handshakes |
| `zeroship.email_verifications` | Email-verification tokens |
| `zeroship.email_suppressions` | Mailer bounce/complaint list |
| `zeroship.rate_limits` | Login throttling state |
| `zeroship.audit_events` | Structured event log |
| `zeroship.dpop_jti` / `zeroship.token_revocations` | DPoP replay + revocation state |
| `zeroship.jwk_key_state` / `zeroship.cron_state` | Key rotation + cron bookkeeping |

Hydra's own tables live in the separate `oauth_hydra` schema (Liquibase changeset `0027` pre-creates the schema + least-privilege role; the `hydra-migrate` one-shot populates it — not `crates/auth`).

## Operator notes

Config-driven; the two files that matter:

- `ops/hydra.yaml` — hydra runtime config (issuer URL, cookie domain, TTLs, login/consent/error URLs)
- `ops/auth-clients.example.toml` — declared OIDC clients

Bring up the stack:

```bash
docker compose up -d postgres
docker compose run --rm hydra-migrate
docker compose up -d hydra auth gateway control worker
```

For local development against hydra, run `crates/auth` with `AUTH_BOOTSTRAP=1 AUTH_INSECURE_DEV=1` on first boot. Bootstrap is idempotent: it ensures hydra has EdDSA + RS256 keys in the `hydra.openid.id-token` set and EdDSA in the `hydra.jwt.access-token` set, then reconciles the client registry from the TOML.

## DPoP-bound access (non-browser clients)

Non-browser OAuth clients (CLI, server-to-server) that hold a hydra access token MAY present it RFC 9449 sender-constrained as `Authorization: DPoP <hydra_access_token>` plus a `DPoP:` proof header. The browser SPA does NOT use this path — it rides the signed `__Host-zeroship_app_session` cookie.

### Dispatch enforcement

When the gateway sees `Authorization: DPoP <token>` on an inbound request:

1. Verify the inbound `DPoP` proof (signature, `htu`, `htm`, `iat`, `ath = SHA-256(access_token)`, `jti` replay against an in-process 120 s cache).
2. Introspect the access token at hydra's `/oauth2/introspect`; reject when `active: false`.
3. Bind per-app: the introspected `client_id` MUST equal the route's `oauth_client_id` (closes cross-app token confusion — a token active for app A presented at app B's host is rejected). There is **no `cnf.jkt`** enforcement: hydra does not issue `cnf.jkt`-bound access tokens, so the binding is proof-of-possession (the proof's `ath` pins the proof to this specific token) plus the per-app `client_id` check.
4. Project the introspected global UUID `sub` to the per-app pairwise `pws_…`, enforce the per-app family-marker revocation (`auth.token_revocations`), then build the `ZeroShip-User` header.

A plain `Authorization: Bearer <hydra_access_token>` (no proof) is also accepted for non-browser clients on the raw-Hydra Bearer arm: the gateway verifies the access JWT locally against hydra's JWKS, binds per-app on the `client_id` claim, and runs the same family-marker revocation.

### Security model

| Token | Binding | Verifier | Notes |
|---|---|---|---|
| Hydra access token (`Authorization: Bearer`) | per-app `client_id` claim | Local JWKS verify + family-marker revocation | Compromise of the token alone is enough to impersonate the user |
| Hydra access token + DPoP proof (`Authorization: DPoP <hydra>`) | DPoP proof verified + per-app `client_id` (no `cnf.jkt`) | Hydra introspection + `core::dpop` + family-marker revocation | Proof verification adds replay protection but the token is not pinned to a key |

### Operator notes

- The gateway needs an Ed25519 signing key for the signed session cookie. Generate with `openssl genpkey -algorithm ed25519 -out gw.key` and pass via `--signing-key-file` (or `GATEWAY_SIGNING_KEY_FILE`). PKCS#8 PEM or DER both work. Without it, the session-cookie auth arm fails closed; the raw-Hydra Bearer / DPoP-introspection paths are unaffected (they do not use the gateway key).
- Key custody and rotation are operator responsibilities at v1.

## See also

- `docs/archive/auth-server.md` — full design proposal (decisions, threat model, deferred work)
- `docs/superpowers/plans/2026-05-26-auth-server-phase-1-foundation.md` — Phase 1 plan (crate skeleton, hydra wiring, schema)
- `docs/superpowers/plans/2026-05-27-auth-server-phase-2-password-login.md` — Phase 2 plan (password identity, login UI)
- `docs/superpowers/plans/2026-05-27-auth-server-phase-3-oidc-rps.md` — Phase 3 plan (gateway + control plane as RPs)
- `docs/superpowers/plans/2026-05-27-auth-server-phase-7-dpop-bcl.md` — Phase 7 plan (back-channel logout + DPoP-ready surface)
- `docs/superpowers/plans/2026-05-27-auth-server-phase-8-dpop-binding.md` — Phase 8 plan (wrapper-token issuer + `cnf.jkt` enforcement). **Superseded:** the gateway wrapper-token issuer was removed in the BFF session redesign — the browser now holds a signed session cookie (verified locally), not a wrapper token. See `docs/archive/superpowers/specs/2026-05-30-auth-bff-session-redesign.md`. Non-browser DPoP binding survives via the introspection path described above.
