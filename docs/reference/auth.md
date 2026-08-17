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

`ZEROSHIP_AUTH_PUBLIC_URL` is the externally visible auth origin, for example
`https://auth.zeroship.ai`. The issuer stamped into tokens and discovery is
`${ZEROSHIP_AUTH_PUBLIC_URL}/oauth2`.

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

All auth cookies are `HttpOnly` and `Path=/`. No JavaScript ever needs to read
one: where a value has to reach a form, the server renders it into both the
cookie and the hidden field in the same response. `Secure` is always set, in
every environment: there is no mode that drops it. Local runs work because
browsers treat `localhost` and `*.localhost` as trustworthy origins, so a
`Secure` cookie is accepted over plain `http` there. The `__Host-` prefix is
used where RFC 6265bis allows it.

`SameSite` is `Lax` for the cookies that must survive a top-level redirect back
from an external identity provider or an emailed link, and `Strict` for those
that are only ever presented by a form on an auth page the user is already
looking at.

| Cookie | Set by | Host | SameSite | Max-Age |
|---|---|---|---|---|
| `__Host-zsidp_session` | auth | `auth.zeroship.ai` | Lax | 12 h hard / 30 min idle |
| `__Host-zsidp_csrf` | auth | `auth.zeroship.ai` | Strict | per form |
| `__Host-zsidp_google_stash` | auth | `auth.zeroship.ai` | Lax | 10 min |
| `__Host-zsidp_github_stash` | auth | `auth.zeroship.ai` | Lax | 10 min |
| `__Host-zsidp_magic_csrf` | auth | `auth.zeroship.ai` | Lax | 15 min |
| `__Host-zeroship_app_session` | gateway | each hosted app origin | Lax | 15 min |
| `__Host-zeroship_app_anchor` | gateway | each hosted app origin | Strict | 30 d |
| `__Host-zs_oidc_stash` | gateway | each hosted app origin | Lax | 10 min |

`__Host-zeroship_app_session` is a gateway-signed `zeroship-sess+jwt` identity
assertion, not an opaque session id, so its lifetime is the token's own 15-minute
`exp`. The durable credential is the 30-day server-held
`__Host-zeroship_app_anchor`, which silently re-signs a fresh session cookie via
`GET /__zeroship/auth/session` once the short one lapses.

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
  and rotate on every refresh. A lost response can be retried once inside a
  bounded window; the record is single-use, so a second presentation of an
  already-rotated token revokes the whole family.
- **Authorization code:** single-use, PKCE-bound, 60 s TTL.
- **Device code:** 10 min TTL, polling interval enforced by the OP.

### CLI platform tokens

`zeroship login` runs the OP's own device grant. It first reads control's RFC
9728 metadata at `GET {control}/.well-known/oauth-protected-resource` to learn
which authorization server control accepts tokens from, then drives
`POST {issuer}/device/authorization` and `POST {issuer}/token` against that
issuer. It asks for `offline_access`, so the OP returns a refresh token
alongside a 15-minute access token, and `zeroship deploy` rotates that refresh
token when the access token expires. The rotated credential is written to disk
before it is used: the presented token is already spent, and re-presenting it
is a reuse detection that revokes the family.

Control's parallel device flow still exists and exchanges an approved grant for
a platform access token through `POST /internal/platform-token`. That mint is
deliberately narrower than the other internal control APIs:

- Only auth and control receive `auth.platform_mint_key`. The worker still
  needs `control_key` for its normal control-plane calls, but that key is not
  accepted by the mint.
- Auth rejects an unknown `principal_id`, loads that principal's grants from
  Postgres itself, and issues only the intersection of stored grants and the
  requested scopes. The caller cannot establish entitlement by naming a
  principal or scope in the request. The auth database role has SELECT-only
  access to `zeroship.principal_grants`; it cannot mutate grants.
- Auth chooses the server-configured platform audience and the fixed
  `zeroship-cli` client ID. Neither value is caller-controlled.
- The requested lifetime must be positive and no more than 12 hours.

Auth reconciles a first-party `zeroship-cli` OAuth client registration at
startup. It may request `apps:deploy`, `apps:read`, `apps:write`,
`secrets:read` and `offline_access`; a token request cannot expand that set,
and `offline_access` asks for the refresh family without conferring any
resource authority of its own. An approved OP device grant for this client
produces a 15-minute access token with the configured control audience and a
public `sub` equal to the platform principal UUID. Generic app clients continue
to receive app-sector pairwise `pws_...` subjects.

The refresh rotation mints through the same code path as the device
redemption, so a rotated CLI token keeps that principal shape rather than
silently becoming a pairwise token control would refuse. The refresh row
stores the principal subject too, which is what makes reuse detection write
the `zeroship.token_revocations` marker described below.

`PLATFORM_TOKEN_MAX_TTL_SECS` (12 hours) still bounds control's parallel mint,
which issues no refresh token and therefore has nothing shorter to fall back
on.

Control's bearer verification path honors a
`zeroship.token_revocations` marker for `zeroship-cli`. Account deletion writes
that marker, so credentials issued before a deletion request stay revoked even
if the request is cancelled. Refresh reuse detection now writes it too: a
rotated-away CLI refresh token, presented once more after its one lost-response
retry, revokes the family AND stamps the marker, which recalls the access token
the attacker may already be holding.

Everything else still relies on expiry. The current `zeroship logout` deletes
only the local credential and calls no RFC 7009 endpoint, so a token already
copied off the machine stays valid for the rest of its lifetime: at most 15
minutes for a login-issued token, at most 12 hours for one minted through
control's parallel flow.

## OAuth Clients

OAuth clients live in `zeroship.oauth_clients`; per-app clients additionally
have `zeroship.app_oauth_clients` rows. Control creates and updates app clients
as apps are created, deployed, and custom-domain redirect URIs change.

Client IDs for hosted apps are deterministic `oac_...` identifiers. The OP
loads the client row during authorize/token flows, validates exact redirect URI
matches, enforces PKCE S256, and uses the app client's sector identifier to
derive pairwise `pws_...` subjects. The reconciled first-party `zeroship-cli`
row has no app-client extension and uses the public platform-principal subject
policy described above.

Brokered clients authenticate at token exchange with a per-client broker secret
derived from `AUTH_BROKER_SECRET_FILE`. Any other client registered with a
`client_secret_basic` or `client_secret_post` method presents that secret on
every grant it uses, `authorization_code` included; only clients registered
with `token_endpoint_auth_method = none` are PKCE-only. A client that fails
authentication gets `401` with `invalid_client` and a `WWW-Authenticate`
challenge.

## Data Model

The auth tables live in the `zeroship` PostgreSQL schema and are created by the
platform corpus applied through `zeroship-platform-migrate`:

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
| `zeroship.principal_grants` | Platform grants; auth has SELECT-only access for CLI mint capping |
| `zeroship.signing_keys` | Public JWK lifecycle and maximum issued-expiry watermark |
| `zeroship.magic_links`, `zeroship.magic_completions` | Magic-link and reset flows |
| `zeroship.email_verifications`, `zeroship.email_suppressions` | Email verification and suppression |
| `zeroship.rate_limits` | Login throttling state |
| `zeroship.audit_events` | Structured audit log |
| `zeroship.dpop_jti`, `zeroship.token_revocations` | DPoP replay and revocation state |
| `zeroship.cron_state` | Durable cron bookkeeping |

### Signing-key retention

JWKS publishes keys in `active`, `next`, or `retiring` state. Every production
token issuance atomically advances that key row's `max_issued_expires_at`
before returning the signed token. A concurrent retirement that wins the row
race changes the status first, so issuance affects zero rows and discards the
token instead of returning a token whose key is no longer published.

The hourly signing-key retention cron changes only elapsed `retiring` keys to
terminal `retired`; it never deletes the audit row and never selects `active`
or `next`. The normal cutoff is the greatest exact issued expiry plus the full
JWKS cache window and clock-skew allowance: 300 seconds `max-age` + 300 seconds
`stale-while-revalidate` + 120 seconds skew. A row without an issuance
watermark uses the conservative fallback of `retiring_at` + the 43,200-second
maximum token lifetime + those 720 seconds. The full fallback horizon is
43,920 seconds (12 hours 12 minutes).

## Operator Notes

Local compose runs the native OP as the `auth` service. In production, configure
at minimum:

- `AUTH_DB_URL`
- `ZEROSHIP_AUTH_PUBLIC_URL`
- `AUTH_SIGNING_KEY_FILE`
- `AUTH_PAIRWISE_SALT_FILE`
- `AUTH_BROKER_SECRET_FILE`
- `REFRESH_HASH_KEY_FILE`
- `REFRESH_IDEM_KEY_FILE`
- `AUTH_STASH_SIGNING_KEY`
- `AUTH_TOTP_ENC_KEY`
- `ZEROSHIP_AUTH_PLATFORM_MINT_KEY`

Run platform migrations before booting services:

```bash
cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate
./target/release/zeroship-platform-migrate \
  --database-url "$DATABASE_URL" \
  --migrations-dir ./db/migrations-ts \
  --project-schema zeroship \
  --project-id zeroship
```

Then start `zeroship-auth` with the variables above. On boot it publishes the
active public JWK metadata from `AUTH_SIGNING_KEY_FILE` into Postgres and serves
discovery from `${ZEROSHIP_AUTH_PUBLIC_URL}/oauth2`.

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
