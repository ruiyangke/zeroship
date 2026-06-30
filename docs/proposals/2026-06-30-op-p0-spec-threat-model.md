# P0 OP protocol spec, threat model, and key rotation design

**Status:** proposal (P0 pre-implementation gate).
**Date:** 2026-06-30.
**Scope:** design/spec only. No protocol code in this phase.
**Build constraints:** zero-tokio; use compio/ntex/cyper/compio-postgres/jsonwebtoken.
**Parent design:** `docs/proposals/2026-06-30-self-contained-auth-replace-hydra.md`.
**Standards map:** `docs/proposals/oauth2-standards-conformance.md`.

This document makes the self-contained OAuth2/OIDC Authorization Server buildable by pinning the endpoint contract, mint formats, storage binding sets, CLI refresh rotation, JOSE rotation, threat model, and dual-issuer migration mechanics.

Normative baseline: OAuth 2.1, RFC 9700 / BCP 240, RFC 6749, RFC 6750, RFC 7636, RFC 8628, RFC 7009, RFC 8414, RFC 9068, RFC 9207, OIDC Core 1.0, OIDC Discovery 1.0, and OIDC Back-Channel Logout 1.0.

Closed-world invariant: all clients are platform-provisioned; no Dynamic Client Registration; no third-party client registration; no implicit, hybrid, or ROPC grants. The subset cuts optional general-purpose-AS machinery, not the security-critical core.

Browser BFF cookie note: the gateway-owned BFF session cookie is out of scope for this OP protocol. It remains gateway-signed (`typ: zeroship-sess+jwt`) and is not an OAuth refresh token. The OP issues OAuth/OIDC artifacts; the gateway cookie lifecycle stays in the gateway.

## 0. Conformance item keys

This document maps endpoint behavior to the 15-item checklist from the parent design's §5A:

| ID | Checklist item |
| --- | --- |
| C1 | PKCE S256 required at `/authorize`; `/token` recomputes and compares. RFC 7636 §4.2, §4.6. |
| C2 | Exact redirect-URI match. OAuth 2.1 §2.3.1; RFC 9700 §2.1. |
| C3 | CSRF protection through `state` and browser-session binding. RFC 6749 §10.12; RFC 9700 §2.1. |
| C4 | `nonce` carried into `id_token` and validated by clients. OIDC Core §3.1.2.1, §3.1.3.7. |
| C5 | `iss` authorization-response parameter. RFC 9207 §2, §3. |
| C6 | JWT access token `typ: at+jwt`, audience restricted, required RFC 9068 claims. RFC 9068 §2.1, §2.2, §4. |
| C7 | Refresh rotation plus reuse detection and family revocation. OAuth 2.1 §6.1; RFC 9700 §4.14. |
| C8 | CLI native-app posture: system browser, loopback redirect, PKCE. RFC 8252 §4, §6, §7.3, §8.5, §8.12. |
| C9 | Device-grant polling discipline. RFC 8628 §3.4, §3.5, §5.4, §6.1. |
| C10 | Reject `alg:none` and unexpected algorithms. RFC 7515 §10.7; RFC 9068 §4. |
| C11 | TLS everywhere; no tokens in URLs. RFC 9700 §2.6; RFC 6750 §2.1, §2.3. |
| C12 | Issuer consistency across discovery, token `iss`, and authorization response `iss`. RFC 8414 §3.3; RFC 9207 §2. |
| C13 | Post-authentication redirect uses 303 See Other, not 307. RFC 9700 §4.12. |
| C14 | Authorization code single-use, short TTL, bound to client + redirect + PKCE. RFC 6749 §4.1.2, §10.5. |
| C15 | Back-channel logout plus local session/token revocation. OIDC BCL §2.4-§2.6; RFC 7009. |

## 1. Endpoint specs

### 1.1 `GET /authorize` and `POST /authorize`

Governing specs: RFC 6749 §3.1, §3.1.2, §4.1.1, §4.1.2, §4.1.2.1, §10.5, §10.12; RFC 7636 §4.3, §4.4; RFC 9207 §2; RFC 9700 §2.1, §4.4, §4.12; OIDC Core §3.1.2.1; RFC 8252 §7.3 for native loopback redirects.

`GET /authorize` accepts query parameters. `POST /authorize` accepts the same parameters as `application/x-www-form-urlencoded` and is used only when the browser posts an authorization request or the OP posts login/consent continuation state. Both paths run the same OAuth validation before issuing an authorization code.

Accepted parameters:

| Parameter | Required | Rule |
| --- | --- | --- |
| `response_type` | yes | Must be exactly `code`. Reject `token`, hybrid values, and empty values. C1 and closed-world grant posture. |
| `client_id` | yes | Must resolve to a platform-provisioned `zeroship.oauth_clients` row. |
| `redirect_uri` | yes | Must exact-match one registered URI for the client. Native loopback clients may vary only by loopback port when the registered URI is loopback. C2, C8. |
| `scope` | yes | Whitespace-separated. Must be subset of the client's allowlist. Unknown scopes reject as `invalid_scope`. `openid` requests require `nonce` in the zeroship profile. |
| `state` | strongly required | Required for browser-facing clients; echoed verbatim in the redirect. Bound to OP login/consent transaction state. C3. |
| `nonce` | required with `openid` | Stored on the authorization code and copied into the `id_token`. C4. |
| `code_challenge` | yes | Required for every client. |
| `code_challenge_method` | yes | Must be exactly `S256`; `plain` is rejected. C1. |
| `prompt` | optional | Closed-world values: `login`, `consent`, `none`. `none` may return `login_required` or `consent_required`; it must not silently create a session. |
| `login_hint`, `idp_hint` | optional | UI hints only; never authorization decisions. |

Ordered validation and processing:

1. Require TLS at the public edge and reject plaintext except explicit insecure-dev loopback. Governs C11. RFC 9700 §2.6; RFC 6749 §1.6.
2. Parse parameters once with a structured form/query parser; reject duplicate security-sensitive parameters (`client_id`, `redirect_uri`, `response_type`, `state`, `nonce`, `code_challenge`, `code_challenge_method`) as `invalid_request`. RFC 6749 §4.1.2.1.
3. Validate `response_type == "code"`. Reject implicit/hybrid with `unsupported_response_type`. RFC 6749 §4.1.1; OAuth 2.1 removes implicit. Maps C1 closed-world grant posture.
4. Look up `client_id` in `zeroship.oauth_clients`, joined to `zeroship.app_oauth_clients` when it is an app client. Reject unknown clients as `unauthorized_client`; do not redirect unless the redirect URI was already validated. RFC 6749 §3.1.2.4. Maps IDOR tests.
5. Exact-match `redirect_uri` against `oauth_clients.redirect_uris`. No wildcard, suffix, host, scheme, or path normalization match. Native loopback exception: host must be `127.0.0.1`, `[::1]`, or `localhost` only if explicitly registered; scheme/path must match; port may vary. RFC 6749 §3.1.2.3; OAuth 2.1 §2.3.1; RFC 8252 §7.3. Maps C2, C8.
6. Validate PKCE: `code_challenge` present, `code_challenge_method == "S256"`, challenge is base64url no padding, and the eventual verifier shape is RFC 7636 compatible. Reject missing/`plain` as `invalid_request`. RFC 7636 §4.2-§4.4; OAuth 2.1 §4.1.1. Maps C1.
7. Validate `scope`: sorted-deduped requested scopes must be subset of `oauth_clients.scopes`; app-declared scopes are loaded from `app_scope_defs`; unknown scopes return `invalid_scope`. RFC 6749 §3.3. Existing consent semantics from `classify_and_authorize` remain: identity/app scopes are self-grantable; delegated platform scopes require the authz gate; reserved unknown prefixes are not grantable. Maps C3/C15 consent integrity.
8. If `scope` contains `openid`, require non-empty `nonce`. Store the nonce verbatim for the code and later `id_token`. OIDC Core §3.1.2.1. Maps C4.
9. Bind `state` to the authorization transaction. Browser UI state is stored server-side or in an authenticated, HttpOnly, SameSite cookie; the redirect echoes the client `state` exactly. RFC 6749 §10.12; RFC 9700 §2.1. Maps C3.
10. Authenticate the user through the existing `zeroship.users` credential/session layer. Login completion no longer calls Hydra `accept_login`; it resumes the platform authorization transaction.
11. Consent check: if an existing `oauth_grants`/consent row covers all requested scopes and the client is allowed for silent approval, touch `last_used_at`; otherwise render consent. Consent accept re-runs scope classification, writes the union of prior and requested scopes under an advisory lock, and mints/reuses the relay alias for `email` scope through `app_user_identities`. RFC 6749 §3.3; OIDC Core §5.4 claim/scope consent. Maps C3/C15 privacy boundary.
12. Compute the pairwise subject for app clients with `derive_pairwise(pairwise_salt, global_user_id, sector_identifier)`. Upsert `app_user_identities(app_client_id, global_user_id, pairwise_sub)` before issuing the code.
13. Create the authorization code transactionally: generate at least 256 bits of entropy, store only `code_hash`, bind the full set in §4.2, set `expires_at <= now + 60s`, `consumed_at = NULL`. RFC 6749 §4.1.2, §10.5. Maps C14.
14. Redirect with HTTP 303 See Other to the exact registered `redirect_uri`, never 307. Include `code`, `state` if supplied, and `iss=<issuer>` in the query. RFC 6749 §4.1.2; RFC 9207 §2; RFC 9700 §4.12. Maps C5, C13.

Success response:

```text
303 See Other
Location: {redirect_uri}?code={opaque_code}&state={state}&iss={platform_issuer}
Cache-Control: no-store
```

If `state` was absent for a non-browser client, omit it. The issuer string must exactly equal discovery `issuer` and token `iss` (C12).

Authorization endpoint errors:

| Condition | Response |
| --- | --- |
| Invalid/missing `redirect_uri`, unknown client, malformed request before redirect is safe | 400 HTML or JSON error page; no redirect. RFC 6749 §3.1.2.4. |
| Unsupported `response_type` after redirect is safe | 303 to redirect URI with `error=unsupported_response_type`, `state`, `iss`. RFC 6749 §4.1.2.1. |
| Missing/invalid PKCE, duplicate params, missing required params | `invalid_request`. RFC 6749 §4.1.2.1; RFC 7636 §4.4. |
| Unauthorized client for this grant/redirect/scope | `unauthorized_client`. RFC 6749 §4.1.2.1. |
| User denied consent or login eligibility denied | `access_denied`. RFC 6749 §4.1.2.1. |
| Unknown or ungrantable requested scope | `invalid_scope`. RFC 6749 §4.1.2.1, §3.3. |
| `prompt=none` cannot complete silently | `login_required` or `consent_required` OIDC error. OIDC Core §3.1.2.6. |
| Internal transient failure | `server_error` or `temporarily_unavailable`. RFC 6749 §4.1.2.1. |

### 1.2 `POST /token` - authorization-code grant

Governing specs: RFC 6749 §2.3, §4.1.3, §5.1, §5.2, §10.5; RFC 7636 §4.5, §4.6; RFC 9068 §2; OIDC Core §2, §3.1.3.3, §3.1.3.6; RFC 7515 §10.7.

Accepted parameters: `grant_type=authorization_code`, `code`, `redirect_uri`, `client_id`, `code_verifier`; confidential clients also authenticate with `client_secret_basic` or `client_secret_post` only if provisioned that way. Public clients never send a static secret. No `scope` or `nonce` parameter is accepted for this grant; the code snapshot is authoritative.

Ordered validation and processing:

1. Require `Content-Type: application/x-www-form-urlencoded`; parse structured body; reject duplicates of security-sensitive fields as `invalid_request`. RFC 6749 §4.1.3.
2. Authenticate confidential clients when their registry row requires it. Reject failed auth with HTTP 401 and `WWW-Authenticate: Basic realm="zeroship"` where Basic was attempted; body `invalid_client`. Public clients are authenticated by PKCE only. RFC 6749 §2.3; RFC 8252 §8.5.
3. Look up `client_id`; ensure it is allowed to use authorization-code grant. Unknown/disabled -> `invalid_client` or `unauthorized_client` per RFC 6749 §5.2.
4. Compute `code_hash = HMAC-SHA256(auth_code_hash_key, code)` and open a transaction. Select the `auth_codes` row `FOR UPDATE` or atomically update `consumed_at` with `WHERE consumed_at IS NULL AND expires_at > NOW()`. RFC 6749 §10.5. Maps C14.
5. If no row, expired row, or already-consumed row: return `invalid_grant`. For an already-consumed code, audit `auth_code_reuse` and revoke any tokens minted from that code if a `token_set_id` exists. RFC 6749 §5.2, §10.5.
6. Re-verify the stored binding set exactly: stored `client_id == request.client_id`; stored `redirect_uri == request.redirect_uri` by byte string; stored `code_challenge_method == "S256"`; stored `scope` remains covered by the consent row; stored subject/pairwise mapping still matches `derive_pairwise` for the stored sector. RFC 6749 §4.1.3, §10.5. Maps C1, C2, C14.
7. Validate `code_verifier`: 43-128 chars from RFC 7636 §4.1 unreserved charset; recompute `BASE64URL(SHA256(ASCII(code_verifier)))` and constant-time compare with stored `code_challenge`. RFC 7636 §4.6. Maps C1.
8. Mark the code consumed exactly once within the same transaction. Persist `consumed_at`, `consumed_by_client_id`, and `token_set_id` before returning tokens. Single-use must hold under concurrent duplicate token requests. Maps C14.
9. Mint an RFC 9068 access token. If the code scope includes `openid`, mint an OIDC `id_token` with the stored `nonce` and `at_hash` over the access token. RFC 9068 §2; OIDC Core §2, §3.1.3.6. Maps C4, C6, C10.
10. Issue a refresh token only when all are true: the client face is CLI/programmatic, the requested/granted scope includes `offline_access`, and the client registry allows refresh. Browser BFF flows do not receive OAuth refresh. Maps C7.

Success response (RFC 6749 §5.1):

```json
{
  "access_token": "<jwt>",
  "token_type": "Bearer",
  "expires_in": 900,
  "refresh_token": "<opaque-cli-refresh-token-if-issued>",
  "scope": "openid profile email offline_access",
  "id_token": "<jwt-if-openid>"
}
```

Headers: `Cache-Control: no-store`, `Pragma: no-cache`, `Content-Type: application/json`.

Error responses (RFC 6749 §5.2):

| Condition | HTTP | `error` |
| --- | --- | --- |
| Missing/duplicate/malformed parameter | 400 | `invalid_request` |
| Bad confidential client auth | 401 | `invalid_client` |
| Unknown/disabled public client | 400 | `invalid_client` |
| Unsupported grant | 400 | `unsupported_grant_type` |
| Code not found, expired, consumed, wrong client, wrong redirect, wrong PKCE, consent no longer covers scope | 400 | `invalid_grant` |
| Client not allowed to use this grant | 400 | `unauthorized_client` |
| Requested scope override present or out of policy | 400 | `invalid_scope` |

### 1.3 `POST /token` - refresh-token grant

Governing specs: RFC 6749 §5.1, §5.2, §6; RFC 7009 §2.1; OAuth 2.1 §6.1; RFC 9700 §4.14.

Accepted parameters: `grant_type=refresh_token`, `refresh_token`, `client_id` for public CLI clients; confidential client auth when the registry requires it. `scope` may be omitted or may request a subset of the original grant. Superset requests are rejected.

Ordered validation and processing:

1. Parse form body and client authentication as in §1.2. Public CLI clients authenticate by possession of the refresh token plus registry `client_id`; no static secret. RFC 6749 §2.3; RFC 8252 §8.5.
2. Hash the presented refresh token with HMAC-SHA256 using a refresh-token hash key. Never log or store the raw token.
3. In one transaction, select the `refresh_tokens` row and its `refresh_token_families` row `FOR UPDATE`.
4. If the token is unknown, family expired, family revoked, token expired, token revoked, or client/subject binding mismatches: return `invalid_grant`. RFC 6749 §5.2.
5. If the token exists but is already rotated/consumed, treat this as replay. Set `refresh_token_families.revoked_at = NOW()`, `revoke_reason = 'reuse_detected'`; mark every non-revoked token in the family revoked; write `token_revocations(client_id, sub, revoked_after=NOW())`; return `invalid_grant`. OAuth 2.1 §6.1; RFC 9700 §4.14. Maps C7.
6. If active, rotate: mark old token `consumed_at = NOW()`, `status = 'rotated'`; insert exactly one new active child with `rotated_from = old.token_hash`, same family, same client, same subject, narrowed scope if requested, `expires_at <= family.expires_at`. A partial unique index enforces one active token per family.
7. Mint a new access token; do not mint an `id_token` on refresh unless the original authorization code had `openid` and a stored nonce. For browser BFF, this grant is not available.

Concurrency rule: if two refresh requests race with the same active token, one commits the rotation; the loser observes `status='rotated'` after lock acquisition and revokes the whole family. This is deliberate: the AS cannot distinguish a retry race from token theft, and RFC 9700 §4.14 chooses family revocation.

Success response: same shape as RFC 6749 §5.1 with a new `refresh_token`; always `Cache-Control: no-store`, `Pragma: no-cache`.

Errors: standard RFC 6749 §5.2. Replay, expired, revoked, wrong client, and wrong family all return `invalid_grant` with no detail that helps oracle attacks. Security logs include `family_id`, `client_id`, and subject, never token values.

### 1.4 `POST /device/authorization`

Governing specs: RFC 8628 §3.1, §3.2, §5.1, §5.4, §6.1; RFC 6749 §3.3, §5.2.

Accepted parameters: `client_id`, optional `scope`.

Ordered validation and processing:

1. Require TLS except insecure-dev loopback. C11.
2. Look up the client; it must be a public CLI/device client and allowed to use the device grant. RFC 8628 §3.1.
3. Validate requested scopes against the client allowlist. If omitted, use the CLI default scope set. Unknown or disallowed scopes return `invalid_scope`. RFC 6749 §3.3.
4. Generate `device_code` with at least 256 bits of entropy. Store only `device_code_hash = HMAC-SHA256(device_code_hash_key, device_code)`. RFC 8628 §5.1. Maps C9.
5. Generate `user_code` from the non-confusable alphabet already used by the tree (`BCDFGHJKLMNPQRSTVWXZ`), grouped as `XXXX-XXXX`, with enough entropy and an attempt/rate-limit budget. RFC 8628 §6.1. Maps C9.
6. Insert a device grant row with status `pending`, `client_id`, canonical scopes, `expires_at = now + 10 minutes`, `interval = 5`, `last_polled_at = NULL`, and no subject. Unique constraints cover `device_code_hash` and active `user_code`.

Success response (RFC 8628 §3.2):

```json
{
  "device_code": "<opaque>",
  "user_code": "BCDF-GHJK",
  "verification_uri": "https://auth.zeroship.ai/device",
  "verification_uri_complete": "https://auth.zeroship.ai/device?user_code=BCDF-GHJK",
  "expires_in": 600,
  "interval": 5
}
```

Errors: `invalid_request`, `invalid_client`, `unauthorized_client`, `invalid_scope`, `server_error` using RFC 6749 §5.2 JSON shape.

### 1.5 `POST /token` - device-code polling grant

Governing specs: RFC 8628 §3.4, §3.5, §5.1, §5.4; RFC 6749 §5.1, §5.2; RFC 9068 §2.

Accepted parameters: `grant_type=urn:ietf:params:oauth:grant-type:device_code`, `device_code`, `client_id`.

Ordered validation and processing:

1. Parse form body; reject wrong `grant_type` as `unsupported_grant_type`.
2. Look up client and verify it matches the stored device grant's `client_id`. Wrong client returns `invalid_grant`; unknown client returns `invalid_client`.
3. Hash `device_code`; select the device grant row `FOR UPDATE`.
4. If no row or expired: delete stale row if present and return `expired_token`. RFC 8628 §3.5.
5. Enforce polling interval. If `now - last_polled_at < interval`, update `last_polled_at`, increase the grant's required interval by 5 seconds for subsequent polls, and return `slow_down`. RFC 8628 §3.5. Maps C9.
6. If status is `pending`, update `last_polled_at` and return `authorization_pending`. RFC 8628 §3.5. Maps C9.
7. If status is `denied`, delete the row and return `access_denied`. RFC 8628 §3.5.
8. If status is `approved`, require `approved_subject`, `approved_at`, and an unexpired grant. Delete or mark the device grant consumed in the same transaction so the device code is one-use. RFC 8628 §5.1.
9. Mint an RFC 9068 access token for the approved subject and client. Issue a CLI refresh token if the approved scopes include `offline_access`. Do not mint a nonce-less `id_token` for the v1 CLI device grant; the retained-critical `nonce` requirement applies to OIDC auth-code requests.

Success response: RFC 6749 §5.1 JSON, `Cache-Control: no-store`, `Pragma: no-cache`.

Device-specific errors (RFC 8628 §3.5): `authorization_pending`, `slow_down`, `access_denied`, `expired_token`. Other errors use RFC 6749 §5.2.

### 1.6 `GET /.well-known/openid-configuration`

Governing specs: RFC 8414 §2, §3, §3.3; OIDC Discovery 1.0 §3, §4; RFC 9207 §3.

Response is `application/json` with `Cache-Control: public, max-age=300` unless local dev disables caching. The `issuer` must be an HTTPS URL with no query or fragment and must exactly equal token `iss` and authorization-response `iss`.

Required metadata:

```json
{
  "issuer": "https://auth.zeroship.ai/",
  "authorization_endpoint": "https://auth.zeroship.ai/authorize",
  "token_endpoint": "https://auth.zeroship.ai/token",
  "device_authorization_endpoint": "https://auth.zeroship.ai/device/authorization",
  "revocation_endpoint": "https://auth.zeroship.ai/revoke",
  "jwks_uri": "https://auth.zeroship.ai/jwks.json",
  "response_types_supported": ["code"],
  "grant_types_supported": [
    "authorization_code",
    "refresh_token",
    "urn:ietf:params:oauth:grant-type:device_code"
  ],
  "code_challenge_methods_supported": ["S256"],
  "token_endpoint_auth_methods_supported": ["none", "client_secret_basic", "client_secret_post"],
  "scopes_supported": ["openid", "profile", "email", "offline_access"],
  "subject_types_supported": ["pairwise"],
  "id_token_signing_alg_values_supported": ["EdDSA"],
  "authorization_response_iss_parameter_supported": true,
  "backchannel_logout_supported": true,
  "backchannel_logout_session_supported": true,
  "claims_supported": ["iss", "sub", "aud", "exp", "iat", "auth_time", "nonce", "at_hash", "email", "email_verified", "name", "picture", "amr", "acr"]
}
```

Closed-world omissions: no DCR metadata, no implicit response types, no hybrid response types, no ROPC, no FAPI/PAR/JARM. Introspection metadata may be omitted if all first-party resource servers validate JWT access tokens locally through JWKS; if an introspection endpoint is exposed for migration, it must be first-party authenticated per RFC 7662 §2.1, §2.3.

Errors: malformed host/issuer configuration is a deployment failure and returns HTTP 500 with `Cache-Control: no-store`; transient store/key-load failures return HTTP 503 with no partial metadata body. The issuer value must never be guessed from an untrusted `Host` header.

Maps C1, C5, C10, C12, C15.

### 1.7 `GET /jwks.json`

Governing specs: RFC 7517 §4, §5; RFC 7515 §4.1.4, §10.7; RFC 8037 §2 for OKP/Ed25519 JWK shape.

Response is a JWK Set:

```json
{
  "keys": [
    {
      "kty": "OKP",
      "crv": "Ed25519",
      "use": "sig",
      "kid": "<rfc7638-thumbprint>",
      "alg": "EdDSA",
      "x": "<base64url-public-key>"
    }
  ]
}
```

Serve public keys whose status is `active`, `next`, or `retiring`. Never serve private key material. `Cache-Control: public, max-age=300, stale-while-revalidate=300` is acceptable only if the key-overlap window accounts for it. Maps C10, C12.

Errors: key-store unavailable returns HTTP 503 with `Cache-Control: no-store`; malformed key material is a boot/config failure and should fail service startup rather than serve an incomplete JWKS.

### 1.8 `POST /revoke`

Governing specs: RFC 7009 §2.1, §2.2, §2.2.1; RFC 6749 §2.3; RFC 6750 §2.1; RFC 9700 §4.14.

Accepted parameters: `token`, optional `token_type_hint` (`access_token` or `refresh_token`), optional `client_id` for public clients. Confidential clients authenticate according to their registry row.

Ordered validation and processing:

1. Parse form body; require `token`.
2. Authenticate/identify the client. Public clients send `client_id`; confidential clients authenticate. Failed confidential auth returns `invalid_client`.
3. If `token_type_hint=refresh_token` or lookup by refresh-token hash succeeds: revoke the whole refresh family, mark active tokens revoked, and write `token_revocations(client_id, sub, revoked_after=NOW())`. RFC 7009 §2.1. Maps C7, C15.
4. If `token_type_hint=access_token` or JWT access-token verification succeeds: verify signature, `typ: at+jwt`, `alg=EdDSA`, `iss`, `exp`, `aud`, and client binding; then write `token_revocations(client_id, sub, revoked_after=NOW())`. This is stronger than single-JTI revocation and matches the existing per-app family-marker pattern. RFC 7009 §2.1; RFC 9068 §4. Maps C6, C10, C15.
5. Unknown, already revoked, or mismatched token still returns HTTP 200 with empty body. RFC 7009 §2.2. Do not reveal token existence.
6. Unsupported token type hints return RFC 7009 §2.2.1 `unsupported_token_type` only when the hint itself is unsupported; unknown token values are still 200.

Success: `200 OK`, empty JSON or empty body, `Cache-Control: no-store`.

### 1.9 Back-channel logout origination

Governing specs: OIDC Back-Channel Logout 1.0 §2.4, §2.5, §2.6; RFC 7519 §4.1.7; RFC 7515 §4.1.4; RFC 7009.

Trigger sources: end-user logout, password reset, account disable, explicit grant revoke, refresh family reuse detection, app disconnect, or admin session termination.

Ordered behavior:

1. Resolve affected clients/RPs from active sessions/grants. For app clients, use `app_oauth_clients.client_id` and any registered `backchannel_logout_uri`.
2. Revoke local platform sessions and token families first. Browser BFF cookies are invalidated by gateway session/token revocation, not by issuing OAuth refresh. Maps C15.
3. For each RP, mint a Logout Token JWT signed with active EdDSA key. Header: `alg=EdDSA`, `kid`, `typ=logout+jwt`. Claims: `iss`, `aud=client_id`, `iat`, `jti`, `events={"http://schemas.openid.net/event/backchannel-logout":{}}`, and at least one of `sub` or `sid`. For app clients, `sub` is the pairwise subject for that app. `nonce` MUST NOT be present. OIDC BCL §2.4. Maps C10, C15.
4. POST `application/x-www-form-urlencoded` body `logout_token=<jwt>` to the RP's `backchannel_logout_uri`. OIDC BCL §2.5.
5. Treat 2xx as delivered; retry transient failures with bounded exponential backoff. Do not block local logout completion on remote delivery; record failed deliveries for ops/audit.

## 2. Token formats

### 2.1 JOSE header common rules

All platform-signed JWTs use EdDSA over Ed25519 through `jsonwebtoken`. Header:

```json
{ "alg": "EdDSA", "kid": "<key-id>", "typ": "<token-type>" }
```

`kid` is the RFC 7638 thumbprint of the public OKP Ed25519 JWK unless ops explicitly imports a key with a precomputed, collision-free `kid`. Verifiers must match by both `kid` and `alg`, then pin `alg=EdDSA` for platform tokens. `alg:none` is never accepted. RFC 7515 §4.1.1, §4.1.4, §10.7. Maps C10.

### 2.2 Access token - RFC 9068 JWT

Header: `typ: "at+jwt"`, `alg: "EdDSA"`, `kid`.

Required claims:

| Claim | Rule |
| --- | --- |
| `iss` | Canonical platform issuer, production `https://auth.zeroship.ai/`. Must exactly match discovery. |
| `sub` | App/end-user tokens: pairwise `pws_...` from `derive_pairwise(pairwise_salt, global_user_id, sector_identifier)`. Control/deploy tokens may use the platform subject only when no app sector exists and must not be forwarded to workers. |
| `aud` | Resource audience. For creator app resource-server paths, the per-app audience is the app OAuth `client_id` / app resource identifier. For control deploy tokens, `control.zeroship.ai`. |
| `exp` | Short-lived; default 15 minutes. |
| `iat` | Current Unix timestamp. |
| `jti` | 128-bit or stronger unique token id. |
| `client_id` | OAuth client that requested the token. Required by RFC 9068 §2.2. |

Additional claims: `scope` (space-separated granted scopes; RFC 9068 §2.2.3), `auth_time`, `amr`, `acr`, and optional `cnf` only if DPoP is introduced later. No bearer token is accepted in a URL query string. RFC 6750 §2.1, §2.3. Maps C6, C11.

### 2.3 ID token - OIDC Core

Issued only for authorization-code grants whose granted scope contains `openid`. The zeroship profile requires `nonce` on those authorization requests so every `id_token` is nonce-bound.

Header: `typ: "JWT"` or omitted per OIDC Core; `alg=EdDSA`; `kid`.

Required claims:

| Claim | Rule |
| --- | --- |
| `iss` | Canonical platform issuer. |
| `sub` | Pairwise subject for the app/client sector. OIDC Core §8.1. |
| `aud` | OAuth `client_id`; include `azp` when `aud` has multiple values. |
| `exp` | Short-lived; not longer than the access token. |
| `iat` | Current Unix timestamp. |
| `nonce` | Exact value stored on the authorization code from `/authorize`. Required by this profile. |
| `auth_time` | Time of end-user authentication. Required when `max_age` is supported/requested; otherwise included when known. |
| `at_hash` | Always included when an access token is issued with the ID token. For EdDSA, compute SHA-512 over the ASCII access token, take the leftmost 256 bits, base64url no padding, matching existing verifier behavior. OIDC Core §3.1.3.6. |

Optional claims: `amr`, `acr`, `email` (relay alias, never real inbox for creator apps), `email_verified`, `name`, `picture`. `c_hash` is not required because the closed-world profile does not use hybrid flow; if present, verifiers may validate it like existing `oidc_verify`.

This is a retained-critical CVE class: `nonce`, `at_hash`, `aud`, `iss`, `kid`, and pairwise `sub` are all mandatory tests. Maps C4, C10, C12.

### 2.4 Refresh token - CLI only

The BFF/browser face receives no OAuth refresh token. The gateway re-mints short-lived access/session artifacts from its live gateway session, not from an OAuth refresh family.

The CLI/programmatic face receives opaque rotating refresh tokens only when granted `offline_access`. Token value format:

```text
zrt_<base64url(32 random bytes)>
```

The database stores only `token_hash = HMAC-SHA256(refresh_token_hash_key, token_value)`. The token value has no embedded `family_id`, client id, subject, or expiry. Rotation and replay detection are specified in §4.1. Maps C7.

## 3. Data model

### 3.1 Reused and extended tables

| Table | Status | Use in platform OP |
| --- | --- | --- |
| `zeroship.oauth_clients` | Extend and make authoritative | Client lookup, exact redirect allowlist, scope allowlist, skip-consent flag. Drop Hydra mirror writes in the implementation phase. Add `client_type`, `token_endpoint_auth_method`, optional `backchannel_logout_uri`, and the migration `issuer_mode`. |
| `zeroship.app_oauth_clients` | Reuse | App id to `client_id`; `sector_identifier` for pairwise subject and relay scoping. |
| `zeroship.oauth_grants` | Extend as the consent ledger | Subject x client x granted scopes. Add remember TTL fields and revocation fields rather than creating a parallel grant ledger. |
| `zeroship.app_user_identities` | Reuse | Pairwise subject persistence and relay alias linkage: `(app_client_id, global_user_id) -> pairwise_sub, relay_email`. |
| `zeroship.token_revocations` | Reuse | Per-app `(client_id, sub)` family marker for access-token/session invalidation. |
| `zeroship.app_scope_defs` | Reuse | App-declared scope vocabulary used by `/authorize` and consent classification. |

### 3.2 New and extended table specs

`zeroship.auth_codes` - net-new:

| Column | Rule |
| --- | --- |
| `code_hash TEXT PRIMARY KEY` | HMAC-SHA256 of the opaque code. Raw code is never stored. |
| `client_id TEXT NOT NULL` | FK to `oauth_clients(client_id)`. Re-verified at `/token`. |
| `redirect_uri TEXT NOT NULL` | Exact URI from `/authorize`. Re-verified byte-for-byte at `/token`. |
| `code_challenge TEXT NOT NULL` | S256 challenge from `/authorize`. |
| `code_challenge_method TEXT NOT NULL CHECK = 'S256'` | No `plain`. |
| `scope TEXT[] NOT NULL` | Sorted-deduped granted scopes snapshot. `/token` does not accept scope override. |
| `nonce TEXT` | Required when `openid` is in scope; copied to `id_token`. |
| `subject UUID NOT NULL` | Global platform user id. |
| `pairwise_sub TEXT NOT NULL` | App/client projected subject used in app tokens. |
| `sector_identifier TEXT NOT NULL` | Input to re-derive pairwise subject. |
| `auth_time TIMESTAMPTZ NOT NULL` | End-user auth event time. |
| `amr TEXT[] NOT NULL DEFAULT '{}'` | Authentication methods. |
| `consent_grant_id TEXT` | Logical pointer to consent/grant version, if added; otherwise `(subject, client_id)` is the key. |
| `issued_at TIMESTAMPTZ NOT NULL DEFAULT NOW()` | Audit. |
| `expires_at TIMESTAMPTZ NOT NULL` | Must be `<= issued_at + interval '60 seconds'`. |
| `consumed_at TIMESTAMPTZ` | Single-use marker. |
| `consumed_by_client_id TEXT` | Audit/debug; must equal `client_id`. |
| `token_set_id UUID` | Optional audit link to tokens minted from this code. |

Indexes: `auth_codes_expires_at_idx`; optional `auth_codes_subject_client_idx`. Reaping deletes expired/consumed rows. The `/token` binding set is exactly: `code_hash`, `client_id`, `redirect_uri`, `code_challenge`, `code_challenge_method`, `scope`, `nonce`, `subject`, `pairwise_sub`, `sector_identifier`, `auth_time`, `amr`, `expires_at`, and `consumed_at`.

`zeroship.refresh_token_families` - net-new:

| Column | Rule |
| --- | --- |
| `family_id TEXT PRIMARY KEY` | `rfam_...` random identifier, not exposed in token value. |
| `client_id TEXT NOT NULL` | FK to `oauth_clients`. |
| `subject TEXT NOT NULL` | Subject the token family belongs to. App tokens use pairwise `pws_...`; control tokens may use platform user id. |
| `scope TEXT[] NOT NULL` | Maximum refreshable scope set. |
| `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()` | Audit. |
| `expires_at TIMESTAMPTZ NOT NULL` | Absolute lifetime; default 30 days, never slid. |
| `revoked_at TIMESTAMPTZ` | Family dead marker. |
| `revoke_reason TEXT` | `user_revoke`, `logout`, `reuse_detected`, `expired`, `admin`, `password_reset`. |
| `last_rotated_at TIMESTAMPTZ` | Updated on successful rotation. |

Unique/indexes: `(client_id, subject) WHERE revoked_at IS NULL` optional if the product wants one live family per CLI client/user; `expires_at` reaper index.

`zeroship.refresh_tokens` - net-new:

| Column | Rule |
| --- | --- |
| `token_hash TEXT PRIMARY KEY` | HMAC-SHA256 of raw token. |
| `family_id TEXT NOT NULL` | FK to `refresh_token_families`. |
| `client_id TEXT NOT NULL` | Duplicated binding for cheap checks. |
| `subject TEXT NOT NULL` | Duplicated binding for cheap checks. |
| `rotated_from TEXT` | Previous token hash in the chain; null for root. |
| `replaced_by TEXT` | New token hash after a successful rotation. |
| `status TEXT NOT NULL` | `active`, `rotated`, `revoked`, `reused`. |
| `issued_at TIMESTAMPTZ NOT NULL DEFAULT NOW()` | Audit. |
| `expires_at TIMESTAMPTZ NOT NULL` | `<= family.expires_at`. |
| `consumed_at TIMESTAMPTZ` | Set on successful use. |
| `last_seen_at TIMESTAMPTZ` | Optional replay/audit. |

Constraint: partial unique index `refresh_tokens_one_active_per_family` on `(family_id) WHERE status = 'active'`. This makes accidental double-active issuance impossible.

`zeroship.oauth_grants` / consent ledger - extended:

| Column | Rule |
| --- | --- |
| existing `user_id`, `client_id`, `granted_scopes` | Reused subject x client x granted scopes. |
| `remember_expires_at TIMESTAMPTZ` | Null means session-only/no remembered silent consent. |
| `last_prompted_at TIMESTAMPTZ` | Audit. |
| `revoked_at TIMESTAMPTZ` | Consent revoked without deleting audit history. |
| `relay_alias_linked BOOLEAN` or view | Derived from `app_user_identities.relay_email IS NOT NULL`; do not duplicate the email unless a denormalized audit column is explicitly needed. |

Consent linkage: the authoritative relay alias remains in `app_user_identities` keyed by `(client_id, user_id)`. Consent accept for `email` scope calls `mint_alias_at_consent` under the grant lock. The grant row and identity row together satisfy "subject x client x scopes x relay alias".

`zeroship.signing_keys` - net-new:

| Column | Rule |
| --- | --- |
| `kid TEXT PRIMARY KEY` | RFC 7638 JWK thumbprint. |
| `alg TEXT NOT NULL CHECK = 'EdDSA'` | Only EdDSA. |
| `private_key_enc BYTEA NOT NULL` | AES-256-GCM encrypted PKCS#8 DER using `core::crypto`. |
| `public_jwk JSONB NOT NULL` | Public OKP JWK served by JWKS. |
| `status TEXT NOT NULL` | `active`, `next`, `retiring`, `retired`, `compromised`. |
| `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()` | Audit. |
| `not_before TIMESTAMPTZ NOT NULL DEFAULT NOW()` | Earliest signing use. |
| `activated_at TIMESTAMPTZ` | Set when promoted to active. |
| `retiring_started_at TIMESTAMPTZ` | Set when replaced by new active. |
| `retire_after TIMESTAMPTZ` | Earliest removal from JWKS. |
| `retired_at TIMESTAMPTZ` | No longer served/accepted. |
| `last_used_at TIMESTAMPTZ` | Signing audit. |

Only one `active` key is allowed. At most one `next` key is expected. `retiring` keys remain in JWKS until the overlap window closes.

## 4. Highest-CVE-density decisions

### 4.1 Refresh decision and CLI rotation scheme

Decision by face:

| Face | Decision |
| --- | --- |
| Browser/BFF | No OAuth refresh token. The gateway session cookie is the longevity anchor and the gateway re-mints short-lived session/access artifacts from that live session. Existing BFF `anchors.rs` pattern is retired for this face. |
| CLI/programmatic | Platform-issued opaque rotating refresh tokens with replay detection and family revocation. |

CLI refresh algorithm:

1. Initial issuance occurs after successful auth-code or device-code grant with `offline_access`.
2. Create `refresh_token_families` with absolute `expires_at = created_at + 30 days` by default. The family lifetime is not slid by rotation.
3. Insert root `refresh_tokens` row with `status='active'`, `rotated_from=NULL`, `expires_at=family.expires_at`.
4. On every refresh grant, run the §1.3 transaction. Successful use always rotates and returns a new refresh token. The old token can never be used again.
5. Reuse detection: any presentation of a token with `status in ('rotated','reused')` or `consumed_at IS NOT NULL` is a replay signal. Revoke the entire family, revoke all active descendants, write `token_revocations`, and return `invalid_grant`.
6. Expiry: expired token or family returns `invalid_grant`; a sweeper marks expired families `revoke_reason='expired'`.
7. Revocation: `/revoke`, logout, password reset, account disable, and device/app disconnect all revoke the family and write the per-app access-token family marker.
8. Logging: token value and token hash are secrets; logs may include `family_id`, client id, subject, status, and reason.

Tradeoff explicitly rejected for v1 unless implementation risk forces a rollback: CLI could re-run the device grant when access expires, deleting refresh-token replay risk at the cost of recurring UX friction. If chosen, remove `refresh_tokens`/`refresh_token_families` from v1 and omit `offline_access`. The current decision is rotation because it meets OAuth 2.1 §6.1 / RFC 9700 §4.14 while preserving CLI usability.

### 4.2 Authorization-code store binding set

The authorization code is an opaque one-use handle to a full authorization snapshot. `/token` re-verifies exactly:

| Binding | Source | `/token` rule |
| --- | --- | --- |
| `code_hash` | Random code generated by `/authorize` | HMAC hash lookup; raw code never stored. |
| `client_id` | Request client | Must equal token request client. |
| `redirect_uri` | Exact validated redirect | Must equal token request `redirect_uri` byte-for-byte. |
| `code_challenge_method` | `/authorize` | Must be `S256`. |
| `code_challenge` | `/authorize` | Must equal recomputed S256 challenge from `code_verifier`. |
| `scope` | Granted scope after consent | Token request cannot expand or replace it; consent row must still cover it. |
| `nonce` | `/authorize` OIDC request | Copied to `id_token`; no override allowed. |
| `subject` | Authenticated user | Used for audit and re-derivation. |
| `pairwise_sub` | Derived at `/authorize` | Must equal fresh derivation from stored subject + sector. |
| `sector_identifier` | `app_oauth_clients` | Pairwise derivation input. |
| `auth_time`, `amr` | Login event | Copied to tokens. |
| `expires_at` | Code issuance | Must be in future; max TTL 60s. |
| `consumed_at` | Code row | Must be null; set atomically before success. |

Concurrency test-grade requirement: two simultaneous `/token` requests using the same code can produce at most one token response. The loser returns `invalid_grant`. If the loser observes a consumed code with `token_set_id`, the OP audits code reuse and may revoke the token set minted from that code. This satisfies RFC 6749 §10.5 and C14.

## 5. Key rotation / JOSE

### 5.1 Key model

The OP has one platform issuer and one signing-key set for access tokens, ID tokens, and logout tokens. Keys are Ed25519 only. Private keys are encrypted at rest with `core::crypto`; AAD:

```text
zs:auth:signing_key:v1\0{kid}\0EdDSA
```

Use the actual string `kid` in the AAD. Decryption tries the primary master key and legacy keys using `decrypt_with_keys`, matching the existing key-rotation model in `core::crypto`.

### 5.2 Rotation cadence and overlap

Default cadence: create a new signing key every 30 days. Emergency rotation may happen immediately.

JWKS overlap constants:

| Value | Default |
| --- | --- |
| Access token TTL | 15 minutes |
| ID token TTL | 15 minutes max |
| Logout token replay window | 5 minutes |
| JWKS cache TTL | 5 minutes |
| Clock skew | 2 minutes |
| Minimum overlap | 1 hour |

The overlap window is `max(max_signed_token_ttl + jwks_cache_ttl + clock_skew, 1 hour)`. Refresh tokens are opaque and do not extend JOSE overlap.

### 5.3 Bootstrap

1. At boot, auth service opens a DB transaction and takes an advisory lock `auth_signing_key_bootstrap`.
2. If an `active` signing key exists, decrypt it into process memory and publish its public JWK.
3. If none exists, generate Ed25519, compute `kid` as RFC 7638 public JWK thumbprint, encrypt PKCS#8 DER, insert `status='active'`, `activated_at=NOW()`, and commit.
4. Refuse to boot if there is more than one active key or if the active private key cannot decrypt.

### 5.4 Planned rotation

1. Generate new Ed25519 key under advisory lock; insert as `status='next'`.
2. JWKS immediately serves `active + next + retiring`. New tokens are still signed by `active`.
3. After at least one JWKS cache TTL, promote `next` to `active` and change the old active to `retiring` with `retire_after = NOW() + overlap`.
4. New tokens are signed only by the new active key.
5. Verifiers accept keys present in JWKS. Retiring keys stay published until `retire_after`.
6. After `retire_after`, mark retiring keys `retired` and stop serving them.

### 5.5 Emergency compromise

If a private key is suspected compromised:

1. Insert/promote a new active key immediately.
2. Mark compromised key `compromised`, remove from JWKS after an operator-approved forced cut, and revoke affected sessions/token families through `token_revocations`.
3. Expect a logout/re-auth event. This is intentionally disruptive; overlap is for normal rotation, not known compromise.

## 6. Threat model

| Attack | Mitigation in this design | Governing spec | Test |
| --- | --- | --- | --- |
| Authorization-code injection/interception | Require S256 PKCE for every code flow; bind code to client, redirect, PKCE, subject, nonce; code TTL <=60s and one-use. | RFC 7636 §4.6; RFC 6749 §10.5; RFC 9700 §2.1.1 | `authorize_code_requires_s256_and_token_rejects_wrong_verifier`; `auth_code_reuse_is_invalid_grant`. |
| CSRF on `/authorize` or redirect callback | Require/echo `state`; bind browser login/consent transaction to authenticated server state; SameSite/HttpOnly cookies for OP UI state. | RFC 6749 §10.12; RFC 9700 §2.1, §4.7 | `authorize_state_roundtrip_required_for_browser`; `callback_state_mismatch_rejects`. Residual: user may ignore browser warnings; mitigated by TLS and SameSite. |
| Mix-up / wrong AS response | Return `iss` on every authorization response and advertise support; clients verify exact issuer against discovery. | RFC 9207 §2, §2.4, §3; RFC 9700 §4.4 | `authorize_response_includes_iss`; `client_rejects_iss_mismatch`. |
| Open redirect | Never redirect until `redirect_uri` exact-matches registry; no wildcard/path suffix matching; invalid redirect errors render local 400. | RFC 6749 §3.1.2.3, §3.1.2.4; OAuth 2.1 §2.3.1; RFC 9700 §2.1 | `authorize_rejects_unregistered_redirect_without_redirecting`; `loopback_port_is_only_native_exception`. |
| 307 POST credential leak after login/consent | All post-auth redirects use 303 See Other. | RFC 9700 §4.12 | `login_post_finishes_with_303_not_307`; `consent_post_finishes_with_303_not_307`. |
| Refresh-token replay/theft | CLI refresh tokens rotate on every use; replay of a rotated token revokes whole family and access-token family marker. | OAuth 2.1 §6.1; RFC 9700 §4.14 | `refresh_reuse_revokes_family`; `concurrent_refresh_old_token_loser_revokes_family`. Residual: legitimate network retry can cause family revoke; accepted per BCP. |
| Access-token / ID-token confusion | Access tokens use `typ: at+jwt`, audience restriction, `client_id`; ID tokens have `aud=client_id` and are never accepted at resource-server Bearer arms. | RFC 9068 §2.1, §4; RFC 9700 §2.3, §4.10 | `bearer_rejects_id_token_typ`; `id_token_rejects_as_access_token`; `access_token_aud_client_binding_required`. |
| PKCE downgrade | Reject missing `code_challenge`, `plain`, and any method other than `S256`. | RFC 7636 §4.2; OAuth 2.1 §4.1.1 | `authorize_rejects_plain_pkce`; `token_rejects_missing_code_verifier`. |
| Code substitution | `/token` rechecks client id, exact redirect URI, PKCE, consent scope, subject/pairwise derivation, and one-use state. | RFC 6749 §4.1.3, §10.5; RFC 7636 §4.6 | `token_rejects_code_for_other_client`; `token_rejects_code_for_other_redirect`. |
| Token leakage via logs/referrer/URLs | No tokens in query strings except short auth code; token responses no-store; redact token bodies on parse/log errors; Bearer only in Authorization header. | RFC 6750 §2.1, §2.3; RFC 6749 §5.1; RFC 9700 §2.6 | `token_response_has_no_store`; `token_parse_error_redacts_body`; `bearer_query_param_rejected`. Residual: client-side mishandling outside platform; docs warn CLI/SDK. |
| `alg:none` / key confusion | Platform pins EdDSA; verifiers match both `kid` and `alg`; JWKS accepts only OKP Ed25519 for platform issuer; no HS/JWKS fallback. | RFC 7515 §10.7; RFC 9068 §4 | `jwt_alg_none_rejected`; `jwt_hs256_key_confusion_rejected`; `jwt_wrong_kid_rejected`. |
| Replay / nonce substitution | `nonce` required for zeroship OIDC code requests and copied into ID token; gateway/RP validates nonce and `at_hash`; logout token `jti` replay cache. | OIDC Core §2, §3.1.2.1, §3.1.3.6, §3.1.3.7; OIDC BCL §2.6 | `id_token_nonce_mismatch_rejected`; `id_token_at_hash_mismatch_rejected`; `logout_token_jti_replay_ignored`. |
| IDOR on clients/codes | Every code/token/device row is bound to `client_id`; app/client tables are authoritative; token endpoint rechecks binding under lock. | RFC 6749 §3.1.2, §4.1.3, §10.5 | `token_rejects_code_with_different_client_id`; `device_poll_wrong_client_invalid_grant`. |
| Consent CSRF | Consent accept/deny POST uses CSRF token; POST re-runs scope classifier and never trusts rendered UI; grants updated under advisory lock. | RFC 6749 §10.12; RFC 9700 §4.7 | `consent_accept_requires_csrf`; `consent_accept_reclassifies_unknown_scope`. |
| Device-code phishing | Verification UI shows client name/scopes and requires explicit signed-in user approval; `verification_uri_complete` convenience does not auto-approve. | RFC 8628 §5.4 | `device_verification_requires_explicit_approval`; `device_complete_uri_does_not_skip_approval`. Residual: user may type code into phishing site; user education and origin `auth.zeroship.ai` remain necessary. |
| Device `user_code` brute force | Non-confusable high-entropy code; rate-limit attempts by IP/session/code; short 10-minute expiry; failed attempts audited. | RFC 8628 §5.1, §5.2, §6.1 | `device_user_code_rate_limit`; `device_user_code_expires`; `device_code_entropy_shape`. |
| Device polling abuse | Enforce `interval`; too-fast polls return `slow_down` and add 5 seconds. | RFC 8628 §3.4, §3.5 | `device_poll_before_interval_returns_slow_down_and_increments_interval`. |
| Scope escalation | `/authorize` validates against client allowlist and `app_scope_defs`; POST consent re-runs classifier; `/token` cannot expand scopes. | RFC 6749 §3.3; RFC 6749 §5.2 `invalid_scope` | `authorize_unknown_scope_invalid_scope`; `token_scope_override_rejected`; `consent_union_does_not_drop_prior_scopes`. |
| Cross-app subject/email correlation | App tokens and ID tokens use pairwise `sub`; creator-app email claim is relay alias minted at consent, not real inbox. | OIDC Core §8.1, §8.2 | `app_tokens_use_pairwise_sub`; `email_scope_uses_relay_alias`. Residual: user may voluntarily reveal real email inside app. |
| Logout/revocation gap | `/revoke`, logout, password reset, and refresh replay write `token_revocations`; OP originates BCL logout tokens with no nonce and RP replay checks. | RFC 7009 §2.1; OIDC BCL §2.4-§2.6 | `refresh_reuse_writes_token_revocation`; `logout_token_has_events_no_nonce`; `bcl_failure_does_not_skip_local_revocation`. |
| JWKS rotation outage | Publish next before use; serve retiring until all old tokens plus cache/skew expire; verifiers stale-on-error last-good keys. | RFC 7517 §5; RFC 7515 §4.1.4 | `jwks_serves_active_next_retiring`; `old_token_verifies_during_overlap`; `retired_key_removed_after_overlap`. |

## 7. Dual-issuer migration mechanics

Migration is issue-first. The platform starts minting platform-`iss` tokens before any verifier stops accepting Hydra. The gateway BFF cookie is excluded because it is already gateway/platform-signed and is not a raw Hydra issuer token.

Issuer invariant:

| Scope | Rule |
| --- | --- |
| Per token | Exactly one `iss`. A token is platform-issued or Hydra-issued, never both. |
| Per app | Exactly one active issuer mode behind a registry flag: `issuer_mode = 'hydra' | 'platform'`. |
| Per fleet during migration | Dual issuer. Some apps may still use Hydra while platform-cutover apps use platform OP. |

Exact code sites that gain a platform-issuer arm:

| Site | Current role | Platform arm |
| --- | --- | --- |
| `crates/gateway/src/router/auth.rs` Bearer/DPoP path | Peeks raw Hydra `iss`; verifies/introspects Hydra access token; binds `client_id`. | If `iss == platform_issuer`, verify RFC 9068 JWT via platform JWKS, enforce `typ=at+jwt`, EdDSA, `aud`, `client_id`, expiry, revocation marker, and pairwise projection. |
| `crates/gateway/src/router/auth.rs` introspection path | Calls Hydra introspection for DPoP/opaque fallback. | Prefer local platform JWT verification; keep Hydra introspection only for Hydra-issuer apps until P6. |
| `crates/gateway/src/oidc_rp.rs` ID-token verify | Verifies Hydra ID token through `core::oidc_verify`. | Dial platform `/authorize`/`/token`; verify platform `id_token` against platform JWKS, issuer, audience, nonce, at_hash. Hydra circuit breaker deletes after cutover. |
| `crates/core/src/auth_provider/mod.rs` deploy-token seam | `AuthProvider::{Hydra, Supabase}` verifies deploy/control tokens by introspection/JWKS. | Add `Platform` JWKS arm that verifies platform access tokens for control/deploy audience. Remove Hydra arm last. |
| `crates/gateway/src/backchannel_logout.rs` | Receives Hydra-originated logout tokens. | During migration accept Hydra and platform logout tokens by issuer/JWKS; final shape is platform-originated BCL or platform-internal fan-out. |

Cutover sequence:

1. P1: add platform issuer, JWKS, signing keys, and verifier arms. No app is cut over yet.
2. P2/P3: platform device and auth-code flows mint platform tokens for selected clients.
3. P4/P5: gateway RP dials platform OP for flagged apps; raw Hydra arms remain for unflagged apps.
4. Rollback before P6: flip an app's `issuer_mode` back to `hydra`. Verifiers still accept both.
5. P6: once every app is platform and no raw Hydra tokens remain within max TTL/overlap, remove Hydra issuer recognition, Hydra sidecar/config/schema, HydraAdmin, and `crates/auth/src/hydra_client/`.

## 8. P0 self-check

| Requirement | Covered by |
| --- | --- |
| Every endpoint behavior cites RFC/OIDC sections | §1 subsections list governing specs and per-step references. |
| Every §5A checklist item maps to an endpoint step | §0 keys plus endpoint step mappings; summary below. |
| Auth-code binding set is exact and test-grade | §3.2 `auth_codes`; §4.2. |
| CLI refresh reuse-detection is exact and test-grade | §1.3; §3.2 refresh tables; §4.1. |
| Data model states net-new vs reused/extended | §3.1 and §3.2. |
| Key rotation/JWKS overlap defined | §5. |
| Threat model maps attack to mitigation/spec/test | §6. |
| Dual-issuer migration names code sites | §7. |

Checklist coverage:

| ID | Endpoint/section step |
| --- | --- |
| C1 | `/authorize` steps 6 and `/token` auth-code step 7. |
| C2 | `/authorize` step 5 and `/token` auth-code step 6. |
| C3 | `/authorize` step 9 and consent steps 10-11. |
| C4 | `/authorize` step 8 and `/token` auth-code step 9; token format §2.3. |
| C5 | `/authorize` success step 14 and discovery §1.6. |
| C6 | `/token` auth-code/device success and token format §2.2. |
| C7 | `/token` refresh §1.3, `/revoke` §1.8, refresh design §4.1. |
| C8 | `/authorize` redirect validation step 5 and CLI/client rules in §1.4. |
| C9 | `/device/authorization` §1.4 and device polling §1.5. |
| C10 | JOSE §2.1, JWKS §1.7, token verification in `/revoke` §1.8. |
| C11 | `/authorize` step 1, device step 1, token leakage controls in §6. |
| C12 | Discovery §1.6, JOSE/token issuer rules §2, migration §7. |
| C13 | `/authorize` step 14. |
| C14 | `/authorize` step 13, `/token` auth-code steps 4-8, §4.2. |
| C15 | `/revoke` §1.8 and back-channel logout §1.9. |
