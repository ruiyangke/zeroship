# Auth

Zeroship's authentication is an OpenID Connect 1.0 / OAuth 2.1 deployment with two cooperating components:

- **`crates/auth`** — login UI, identity flows (password today; federation + magic-link in later phases), user store, audit log, hydra admin client.
- **`oryd/hydra`** — the OIDC kernel (sidecar). Issues ID tokens, access tokens, and refresh tokens; owns the OAuth/OIDC protocol surface. We do not own the protocol implementation.

Every authenticated zeroship surface is an OIDC Relying Party (RP) of `auth.zeroship.ai`:

- **The gateway** runs the RP flow on behalf of every hosted creator app at `*.zeroship.ai`.
- **The control plane** runs an RP for the creator dashboard at `console.zeroship.ai`.

This means: one identity per human, one global user pool, one place that holds raw credentials.

## Topology

```
End user
   │
   │  GET https://myapp.zeroship.ai/anything
   ▼
┌──────────────┐    no __Host-zs_app_session cookie → 302 to /oauth2/auth
│   gateway    │ ─────────────────────────────────────────────────────────┐
└──────────────┘                                                          │
       │ also proxies auth.zeroship.ai/{oauth2,.well-known,userinfo}/*    │
       ▼ to hydra; everything else at that host to crates/auth            │
┌──────────────┐  302 to /login   ┌──────────────┐                        │
│    hydra     │ ───────────────► │  crates/auth │  password verify       │
│ public :4444 │ ◄── accept_login─│   /login     │  + insert auth.sessions│
│ admin  :4445 │                  └──────────────┘                        │
└──────────────┘                                                          │
   │                                                                      │
   │ 302 to https://myapp.zeroship.ai/__zs/auth/callback?code=…&state=…   │
   ▼                                                                      │
   gateway exchanges code at /oauth2/token, validates the ID token,      │
   inserts auth.gateway_sessions, sets __Host-zs_app_session cookie,     │
   then proxies to the worker with ZeroShip-User (HMAC-signed) ◄──────────┘
```

The control plane runs an identical RP flow on `console.zeroship.ai`, terminating at `/auth/callback` and minting `__Host-zs_console_session`.

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

| Method | Path | Purpose |
|---|---|---|
| GET, POST | `/login?login_challenge=…` | Render password form / accept_login against hydra |
| GET, POST | `/signup?login_challenge=…` | Create account (carries challenge if entered mid-flow) |
| GET | `/consent?consent_challenge=…` | First-party skip-consent fast path |
| GET | `/healthz`, `/readyz` | Health checks |

The federation routes (`/oauth/google/*`, `/oauth/github/*`), magic-link redemption (`/magic/verify`), email verification (`/verify`), password reset (`/forgot`, `/reset`), profile (`/me`), logout confirmation (`/logout`), and webhook receivers are designed in `docs/proposals/auth-server.md` §6 and ship in Phases 4–5. They are **not registered today.**

### Owned by `crates/gateway` (per hosted-app host)

| Method | Path | Purpose |
|---|---|---|
| GET | `/__zs/auth/callback?code=…&state=…` | Receives the OIDC code, exchanges at hydra's `/oauth2/token`, sets `__Host-zs_app_session`, redirects to the original path |

### Owned by `crates/control` (`console.zeroship.ai`)

| Method | Path | Purpose |
|---|---|---|
| GET | `/auth/callback?code=…&state=…` | OIDC RP callback for the dashboard; sets `__Host-zs_console_session` |

## Session cookies

All cookies are `HttpOnly`, `SameSite=Lax`, `Path=/`. `Secure` is set unless the process is running with `insecure_dev = true` (localhost). The `__Host-` prefix (RFC 6265bis §4.1.3) requires `Path=/`, no `Domain=`, and `Secure` — the dev override drops `Secure` only.

| Cookie | Set by | Host | Max-Age |
|---|---|---|---|
| `__Host-zsidp_session` | `crates/auth` | `auth.zeroship.ai` | 12 h hard / 30 min idle |
| `__Host-zsidp_csrf` | `crates/auth` | `auth.zeroship.ai` | per-form |
| `__Host-zs_app_session` | gateway | each `*.zeroship.ai` app origin | 12 h (43200 s) |
| `__Host-zs_oidc_stash` | gateway | each app origin | 10 min (600 s) — pending the OIDC redirect |
| `__Host-zs_console_session` | control | `console.zeroship.ai` | 12 h |
| `__Host-zs_console_stash` | control | `console.zeroship.ai` | 10 min |
| `ory_hydra_session_*` | hydra | `auth.zeroship.ai` | hydra-configured |

The `*_stash` cookies carry the HMAC-signed PKCE verifier + state + original request path so the callback can complete the dance without server-side scratch storage. They are cleared on a successful callback.

## SDK surface — `@zeroship/auth`

The npm package exposes:

- `auth.getUser()` — the authenticated user, or `null` if anonymous
- `auth.requireUser()` — the user, or throws a 401-shaped Error
- `auth.isLoggedIn()` — convenience boolean
- `auth.signOut(returnTo?)` — returns a 302 `Response` to `/__zs/auth/signout`

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
- `gateway` — fans out per-hosted-app callbacks. The control plane appends one `https://<app>.zeroship.ai/__zs/auth/callback` to `redirect_uris` per deploy, via `PUT /admin/clients/gateway`. Exact-string match (RFC 9700 §4.1) — no wildcards.

Adding or rotating a client is an edit to the TOML; `crates/auth/src/bootstrap` upserts on startup.

## Data model

The `auth.*` PostgreSQL schema (managed by `crates/auth/src/store/migrations.rs`):

| Table | Purpose |
|---|---|
| `auth.users` | Single global user pool |
| `auth.identities` | Federation provider linkages (populated in Phase 4) |
| `auth.sessions` | `crates/auth`'s IdP login session (`__Host-zsidp_session`) |
| `auth.gateway_sessions` | Per-app sessions for hosted creator apps |
| `auth.console_sessions` | Control-plane dashboard sessions |
| `auth.magic_links` | Magic-link tokens (Phase 5) |
| `auth.email_verifications` | Email-verification tokens (Phase 5) |
| `auth.email_suppressions` | Mailer bounce/complaint list (Phase 5) |
| `auth.rate_limits` | Login throttling state |
| `auth.audit_events` | Structured event log |

Hydra's own `hydra_*` tables live in the same database, managed by `hydra migrate sql up` (a deployment-managed init step — not by `crates/auth`).

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

## See also

- `docs/proposals/auth-server.md` — full design proposal (decisions, threat model, deferred work)
- `docs/superpowers/plans/2026-05-26-auth-server-phase-1-foundation.md` — Phase 1 plan (crate skeleton, hydra wiring, schema)
- `docs/superpowers/plans/2026-05-27-auth-server-phase-2-password-login.md` — Phase 2 plan (password identity, login UI)
- `docs/superpowers/plans/2026-05-27-auth-server-phase-3-oidc-rps.md` — Phase 3 plan (gateway + control plane as RPs)
