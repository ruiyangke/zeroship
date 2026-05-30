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
| POST | `/__zs/auth/dpop-exchange` | Exchanges a hydra access token + DPoP proof for a `cnf.jkt`-bound wrapper JWT. See [DPoP-bound wrapper tokens](#dpop-bound-wrapper-tokens). |
| POST | `/oidc/backchannel-logout` | OIDC BCL 1.0 receiver — verifies a `logout_token` and revokes the subject's gateway sessions. |

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

The `auth.*` PostgreSQL schema (owned by Liquibase — `db/changelog/`; see [Database migrations](../runbooks/db-migrations.md)):

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

## DPoP-bound wrapper tokens

Phase 8 adds RFC 9449 token binding (`cnf.jkt`) without forking hydra. The gateway becomes a JWT issuer: DPoP-aware clients exchange a hydra access token for a gateway-signed *wrapper token* whose `cnf.jkt` claim binds it to a specific public key. Subsequent requests carrying `Authorization: DPoP <wrapper>` are verified locally — no per-request introspection — and the key in the inbound DPoP proof must match `cnf.jkt`.

### `POST /__zs/auth/dpop-exchange`

Mints a wrapper token from a hydra access token + a DPoP proof.

```
POST /__zs/auth/dpop-exchange
Host: myapp.zeroship.ai
Authorization: Bearer <hydra_access_token>
DPoP: <RFC 9449 proof JWT, signed by the client's DPoP key>
```

The proof's `htu` must be `https://<host>/__zs/auth/dpop-exchange`, `htm` must be `POST`, and `ath` must be `base64url(SHA-256(hydra_access_token))`. The handler also enforces `iat` freshness and a per-`jti` replay cache (in-process, 120 s TTL).

On success:

```
200 OK
Cache-Control: no-store
Content-Type: application/json

{
  "wrapper_token": "<JWT>",
  "expires_in": 3600,
  "token_type": "DPoP"
}
```

Error responses (all include `Cache-Control: no-store`):

| Status | `error` code | Meaning |
|---|---|---|
| 400 | `missing_bearer` | `Authorization: Bearer <hydra_token>` absent or malformed |
| 400 | `missing_dpop` | `DPoP` header absent |
| 401 | `invalid_dpop_proof` | Proof signature / `htu` / `htm` / `iat` / `ath` failed verification |
| 401 | `dpop_jti_replay` | This `jti` was already accepted within the 120 s window |
| 401 | `hydra_token_inactive` | Hydra introspection reports `active: false` |
| 401 | `introspection_failed` | Introspection call failed (network / hydra error) |
| 500 | `issuer_failed` | JWT signing failed (should not occur with a valid signing key) |
| 503 | `dpop_binding_not_configured` | Gateway booted without `--signing-key-file`; no issuer is available |

### Wrapper token shape

Header:

```json
{ "alg": "EdDSA", "typ": "at+jwt", "kid": "<RFC 7638 thumbprint of gateway signing key>" }
```

`typ: at+jwt` follows RFC 9068. The signing algorithm is Ed25519 (`EdDSA`).

Claims:

| Claim | Source | Notes |
|---|---|---|
| `iss` | Gateway public URL | Verified on the inbound side |
| `aud` | Request `Host` header | Per-app binding — wrapper minted for `myapp.zeroship.ai` won't be accepted at `other.zeroship.ai` |
| `sub` | Hydra introspection | `usr_…` |
| `exp` | `iat + 3600` | 1 h TTL, matches hydra access-token lifetime |
| `iat` | Now | Unix seconds |
| `jti` | Random UUID v4 | |
| `cnf.jkt` | DPoP proof | RFC 7638 thumbprint of the client's DPoP public key — the binding |
| `scope` | Hydra introspection | |
| `client_id` | Hydra introspection | |
| `email`, `name`, `email_verified` | Hydra introspection | Optional, only present if hydra returned them |
| `wraps` | `base64url(SHA-256(hydra_access_token))` | Forensic link to the underlying hydra token. Not checked at verify time. |

### Dispatch enforcement

When the gateway sees `Authorization: DPoP <token>` on an inbound request it tries to decode the token as a wrapper first. If the signature, `iss`, and `exp` check out:

1. Verify the inbound `DPoP` proof normally (signature, `htu`, `htm`, `iat`, `ath = SHA-256(wrapper)`, `jti` replay).
2. Enforce **`wrapper.cnf.jkt == thumbprint(proof.jwk)`** — mismatch ⇒ 401.
3. Use the wrapper claims (`sub`/`email`/`name`/…) to build the `ZeroShip-User` header — no hydra round-trip.

If the token is *not* a wrapper (no valid gateway signature), the gateway falls back to the Phase 7 path: hydra introspection + DPoP proof verification, but **without** `cnf.jkt` enforcement. Raw hydra tokens are accepted because hydra doesn't issue `cnf.jkt`-bound access tokens; the fallback is what lets non-binding clients keep working.

### Security model

| Token | Binding | Verifier | Notes |
|---|---|---|---|
| Hydra access token (`Authorization: Bearer`) | None | Hydra introspection | Compromise of the token alone is enough to impersonate the user |
| Hydra access token + DPoP proof (`Authorization: DPoP <hydra>`) | DPoP proof verified, **no `cnf.jkt`** | Hydra introspection + `core::dpop` | Bearer-style — proof verification adds replay protection but the token is not pinned to a key |
| Wrapper token (`Authorization: DPoP <wrapper>`) | **`cnf.jkt` enforced** at dispatch | Local Ed25519 verify + `core::dpop` + `cnf.jkt` match | Stolen wrapper without the DPoP private key is unusable |

**DPoP-aware clients SHOULD exchange immediately:** call `/__zs/auth/dpop-exchange` right after the OIDC code exchange and use the wrapper for every subsequent request. Treat the hydra access token as a single-use credential that exists only to mint the wrapper.

The raw-hydra-DPoP fallback is a known attenuation of the DPoP guarantee. A future `--strict-dpop` flag will disable the fallback (returning 401 instead of falling through to introspection); it is not implemented in Phase 8.

### Operator notes

- The gateway needs an Ed25519 signing key. Generate with `openssl genpkey -algorithm ed25519 -out gw.key` and pass via `--signing-key-file` (or `GATEWAY_SIGNING_KEY_FILE`). PKCS#8 PEM or DER both work.
- Without a signing key, the gateway boots fine but `/__zs/auth/dpop-exchange` returns 503 and the wrapper-verifier path is disabled (the raw-hydra-DPoP fallback still works).
- Key custody and rotation are operator responsibilities at v1; the planned Phase 9 KMS integration is not implemented here.
- Wrapper tokens have a 1 h hard TTL. There is no refresh — when a wrapper expires, the client re-exchanges. (Cascade revocation from hydra back-channel logout to wrapper invalidation is deferred to a future phase.)

## See also

- `docs/proposals/auth-server.md` — full design proposal (decisions, threat model, deferred work)
- `docs/superpowers/plans/2026-05-26-auth-server-phase-1-foundation.md` — Phase 1 plan (crate skeleton, hydra wiring, schema)
- `docs/superpowers/plans/2026-05-27-auth-server-phase-2-password-login.md` — Phase 2 plan (password identity, login UI)
- `docs/superpowers/plans/2026-05-27-auth-server-phase-3-oidc-rps.md` — Phase 3 plan (gateway + control plane as RPs)
- `docs/superpowers/plans/2026-05-27-auth-server-phase-7-dpop-bcl.md` — Phase 7 plan (back-channel logout + DPoP-ready surface)
- `docs/superpowers/plans/2026-05-27-auth-server-phase-8-dpop-binding.md` — Phase 8 plan (wrapper-token issuer + `cnf.jkt` enforcement)
