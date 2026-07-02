# Auth

Zeroship authentication is a native OpenID Connect 1.0 / OAuth 2.1 provider.
`crates/auth` is the sole OP: it serves the login UI, first-party identity
flows, OAuth/OIDC protocol endpoints, token signing, refresh-token rotation,
consent, device authorization, UserInfo, discovery, and JWKS.

Every authenticated hosted app is an OIDC relying party of
`auth.zeroship.ai/oauth2`:

- **Gateway** runs the RP flow for every hosted creator app at `*.zeroship.ai`.
- **Console** is a normal hosted app at `console.zeroship.ai`; it uses the same
  gateway RP flow as every other hosted app.
- **Control** is a resource server and client registry manager. It does not run
  a browser RP of its own.

This gives the platform one global user pool, one issuer, and one process that
holds raw credentials.

## Topology

```text
End user
  |
  | GET https://myapp.zeroship.ai/anything
  v
gateway
  | no __Host-zeroship_app_session cookie
  | -> 302 to auth service /oauth2/authorize
  v
crates/auth
  | /oauth2/authorize -> /login or /consent when interaction is needed
  | /login verifies password, magic link, or federated identity
  | /oauth2/token exchanges the code and mints tokens
  v
gateway callback
  | validates the ID token, creates the app session cookie,
  | then forwards ZeroShip-User to the worker
  v
worker runtime
```

`AUTH_PUBLIC_URL` is the externally visible auth origin, for example
`https://auth.zeroship.ai`. The issuer stamped into tokens and discovery is
`${AUTH_PUBLIC_URL}/oauth2`.

## Endpoints

### Native OP endpoints

All of these are served by `crates/auth` under `/oauth2`.

| Path | Notes |
|---|---|
| `/oauth2/authorize` | Authorization-code endpoint, PKCE S256 only |
| `/oauth2/token` | Authorization-code, refresh-token, and device-code exchange |
| `/oauth2/device/authorization` | RFC 8628 device authorization start |
| `/oauth2/revoke` | RFC 7009 refresh-family revocation |
| `/oauth2/introspect` | RFC 7662 token introspection |
| `/oauth2/userinfo` | OIDC UserInfo |
| `/oauth2/logout` | RP-initiated logout confirmation |
| `/oauth2/.well-known/openid-configuration` | OIDC discovery |
| `/oauth2/.well-known/oauth-authorization-server` | RFC 8414 metadata |
| `/oauth2/.well-known/jwks.json` | EdDSA public signing key |

### Identity UI and webhooks

These are also served by `crates/auth` on the auth host.

| Method | Path | Purpose |
|---|---|---|
| GET, POST | `/login` | Password login and challenge continuation |
| GET, POST | `/signup` | Account creation |
| GET, POST | `/consent` | Consent screen and accept/deny handling |
| GET, POST | `/device` | User-code entry for device authorization |
| GET | `/oauth/google/start`, `/oauth/google/callback` | Google federation, when configured |
| GET | `/oauth/github/start`, `/oauth/github/callback` | GitHub federation, when configured |
| GET, POST | `/link` | Link a federated identity to an existing account |
| POST | `/magic/start` | Issue and email a magic-link token |
| GET | `/magic/await`, `/magic/verify` | Magic-link wait and landing pages |
| POST | `/magic/verify/redeem`, `/magic/complete` | Cross-device and final magic-link redemption |
| GET | `/verify` | Email-verification landing |
| POST | `/verify/redeem` | Consume an email-verification token |
| GET, POST | `/forgot`, `/reset` | Password reset issue and redeem |
| GET | `/me` | Signed-in profile page |
| POST | `/me/unlink/{provider}` | Unlink a federated identity |
| POST | `/me/2fa/enroll`, `/me/2fa/confirm`, `/me/2fa/disable` | TOTP self-service |
| POST | `/webhooks/postmark`, `/webhooks/ses-sns`, `/webhooks/relay-inbound` | Mailer and relay webhooks |
| GET | `/healthz`, `/readyz`, `/static/style.css` | Health checks and static CSS |

### Gateway BFF endpoints

Per hosted-app host, the gateway serves the same-origin browser BFF endpoints
used by `@zeroship/auth`.

| Method | Path | Purpose |
|---|---|---|
| GET | `/__zeroship/auth/authorize` | Builds the OP authorize URL and stores PKCE/state in an app-origin stash cookie |
| GET | `/__zeroship/auth/popup-callback` | Same-origin relay page for popup/iframe flows |
| GET | `/__zeroship/auth/callback?code=...&state=...` | Exchanges the code, validates tokens, creates the app session |
| GET, POST | `/__zeroship/auth/session` | Reads or re-mints the BFF session projection |
| POST | `/__zeroship/auth/signout` | Clears the app session and revokes refresh-family state |
| POST | `/oidc/backchannel-logout` | Back-channel logout receiver |

## Cookies

All auth cookies are `HttpOnly`, `SameSite=Lax`, and `Path=/`. `Secure` is set
outside explicit insecure dev mode. The `__Host-` prefix is used where RFC
6265bis allows it.

| Cookie | Set by | Host | Max-Age |
|---|---|---|---|
| `__Host-zsidp_session` | auth | `auth.zeroship.ai` | 12 h hard / 30 min idle |
| `__Host-zsidp_csrf` | auth | `auth.zeroship.ai` | per form |
| `__Host-zsidp_google_stash` | auth | `auth.zeroship.ai` | 10 min |
| `__Host-zsidp_github_stash` | auth | `auth.zeroship.ai` | 10 min |
| `__Host-zsidp_magic_csrf` | auth | `auth.zeroship.ai` | 15 min |
| `__Host-zeroship_app_session` | gateway | each hosted app origin | 12 h |
| `__Host-zeroship_app_anchor` | gateway | each hosted app origin | 30 d |
| `__Host-zs_oidc_stash` | gateway | each hosted app origin | 10 min |

The app-origin stash cookies carry HMAC-signed PKCE verifier, state, nonce, and
return path. They are cleared on successful callback. The federation stash names
are provider-specific so separate tabs do not overwrite each other.

## SDK Surface

`@zeroship/auth` exposes the server helper surface:

- `auth.getUser()` returns the authenticated user or `null`.
- `auth.requireUser()` returns the user or throws a 401-carrying error.
- `auth.isLoggedIn()` is a convenience boolean.
- `auth.signOut(returnTo?)` returns a redirect `Response` to sign out.

All server helpers read `env.auth.user`, which the runtime populates from the
gateway's HMAC-signed `ZeroShip-User` header. The browser client uses only the
BFF endpoints above; it never receives an access token or refresh token.

```javascript
import { auth } from "@zeroship/auth";

export default {
  async fetch(req, env) {
    const user = auth.requireUser();
    return Response.json({ hello: user.name ?? user.id });
  }
};
```

## Token Formats

All token operations are native to `crates/auth`.

- **ID token:** EdDSA-signed JWT, 15 min TTL. Standard OIDC claims plus
  scope-gated identity claims such as `email`, `email_verified`, `name`, and
  `picture`.
- **Access token:** RFC 9068-style EdDSA JWT, 15 min TTL, `typ = at+jwt`.
- **Refresh token:** opaque `zrt_...` token. Only HMAC-SHA256 verifiers are
  stored in `zeroship.oauth_refresh_tokens`. Families are 7 d idle / 30 d hard
  and rotate on every refresh with a bounded idempotency window.
- **Authorization code:** single-use, PKCE-bound, 60 s TTL.
- **Device code:** 10 min TTL, polling interval enforced by the OP.

## OAuth Clients

OAuth clients live in `zeroship.oauth_clients`; per-app clients additionally
have `zeroship.app_oauth_clients` rows. Control creates and updates app clients
as apps are created, deployed, and custom-domain redirect URIs change.

Client IDs for hosted apps are deterministic `oac_...` identifiers. The OP
loads the client row during authorize/token flows, validates exact redirect URI
matches, enforces PKCE S256, and uses the app client's sector identifier to
derive pairwise `pws_...` subjects.

Brokered clients authenticate at token exchange with a per-client broker secret
derived from `AUTH_BROKER_SECRET_FILE`; public clients are PKCE-only.

## Data Model

The auth tables live in the `zeroship` PostgreSQL schema and are owned by
`zeroship-migrate`:

| Table | Purpose |
|---|---|
| `zeroship.users` | Global user pool |
| `zeroship.federated_identities` | Google/GitHub identity links |
| `zeroship.idp_sessions` | Auth-origin login sessions |
| `zeroship.gateway_sessions` | Per-app session audit/revocation state |
| `zeroship.oauth_clients` | Native OP client registry |
| `zeroship.app_oauth_clients` | Per-app client extension rows |
| `zeroship.oauth_grants` | User consent grants |
| `zeroship.oauth_authorization_codes` | Pending authorization codes |
| `zeroship.oauth_refresh_tokens` | Refresh-token family state |
| `zeroship.device_grants` | Device authorization grants |
| `zeroship.signing_keys` | Published public JWK metadata |
| `zeroship.magic_links`, `zeroship.magic_completions` | Magic-link and reset flows |
| `zeroship.email_verifications`, `zeroship.email_suppressions` | Email verification and suppression |
| `zeroship.rate_limits` | Login throttling state |
| `zeroship.audit_events` | Structured audit log |
| `zeroship.dpop_jti`, `zeroship.token_revocations` | DPoP replay and revocation state |
| `zeroship.jwk_key_state`, `zeroship.cron_state` | Key rotation and cron bookkeeping |

## Operator Notes

Local compose runs the native OP as the `auth` service. In production, configure
at minimum:

- `AUTH_DB_URL`
- `AUTH_PUBLIC_URL`
- `AUTH_SIGNING_KEY_FILE`
- `AUTH_PAIRWISE_SALT_FILE`
- `AUTH_BROKER_SECRET_FILE`
- `REFRESH_HASH_KEY_FILE`
- `REFRESH_IDEM_KEY_FILE`
- `AUTH_STASH_SIGNING_KEY`
- `AUTH_TOTP_ENC_KEY`

Run platform migrations before booting services:

```bash
zeroship-migrate migrate --dir ./db/migrations --database-url "$DATABASE_URL" --profile platform --yes
```

Then start `zeroship-auth` with the variables above. On boot it publishes the
active public JWK metadata from `AUTH_SIGNING_KEY_FILE` into Postgres and serves
discovery from `${AUTH_PUBLIC_URL}/oauth2`.

## DPoP and Bearer Access

Browser apps use the BFF session cookie. Non-browser clients may present native
OP access tokens directly.

When the gateway receives `Authorization: DPoP <token>`, it verifies the DPoP
proof (`htu`, `htm`, `iat`, `ath`, signature, and replay `jti`), then
introspects the token at `/oauth2/introspect`, binds the resulting `client_id`
to the route's OAuth client, projects the global user to the app's pairwise
subject, enforces revocation markers, and forwards `ZeroShip-User`.

Plain `Authorization: Bearer <token>` is accepted for non-browser clients on the
same native-token arm, with local JWKS verification plus the same per-app
`client_id` and revocation checks.

| Token | Binding | Verifier | Notes |
|---|---|---|---|
| Native access token | per-app `client_id` claim | Local JWKS verify + revocation marker | Token compromise is enough to impersonate until expiry/revocation |
| Native access token + DPoP proof | DPoP proof + per-app `client_id` | DPoP verifier + introspection + revocation marker | Adds replay protection; access tokens are not `cnf.jkt`-bound |

## See Also

- [Auth deployment runbook](../runbooks/auth-deploy.md)
- [Auth dev tier](auth-dev-tier.md)
- [Control plane architecture](../architecture/control-plane.md)
- [Gateway routing architecture](../architecture/gateway-routing.md)
- [Historical auth-server design](../archive/auth-server.md)
