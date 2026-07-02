# P0 OP protocol spec, threat model, and key rotation design

**Status:** proposal (P0 pre-implementation gate).
**Date:** 2026-06-30.
**Scope:** design/spec only. No protocol code in this phase.
**Build constraints:** zero-tokio; use compio/ntex/cyper/compio-postgres/jsonwebtoken.
**Parent design:** `docs/proposals/2026-06-30-self-contained-auth-replace-hydra.md`.
**Standards map:** `docs/proposals/oauth2-standards-conformance.md`.

This document makes the self-contained OAuth2/OIDC Authorization Server buildable by pinning the endpoint contract, mint formats, storage binding sets, CLI refresh rotation, JOSE rotation, threat model, and dual-issuer migration mechanics.

Normative baseline: OAuth 2.1, RFC 9700 / BCP 240, RFC 6749, RFC 6750, RFC 7636, RFC 8628, RFC 7009, RFC 8414, RFC 9068, RFC 9207, OIDC Core 1.0, OIDC Discovery 1.0, OIDC Back-Channel Logout 1.0, and OIDC RP-Initiated Logout 1.0.

<!-- Added in round 1: addressing critic's MF-1/MF-2/MF-3/MF-4/MF-5. This spec was authored before the same-day canonical schema redesign and is now reconciled against `docs/proposals/2026-06-30-auth-schema-redesign.md`. The schema-redesign table/column names are authoritative; where this spec and the redesign once disagreed, the redesign wins. The OP private signing key is **file-based** (`AUTH_SIGNING_KEY_FILE`), never DB-resident. The OAuth code/refresh/signing-key tables are exactly `zeroship.oauth_authorization_codes`, `zeroship.oauth_refresh_tokens` (single table, family-by-column), and `zeroship.signing_keys` (public JWK + metadata only). -->

Canonical-schema invariant: P0 binds to the canonical names in `docs/proposals/2026-06-30-auth-schema-redesign.md` §3. The OP-owned tables are `zeroship.oauth_authorization_codes` (single-use code store), `zeroship.oauth_refresh_tokens` (refresh family by `refresh_family_id` column — there is **no** separate families table), `zeroship.signing_keys` (public JWK + rotation metadata only; **no private-key column**), and `zeroship.token_revocations` (column `token_subject`, not `sub`). All live in the single `zeroship` schema.

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
12. Compute the pairwise subject for app clients with `derive_pairwise(pairwise_salt, user_id, sector_identifier)`, where `sector_identifier` comes from `zeroship.app_oauth_clients.sector_identifier` for the app client. Upsert the canonical pairwise row `zeroship.app_user_identities(client_id, user_id, pairwise_sub)` (canonical columns; old `app_client_id`/`global_user_id` are renamed to `client_id`/`user_id` per schema-redesign §`app_user_identities`) before issuing the code. <!-- Added in round 1: addressing MF-3 (canonical app_user_identities columns) + SF-9 (salt provenance, pinned below). --> `pairwise_salt` is **not** stored on the code or in any OAuth table: it is derived at boot via `core::auth::derive_pairwise_salt(secret_bytes)` from a single global platform secret loaded from `AUTH_PAIRWISE_SALT_FILE` (or the same KMS custody path as `AUTH_SIGNING_KEY_FILE`). The salt is global (one secret for the deployment); per-app uniqueness of `sub` comes from the `sector_identifier` input, not from a per-sector salt. Because `/token` re-derives the pairwise subject (see §4.2), salt custody is load-bearing and is pinned alongside the signing-key file decision (§5.1).
13. Create the authorization code transactionally: generate at least 256 bits of entropy, store only `code_hash`, bind the full set in §4.2, set `expires_at <= now + 60s`, `consumed_at = NULL`. RFC 6749 §4.1.2, §10.5. Maps C14.
14. Redirect with HTTP 303 See Other to the exact registered `redirect_uri`, never 307. Include `code`, `state` if supplied, and `iss=<issuer>` in the query. RFC 6749 §4.1.2; RFC 9207 §2; RFC 9700 §4.12. Maps C5, C13.

Success response:

```text
303 See Other
Location: {redirect_uri}?code={opaque_code}&state={state}&iss={platform_issuer}
Cache-Control: no-store
Referrer-Policy: no-referrer
```

<!-- Added in round 1: addressing SF-4 (referrer leakage of the code). -->
`Referrer-Policy: no-referrer` is mandatory on the code redirect response. The code rides in the redirect query string (`?code=...`); without it the RP callback page can leak the code cross-site through the `Referer` header (RFC 9700 §4.2.4). This is enforced by header, not just claimed, and is asserted by `authorize_code_redirect_sets_referrer_policy_no_referrer`.

<!-- Added in round 1: addressing SF-2 (clickjacking / framing of the OP UI). -->
Framing protection on the OP-rendered HTML (login, consent, device-approval pages): every OP UI response sets `Content-Security-Policy: frame-ancestors 'none'` and `X-Frame-Options: DENY`. The OP UI is never legitimately framed (the platform login is the immersive same-site model, not an embedded iframe of the consent page), so framing is denied outright. RFC 9700 §4.x calls out consent-page framing; this is the platform's standing `frame-ancestors`/Strict-CSRF posture applied to the OP. Asserted by `consent_page_sets_frame_ancestors_none`.

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
2. Authenticate confidential clients when their registry row requires it. The presented secret is verified against `zeroship.oauth_clients.client_secret_hash` (canonical column; column-grant restricted to auth/control roles per schema-redesign §`oauth_clients`). <!-- Added in round 1: addressing SF-3 (unspecified secret-verification algorithm). --> The hash is **argon2id** (a password-grade KDF, matching `zeroship.users.password_hash`); verification uses the argon2 verifier, which is constant-time over the stored encoded hash. A legacy/HMAC-pepper variant is not used. Failed verification rejects with HTTP 401 and `WWW-Authenticate: Basic realm="zeroship"` where Basic was attempted; body `invalid_client`. Never branch on whether the client exists vs. the secret mismatched in a way that creates a timing/error oracle. Public clients are authenticated by PKCE only. RFC 6749 §2.3; RFC 8252 §8.5.
3. Look up `client_id`; ensure it is allowed to use authorization-code grant. Unknown/disabled -> `invalid_client` or `unauthorized_client` per RFC 6749 §5.2.
4. Compute `code_hash = HMAC-SHA256(auth_code_hash_key, code)` (stored as `BYTEA` per canonical `oauth_authorization_codes.code_hash`) and open a transaction. <!-- Added in round 1: addressing MF-2 (table name) + SF-7 (pin one single-use mechanism). --> The single-use claim is pinned to **one** mechanism: an atomic conditional consume — `UPDATE zeroship.oauth_authorization_codes SET consumed_at = NOW() WHERE code_hash = $1 AND consumed_at IS NULL AND expires_at > NOW() RETURNING *`. A zero row-count means not-found / expired / already-consumed and is indistinguishable to the caller. (No `SELECT … FOR UPDATE` alternative; the lock-free conditional UPDATE is the only specified path so the concurrency test asserts a single mechanism.) RFC 6749 §10.5. Maps C14.
5. If the conditional consume returned zero rows: return `invalid_grant`. To distinguish replay for audit, a follow-up read MAY observe a row whose `consumed_at IS NOT NULL`; that is an `auth_code_reuse` audit event. <!-- Added in round 1: addressing MF-4 (reuse→revoke linkage is refresh_family_id, not a token_set_id on the code). --> The canonical `oauth_authorization_codes` table has **no** `token_set_id` and `oauth_refresh_tokens` has no back-pointer to the code, so code replay itself returns `invalid_grant` and audits; it does not chase a per-code token set. The replay→family-revoke linkage lives on the refresh side: refresh-token replay revokes the whole `refresh_family_id` (see §1.3 step 5, §4.2). If a replayed code's original exchange minted a refresh family, that family is reachable for revocation by `(client_id, user_id)` on `oauth_refresh_tokens`. RFC 6749 §5.2, §10.5.
6. Re-verify the stored binding set exactly against the canonical columns of `oauth_authorization_codes`: stored `client_id == request.client_id`; stored `redirect_uri == request.redirect_uri` by byte string; stored `pkce_method == "S256"`; stored `granted_scopes` remain covered by the consent row (`oauth_grants.granted_scopes`); and the pairwise subject re-derives consistently — `derive_pairwise(pairwise_salt, oauth_authorization_codes.user_id, app_oauth_clients.sector_identifier)` must equal the persisted `app_user_identities.pairwise_sub` for `(client_id, user_id)`. <!-- Added in round 1: addressing MF-3. The code table stores user_id (not subject/pairwise_sub/sector_identifier); pairwise is RE-DERIVED at /token via JOIN to app_oauth_clients.sector_identifier and checked against app_user_identities, since there is nowhere on the code to store the projection. --> RFC 6749 §4.1.3, §10.5. Maps C1, C2, C14.
7. Validate `code_verifier`: 43-128 chars from RFC 7636 §4.1 unreserved charset; recompute `BASE64URL(SHA256(ASCII(code_verifier)))` and constant-time compare with stored `pkce_challenge` (canonical column name; old `code_challenge`). RFC 7636 §4.6. Maps C1.
8. The atomic consume in step 4 has already set `consumed_at` exactly once within the transaction; no separate "mark consumed" write and no `consumed_by_client_id`/`token_set_id` columns exist on the canonical code table. Mint tokens inside the same transaction so a rollback un-consumes the code on mint failure. Single-use holds under concurrent duplicate token requests because the conditional UPDATE row-count is the arbiter. Maps C14.
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
3. In one transaction, select the presented `zeroship.oauth_refresh_tokens` row `FOR UPDATE` by `token_hash`. <!-- Added in round 1: addressing MF-2. There is no refresh_token_families table; the family is the column oauth_refresh_tokens.refresh_family_id. Family-wide state (e.g. absolute expiry, revocation) is computed/applied over all rows sharing (refresh_family_id, client_id, user_id). --> The "family" is the set of rows sharing `(refresh_family_id, client_id, user_id)`; there is no separate families table.
4. If the token is unknown, the family is revoked (any row with that `refresh_family_id` has `revoked_at IS NOT NULL`), the token is past its `expires_at`, the token is itself revoked, or the `client_id`/`user_id` binding mismatches: return `invalid_grant`. RFC 6749 §5.2.
5. If the presented token is already rotated (`rotated_at IS NOT NULL`) or revoked (`revoked_at IS NOT NULL`), treat this as replay. <!-- Added in round 1: addressing MF-4 (family revoke keyed on refresh_family_id) + MF-2 (single table) + SF-5 (token_subject column). --> Revoke the whole family: `UPDATE zeroship.oauth_refresh_tokens SET revoked_at = NOW() WHERE refresh_family_id = $1 AND revoked_at IS NULL`; write the Bearer-arm marker `zeroship.token_revocations(client_id, token_subject, revoked_after = NOW())` (canonical column `token_subject`, old `sub`); return `invalid_grant`. OAuth 2.1 §6.1; RFC 9700 §4.14. Maps C7.
6. If active, rotate within the same transaction: mark the old row `rotated_at = NOW()`, `replaced_by_token_hash = <new hash>`; insert exactly one new active child with the same `refresh_family_id`, `client_id`, `user_id`, narrowed `granted_scopes` if requested, and `expires_at <=` the family's absolute expiry (the minimum/`issued_at + 30d` ceiling shared across the family; the family lifetime is never slid by rotation). A partial unique index `oauth_refresh_tokens_one_active_per_family` on `(refresh_family_id) WHERE rotated_at IS NULL AND revoked_at IS NULL` enforces one live token per family.
7. Mint a new access token; do not mint an `id_token` on refresh unless the original authorization code had `openid` and a stored nonce. For browser BFF, this grant is not available.

Concurrency rule: if two refresh requests race with the same active token, one commits the rotation; the loser acquires the row lock, observes `rotated_at IS NOT NULL`, and revokes the whole `refresh_family_id`. This is deliberate: the AS cannot distinguish a retry race from token theft, and RFC 9700 §4.14 chooses family revocation.

Success response: same shape as RFC 6749 §5.1 with a new `refresh_token`; always `Cache-Control: no-store`, `Pragma: no-cache`.

Errors: standard RFC 6749 §5.2. Replay, expired, revoked, wrong client, and wrong family all return `invalid_grant` with no detail that helps oracle attacks. Security logs include `refresh_family_id`, `client_id`, and `user_id`, never token values.

### 1.4 `POST /device/authorization`

Governing specs: RFC 8628 §3.1, §3.2, §5.1, §5.4, §6.1; RFC 6749 §3.3, §5.2.

Accepted parameters: `client_id`, optional `scope`.

Ordered validation and processing:

1. Require TLS except insecure-dev loopback. C11.
2. Look up the client; it must be a public CLI/device client and allowed to use the device grant. RFC 8628 §3.1.
3. Validate requested scopes against the client allowlist. If omitted, use the CLI default scope set. Unknown or disallowed scopes return `invalid_scope`. RFC 6749 §3.3.
4. Generate `device_code` with at least 256 bits of entropy. Store only `device_code_hash = HMAC-SHA256(device_code_hash_key, device_code)`. RFC 8628 §5.1. Maps C9.
5. Generate `user_code` from the non-confusable alphabet already used by the tree (`BCDFGHJKLMNPQRSTVWXZ`), grouped as `XXXX-XXXX-XXXX`, with enough entropy and an attempt/rate-limit budget. RFC 8628 §6.1. Maps C9.
6. Insert a device grant row with status `pending`, `client_id`, canonical scopes, `expires_at = now + 10 minutes`, `interval = 5`, `last_polled_at = NULL`, and no subject. Unique constraints cover `device_code_hash` and active `user_code`.

Success response (RFC 8628 §3.2):

```json
{
  "device_code": "<opaque>",
  "user_code": "BCDF-GHJK-LMNP",
  "verification_uri": "https://auth.zeroship.ai/device",
  "verification_uri_complete": "https://auth.zeroship.ai/device?user_code=BCDF-GHJK-LMNP",
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
  "userinfo_endpoint": "https://auth.zeroship.ai/userinfo",
  "device_authorization_endpoint": "https://auth.zeroship.ai/device/authorization",
  "revocation_endpoint": "https://auth.zeroship.ai/revoke",
  "end_session_endpoint": "https://auth.zeroship.ai/logout",
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

Discovery additions (round 1): `userinfo_endpoint` (MF-6) and `end_session_endpoint` (MF-7) are advertised because the Basic-OP conformance gate (parent design §5A.5) exercises UserInfo and RP-initiated logout. Also advertise `frontchannel_logout_supported: false` (the platform uses back-channel logout only) and `claims_parameter_supported: false`.

<!-- Added in round 1: addressing SF-6 (RFC 8414 metadata path). -->
RFC 8414 path: the same metadata document is also served at `GET /.well-known/oauth-authorization-server` (RFC 8414 §3) in addition to `GET /.well-known/openid-configuration` (OIDC Discovery §4). A strict RFC 8414 / Config-OP OAuth-metadata consumer probes the former; both paths return byte-identical bodies (the OIDC superset is a valid RFC 8414 document) so the two never drift.

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
3. If `token_type_hint=refresh_token` or lookup by refresh-token hash succeeds: revoke the whole family (`UPDATE zeroship.oauth_refresh_tokens SET revoked_at = NOW() WHERE refresh_family_id = $1 AND revoked_at IS NULL`) and write `zeroship.token_revocations(client_id, token_subject, revoked_after = NOW())` (canonical column `token_subject`, old `sub`). RFC 7009 §2.1. Maps C7, C15.
4. If `token_type_hint=access_token` or JWT access-token verification succeeds: verify signature, `typ: at+jwt`, `alg=EdDSA`, `iss`, `exp`, `aud`, and client binding; then write `zeroship.token_revocations(client_id, token_subject, revoked_after = NOW())`. This is stronger than single-JTI revocation and matches the existing per-app family-marker pattern. RFC 7009 §2.1; RFC 9068 §4. Maps C6, C10, C15.
5. Unknown, already revoked, or mismatched token still returns HTTP 200 with empty body. RFC 7009 §2.2. Do not reveal token existence.
6. Unsupported token type hints return RFC 7009 §2.2.1 `unsupported_token_type` only when the hint itself is unsupported; unknown token values are still 200.

Success: `200 OK`, empty JSON or empty body, `Cache-Control: no-store`.

### 1.9 Back-channel logout origination

Governing specs: OIDC Back-Channel Logout 1.0 §2.4, §2.5, §2.6; RFC 7519 §4.1.7; RFC 7515 §4.1.4; RFC 7009.

Trigger sources: end-user logout (received at the RP-initiated logout endpoint `end_session_endpoint`, §1.11), password reset, account disable, explicit grant revoke, refresh family reuse detection, app disconnect, or admin session termination. <!-- Added in round 1: addressing MF-7. §1.9 is OP-originated fan-out; the browser-facing entry point that triggers it on user logout is §1.11. -->.

Ordered behavior:

1. Resolve affected clients/RPs from active sessions/grants. For app clients, use `app_oauth_clients.client_id` and any registered `backchannel_logout_uri`.
2. Revoke local platform sessions and token families first. Browser BFF cookies are invalidated by gateway session/token revocation, not by issuing OAuth refresh. Maps C15.
3. For each RP, mint a Logout Token JWT signed with active EdDSA key. Header: `alg=EdDSA`, `kid`, `typ=logout+jwt`. Claims: `iss`, `aud=client_id`, `iat`, `jti`, `events={"http://schemas.openid.net/event/backchannel-logout":{}}`, and at least one of `sub` or `sid`. For app clients, `sub` is the pairwise subject for that app. `nonce` MUST NOT be present. OIDC BCL §2.4. Maps C10, C15.
4. POST `application/x-www-form-urlencoded` body `logout_token=<jwt>` to the RP's `backchannel_logout_uri`. OIDC BCL §2.5.
5. Treat 2xx as delivered; retry transient failures with bounded exponential backoff. Do not block local logout completion on remote delivery; record failed deliveries for ops/audit.

<!-- Added in round 1: addressing SF-8 (logout-token jti replay-cache home). -->
Logout-token `jti` replay cache: a receiver of a back-channel logout token (the gateway during migration, or the platform-internal fan-out consumer) MUST reject a `jti` it has already processed within the logout-token replay window (§5.2, 5 minutes). The replay store is `zeroship.dpop_jtis` (the canonical single-purpose jti replay table, renamed from `dpop_jti`), reused as the platform's generic "seen jti" cache; entries are keyed by `jti` and reaped after the window. There is no separate logout-jti table. This backs the `logout_token_jti_replay_ignored` test.

### 1.10 `GET /userinfo` and `POST /userinfo`

<!-- Added in round 1: addressing MF-6. The parent design §5A.5 makes a green Basic-OP + Config-OP run the cutover gate, and the Basic-OP profile exercises UserInfo; without this endpoint P0 cannot pass the gate it is built to satisfy. -->

Governing specs: OIDC Core §5.3 (UserInfo Endpoint); RFC 6750 §2.1 (Bearer presentation); RFC 9068 §4 (JWT access-token validation).

Purpose: return claims about the end-user identified by the access token's subject. Required by the OIDC Basic-OP profile that gates the Hydra cutover.

Accepted requests: `GET` (claims) or `POST` (`application/x-www-form-urlencoded`, no body params required). Authentication is the platform access token in the `Authorization: Bearer <jwt>` header only. A token in a query string is rejected (RFC 6750 §2.3; no tokens in URLs, C11).

Ordered validation and processing:

1. Extract the Bearer token from the `Authorization` header; reject query/body-borne tokens.
2. Verify the access token as an RFC 9068 JWT exactly as the resource-server arm does: signature against platform JWKS, `typ: at+jwt`, `alg=EdDSA`, matching `kid`, `iss == platform issuer`, unexpired `exp`, and the `token_revocations(client_id, token_subject)` marker check. Reject with `401` and `WWW-Authenticate: Bearer error="invalid_token"` on any failure (OIDC Core §5.3.3).
3. Require the `openid` scope in the token's `scope` claim; otherwise `403 insufficient_scope`.
4. The returned `sub` is the token's pairwise subject (`pws_...`) and MUST byte-equal the access-token `sub`; UserInfo never reveals the global `user_id`. Resolve the row via `app_user_identities` for `(client_id, pairwise_sub)`.
5. Map granted scopes to claims: `profile` → `name`, `picture`; `email` → `email` (the relay alias from `app_user_identities.relay_email` for app clients, never the real inbox), `email_verified`. Omit claims for scopes not granted (OIDC Core §5.4).
6. Respond `200` `application/json`, `Cache-Control: no-store`. The `sub` claim is always present.

```json
{
  "sub": "pws_...",
  "name": "Ada L.",
  "picture": "https://...",
  "email": "relay+abc123@mail.zeroship.ai",
  "email_verified": true
}
```

Maps C6, C10, C11 (token validation reuse), and the privacy boundary of C15. Tests: `userinfo_requires_bearer_access_token`; `userinfo_rejects_id_token_as_bearer`; `userinfo_sub_is_pairwise_and_matches_token`; `userinfo_email_is_relay_alias`.

### 1.11 `GET /logout` - RP-initiated logout (`end_session_endpoint`)

<!-- Added in round 1: addressing MF-7. This is the browser-facing entry point that C15's "logout" half needs; it fans out via §1.9 OP-originated back-channel logout. -->

Governing specs: OIDC RP-Initiated Logout 1.0 §2, §3; RFC 9700 §4.12 (303 redirects); RFC 6749 §10.12 (anti-CSRF on `state`).

Purpose: receive an end-user / RP logout request, terminate platform sessions and token families, and fan out OP-originated back-channel logout (§1.9).

Accepted parameters (OIDC RP-Initiated Logout §2): `id_token_hint` (the RP's issued ID token, strongly recommended; used to identify the subject/client and to validate `post_logout_redirect_uri`), `client_id`, `post_logout_redirect_uri`, `state`, optional `logout_hint`, `ui_locales`.

Ordered validation and processing:

1. Require TLS at the public edge. If `id_token_hint` is present, verify it against platform JWKS, issuer, and `aud=client_id`; an expired but otherwise valid hint is accepted for logout identification (the session it named may already be gone).
2. If `post_logout_redirect_uri` is supplied, it MUST exact-match a `post_logout_redirect_uris` entry registered for the resolved `client_id`; no match → render a local 400 logout-confirmation page, never redirect to an unvalidated URI (open-redirect guard, mirrors §1.1 redirect handling).
3. Terminate local state first: revoke the gateway browser session(s) for the subject (the BFF cookie is invalidated by gateway session revocation, not by OAuth refresh), revoke refresh families for the `(client_id, user_id)`, and write the `token_revocations(client_id, token_subject)` Bearer-arm marker.
4. Trigger §1.9 OP-originated back-channel logout for the affected RPs (origination is the same machinery; this endpoint is its end-user entry point).
5. If `post_logout_redirect_uri` validated, `303 See Other` to it with `state` echoed verbatim when supplied; otherwise render a local "you are signed out" page. `Cache-Control: no-store`.

Errors: invalid/unregistered `post_logout_redirect_uri` or unknown client renders a local 400 (no redirect). A bad `id_token_hint` signature is `400` with a local page. Maps C15 (logout half), C13 (303), C3 (state echo). Tests: `end_session_requires_registered_post_logout_redirect`; `end_session_revokes_session_and_fans_out_bcl`; `end_session_redirect_is_303_with_state`.

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
| `sub` | App/end-user tokens: pairwise `pws_...` from `derive_pairwise(pairwise_salt, user_id, sector_identifier)` (canonical `user_id`; `sector_identifier` from `app_oauth_clients`). Control/deploy tokens may use the platform subject only when no app sector exists and must not be forwarded to workers. |
| `aud` | Resource-server audience, **not** the requesting client. Per RFC 9068 §3, `aud` names the resource server the token is presented to, never the client that requested it (`client_id` is carried separately, below). The audience is selected by client type, deterministically: (a) **creator-app first-party tokens** — the app is its own resource server, so `aud` is the app's stable resource identifier `app:<app_id>` resolved from `app_oauth_clients` (a fixed resource URN, not the `oac_...` client id); (b) **control/deploy tokens** — `aud = "control.zeroship.ai"`. With RFC 8707 (`resource`) cut from the closed world, there is no request-time audience selector; the audience is fully determined by the client's registry classification, so an implementer can stamp it without ambiguity. |
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
| `zeroship.oauth_clients` | Extend and make authoritative | Canonical columns (schema-redesign §`oauth_clients`): `client_id`, `client_name`, `redirect_uris`, `scopes`, `skip_consent`, `client_secret_hash` (P1 OP addition; column-grant only to auth/control). Drop the stale `hydra_client_id`; no Hydra mirror writes. **P1 OP additions** this spec relies on: `token_endpoint_auth_method`, optional `backchannel_logout_uri`, `post_logout_redirect_uris TEXT[]` (for §1.11 RP-initiated logout), and the migration-only `issuer_mode` (§7). These are additive P1 OP columns, declared here the same way canonical declares `client_secret_hash`. |
| `zeroship.app_oauth_clients` | Reuse | App id to `client_id`; `sector_identifier` for pairwise subject re-derivation and relay scoping. |
| `zeroship.oauth_grants` | Extend as the consent ledger | `(user_id, client_id) -> granted_scopes` (canonical PK `(user_id, client_id)`). Canonical already adds `remember_expires_at` (P1 OP). Use it for remembered-consent reprompt skip rather than creating a parallel ledger; do **not** create `zeroship.consents`. |
| `zeroship.app_user_identities` | Reuse | Pairwise subject persistence and relay alias linkage: canonical `(client_id, user_id) -> pairwise_sub, relay_email` (old `app_client_id`/`global_user_id` renamed). |
| `zeroship.token_revocations` | Reuse | Per-app `(client_id, token_subject)` family marker for access-token/session invalidation (canonical column `token_subject`, old `sub`). |
| `zeroship.app_scope_defs` | Reuse | App-declared scope vocabulary used by `/authorize` and consent classification. |

### 3.2 New and extended table specs

<!-- Rewritten in round 1: addressing MF-2/MF-3/MF-4. These tables now mirror the canonical schema-redesign §3 exactly: one code table (`oauth_authorization_codes`), one refresh table with the family as a column (`oauth_refresh_tokens`, no families table), `user_id` not `subject`, `pkce_challenge`/`pkce_method` not `code_challenge`/`code_challenge_method`, `code_hash BYTEA`, split `requested_scopes`/`granted_scopes`, and no `token_set_id`/`sector_identifier`/`pairwise_sub` on the code (pairwise is re-derived at /token). -->

`zeroship.oauth_authorization_codes` - net-new (canonical):

| Column | Rule |
| --- | --- |
| `code_hash BYTEA NOT NULL` (PK) | HMAC-SHA256 of the opaque code. Raw code never stored. Canonical type is `BYTEA`, not `TEXT`. |
| `client_id TEXT NOT NULL` | FK to `oauth_clients(client_id)`. Re-verified at `/token`. |
| `user_id UUID NOT NULL` | FK to `users(id)`. Global platform user; the pairwise subject is re-derived from this at `/token`, not stored here. |
| `redirect_uri TEXT NOT NULL` | Exact URI from `/authorize`. Re-verified byte-for-byte at `/token`. |
| `pkce_challenge TEXT NOT NULL` | S256 challenge from `/authorize` (canonical name; old `code_challenge`). |
| `pkce_method TEXT NOT NULL` | Must be `S256` (canonical name; old `code_challenge_method`). No `plain`. |
| `nonce TEXT NULL` | Required when `openid` is in scope; copied to `id_token`. |
| `requested_scopes TEXT[] NOT NULL` | Scopes asked for at `/authorize`. |
| `granted_scopes TEXT[] NOT NULL` | Scopes consented; `/token` does not accept scope override and the consent row must still cover these. |
| `auth_time TIMESTAMPTZ NOT NULL` | End-user auth event time; copied to tokens. |
| `amr TEXT[] NOT NULL DEFAULT '{}'` | Authentication methods; copied to tokens. |
| `issued_at TIMESTAMPTZ NOT NULL DEFAULT NOW()` | Audit. |
| `expires_at TIMESTAMPTZ NOT NULL` | Must be `<= issued_at + interval '60 seconds'`. |
| `consumed_at TIMESTAMPTZ NULL` | Single-use marker; set by the atomic conditional consume (§1.2 step 4). |

PK `code_hash`; FK `client_id -> oauth_clients(client_id) ON DELETE CASCADE`; FK `user_id -> users(id) ON DELETE CASCADE`. Index `oauth_authorization_codes_expires_at_idx`. Reaping deletes expired/consumed rows. There is **no** `sector_identifier`, `pairwise_sub`, `subject`, `consumed_by_client_id`, `consent_grant_id`, or `token_set_id` column — those are deliberately absent in canonical. The `/token` binding set is exactly: `code_hash`, `client_id`, `user_id`, `redirect_uri`, `pkce_challenge`, `pkce_method`, `granted_scopes`, `nonce`, `auth_time`, `amr`, `expires_at`, `consumed_at`. Pairwise re-derivation joins `app_oauth_clients.sector_identifier` and checks `app_user_identities.pairwise_sub` (see §4.2).

`zeroship.oauth_refresh_tokens` - net-new (canonical, single table; family-by-column):

| Column | Rule |
| --- | --- |
| `token_hash BYTEA NOT NULL` (PK) | HMAC-SHA256 of raw token. Canonical type is `BYTEA`. |
| `refresh_family_id TEXT NOT NULL` | The rotation family. **This column is the family** — there is no `refresh_token_families` table. Family-wide ops act over rows sharing `(refresh_family_id, client_id, user_id)`. |
| `client_id TEXT NOT NULL` | FK to `oauth_clients(client_id)`. |
| `user_id UUID NOT NULL` | FK to `users(id)`. |
| `granted_scopes TEXT[] NOT NULL` | Refreshable scope set; narrowing-only on rotation. |
| `issued_at TIMESTAMPTZ NOT NULL DEFAULT NOW()` | Audit. |
| `expires_at TIMESTAMPTZ NOT NULL` | Per-token expiry; bounded by the family's absolute ceiling (`issued_at` of the root + 30 days, never slid). |
| `rotated_at TIMESTAMPTZ NULL` | Set when this token is rotated; a non-null value on a presented token signals replay. |
| `replaced_by_token_hash BYTEA NULL` | Successor token hash after rotation. |
| `revoked_at TIMESTAMPTZ NULL` | Per-token/family revocation marker. |
| `last_used_at TIMESTAMPTZ NULL` | Optional replay/audit. |

PK `token_hash`; FK `client_id -> oauth_clients(client_id) ON DELETE CASCADE`; FK `user_id -> users(id) ON DELETE CASCADE`; family policy index/unique on `(refresh_family_id, client_id, user_id)` per schema-redesign §`oauth_refresh_tokens`. Constraint: partial unique index `oauth_refresh_tokens_one_active_per_family` on `(refresh_family_id) WHERE rotated_at IS NULL AND revoked_at IS NULL` — one live token per family (replaces the old `WHERE status='active'`; there is no `status` column). Reuse detection (replay → whole-family revoke) keys on `refresh_family_id` (§1.3 step 5, §4.1); the absolute lifetime is the family's root-token ceiling, computed from the rows, not a separate family row.

`zeroship.oauth_grants` / consent ledger - extended:

Canonical columns (schema-redesign §`oauth_grants`): `user_id UUID`, `client_id TEXT`, `granted_scopes TEXT[]`, `granted_at`, `updated_at`, `last_used_at`, `remember_expires_at` (P1 OP addition for remembered-consent reprompt skip). PK `(user_id, client_id)`.

| Column | Rule |
| --- | --- |
| `user_id`, `client_id`, `granted_scopes` | Reused: end-user x client x granted scopes. PK `(user_id, client_id)`. |
| `granted_at`, `updated_at` | Lifecycle audit. |
| `last_used_at TIMESTAMPTZ NULL` | Touched on silent (remembered) approval. |
| `remember_expires_at TIMESTAMPTZ NULL` | Null means session-only/no remembered silent consent; non-null + future enables reprompt skip. |

Consent linkage: the authoritative relay alias remains in `app_user_identities` keyed by `(client_id, user_id)` (canonical column order). Consent accept for `email` scope mints/reuses the relay alias under the grant lock. The grant row and identity row together satisfy "user x client x scopes x relay alias". Relay-alias presence is derived from `app_user_identities.relay_email IS NOT NULL`; it is not duplicated onto `oauth_grants`. Consent revocation is recorded by deleting the grant row (control holds DELETE) and writing `token_revocations`; there is no `revoked_at` column on canonical `oauth_grants`.

<!-- Rewritten in round 1: addressing MF-1 (no private key column; file custody) + MF-5 (status enum to canonical CHECK). -->
`zeroship.signing_keys` - registry only (canonical):

| Column | Rule |
| --- | --- |
| `kid TEXT NOT NULL` (PK) | RFC 7638 JWK thumbprint. |
| `alg TEXT NOT NULL` | EdDSA for platform keys. |
| `public_jwk JSONB NOT NULL` | Public OKP Ed25519 JWK served by JWKS. **No private-key column.** |
| `status TEXT NOT NULL CHECK (status IN ('active','next','retiring'))` | Canonical CHECK domain. There is **no** `retired` or `compromised` status value (the CHECK forbids them); retirement and compromise are modeled by lifecycle + row deletion, see below. |
| `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()` | Audit. |
| `activated_at TIMESTAMPTZ NULL` | Set when promoted to `active`. |
| `retiring_at TIMESTAMPTZ NULL` | Set when moved to `retiring` (replaced by new active). The earliest-removal instant is computed as `retiring_at + overlap` (§5.2); no separate `retire_after` column. |
| `retired_at TIMESTAMPTZ NULL` | Recorded at the moment the row is removed from JWKS (the row is then deleted). |

PK `kid`; no FKs. Only one `active` key at a time; at most one `next`. `retiring` keys stay in JWKS until the overlap window closes, then the row is **deleted** (retirement is `retiring` → removed, not a `retired` status value). Compromise is an emergency rotation that promotes a fresh `active` and **deletes** the compromised key's row outright (documented in §5.5), not a status transition.

**Private-key custody (MF-1):** the raw OP private signing key is **never stored in Postgres**. `crates/auth` loads it from `AUTH_SIGNING_KEY_FILE` (PEM/PKCS#8) at boot, mirroring the gateway's `GATEWAY_SIGNING_KEY_FILE` pattern (or a KMS-backed equivalent with the same custody boundary). This table is a public-key + rotation-metadata registry only; no database role can read usable private key material. If a future multi-node design requires persisted private-key material for rotation coordination, it must be encrypted at rest by a wrapping key from the file/KMS custody path and never stored plaintext (schema-redesign §"Signing-key custody requirement").

## 4. Highest-CVE-density decisions

### 4.1 Refresh decision and CLI rotation scheme

Decision by face:

| Face | Decision |
| --- | --- |
| Browser/BFF | No OAuth refresh token. The gateway session cookie is the longevity anchor and the gateway re-mints short-lived session/access artifacts from that live session. Existing BFF `anchors.rs` pattern is retired for this face. |
| CLI/programmatic | Platform-issued opaque rotating refresh tokens with replay detection and family revocation. |

CLI refresh algorithm:

<!-- Rewritten in round 1: addressing MF-2/MF-4. Single canonical table oauth_refresh_tokens; the family is the refresh_family_id column; reuse detection keys on rotated_at/revoked_at, not a status enum. -->

1. Initial issuance occurs after successful auth-code or device-code grant with `offline_access`. Mint a fresh `refresh_family_id` (`rfam_...`, never embedded in the token value).
2. Insert the root `zeroship.oauth_refresh_tokens` row with that `refresh_family_id`, `rotated_at = NULL`, `revoked_at = NULL`, and `expires_at = NOW() + 30 days` by default. This root `expires_at` is the family's absolute ceiling; rotation never slides it.
3. On every refresh grant, run the §1.3 transaction. Successful use always rotates (old row `rotated_at = NOW()`, `replaced_by_token_hash` set) and inserts exactly one new live child in the same family with `expires_at <=` the family ceiling. The old token can never be used again.
4. Reuse detection: any presentation of a token with `rotated_at IS NOT NULL` or `revoked_at IS NOT NULL` is a replay signal. Revoke the entire `refresh_family_id` (`UPDATE ... SET revoked_at = NOW() WHERE refresh_family_id = $1 AND revoked_at IS NULL`), write `token_revocations(client_id, token_subject, revoked_after = NOW())`, and return `invalid_grant`.
5. Expiry: a token past its `expires_at` (and therefore the whole family past the ceiling) returns `invalid_grant`; a sweeper deletes expired rows. There is no `revoke_reason` column on canonical `oauth_refresh_tokens`; the reason is captured in the audit log, not the row.
6. Revocation: `/revoke`, logout (§1.11), password reset, account disable, and device/app disconnect all revoke the family (by `refresh_family_id`) and write the per-app access-token family marker `token_revocations(client_id, token_subject)`.
7. Logging: token value and token hash are secrets; logs may include `refresh_family_id`, `client_id`, `user_id`, and reason.

Tradeoff explicitly rejected for v1 unless implementation risk forces a rollback: CLI could re-run the device grant when access expires, deleting refresh-token replay risk at the cost of recurring UX friction. If chosen, drop `zeroship.oauth_refresh_tokens` from v1 and omit `offline_access`. The current decision is rotation because it meets OAuth 2.1 §6.1 / RFC 9700 §4.14 while preserving CLI usability.

### 4.2 Authorization-code store binding set

The authorization code is an opaque one-use handle to a full authorization snapshot. `/token` re-verifies exactly the canonical `oauth_authorization_codes` columns: <!-- Rewritten in round 1: addressing MF-3 (canonical columns; pairwise re-derived via JOIN, not stored on the code) + MF-4 (no token_set_id; reuse→family-revoke lives on the refresh side). -->

| Binding | Source | `/token` rule |
| --- | --- | --- |
| `code_hash` | Random code from `/authorize` | `BYTEA` HMAC hash lookup via the atomic conditional consume; raw code never stored. |
| `client_id` | Request client | Must equal token request client. |
| `user_id` | Authenticated user (canonical, not `subject`) | The global user; input to pairwise re-derivation. |
| `redirect_uri` | Exact validated redirect | Must equal token request `redirect_uri` byte-for-byte. |
| `pkce_method` | `/authorize` (canonical, not `code_challenge_method`) | Must be `S256`. |
| `pkce_challenge` | `/authorize` (canonical, not `code_challenge`) | Must equal recomputed S256 challenge from `code_verifier`. |
| `granted_scopes` | Granted scope after consent (canonical, split from `requested_scopes`) | Token request cannot expand or replace it; `oauth_grants.granted_scopes` must still cover it. |
| `nonce` | `/authorize` OIDC request | Copied to `id_token`; no override allowed. |
| `auth_time`, `amr` | Login event | Copied to tokens. |
| `expires_at` | Code issuance | Must be in future; max TTL 60s. |
| `consumed_at` | Code row | Must be null when the conditional consume runs; set atomically by it. |
| pairwise subject (derived) | `app_oauth_clients.sector_identifier` + `app_user_identities` | **Re-derived at `/token`** (not stored on the code): `derive_pairwise(pairwise_salt, user_id, sector_identifier)` must byte-equal the persisted `app_user_identities.pairwise_sub` for `(client_id, user_id)`. There is no `sector_identifier`/`pairwise_sub` column on the code table. |

Concurrency test-grade requirement: two simultaneous `/token` requests using the same code can produce at most one token response, arbitrated by the conditional-UPDATE row-count (§1.2 step 4). The loser returns `invalid_grant`. Code replay is audited as `auth_code_reuse` and returns `invalid_grant`; there is no per-code `token_set_id` to chase (canonical has none). The replay→revoke linkage that does exist is on the refresh side: refresh-token replay revokes the whole `refresh_family_id` (§1.3 step 5, §4.1). This satisfies RFC 6749 §10.5 and C14.

## 5. Key rotation / JOSE

### 5.1 Key model

<!-- Rewritten in round 1: addressing MF-1. The private key is file-based (AUTH_SIGNING_KEY_FILE), never DB-resident; there is no DB ciphertext, no AAD, and no decrypt_with_keys path for the signing key. -->

The OP has one platform issuer and one signing-key set for access tokens, ID tokens, and logout tokens. Keys are Ed25519 only. The **active private signing key is loaded from `AUTH_SIGNING_KEY_FILE`** (PEM/PKCS#8) into process memory at boot by `crates/auth`, mirroring the gateway's `GATEWAY_SIGNING_KEY_FILE`; a KMS-backed equivalent with the same custody boundary may substitute. The private key is **never written to Postgres** — there is no `private_key_enc` column, no AES-GCM ciphertext, no `core::crypto` AAD, and no `decrypt_with_keys` path for it. `zeroship.signing_keys` holds only the public JWK + rotation metadata (`kid`, `alg`, `public_jwk`, `status`, `created_at`, `activated_at`, `retiring_at`, `retired_at`); the `kid` of the file-provisioned key is computed at boot as the RFC 7638 thumbprint of its public JWK and reconciled against (or inserted into) the registry row. Key provisioning and rollover are out-of-band file/KMS operations coordinated with the registry status transitions in §5.4/§5.5.

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

<!-- Rewritten in round 1: addressing MF-1. The key is provisioned out-of-band into AUTH_SIGNING_KEY_FILE; boot LOADS it and reconciles the public-JWK registry row. It is never minted into the DB. -->

1. At boot, `crates/auth` reads `AUTH_SIGNING_KEY_FILE` (PEM/PKCS#8) into process memory and derives the public OKP JWK and its RFC 7638 `kid`. Refuse to boot if the file is missing, unreadable, or not a valid Ed25519 key.
2. Open a DB transaction and take an advisory lock `auth_signing_key_bootstrap`. Reconcile the registry: if a `zeroship.signing_keys` row for the loaded `kid` exists, ensure its `public_jwk` matches the file's public key and that exactly one row is `status='active'`; if no row exists for the loaded `kid`, insert it as `status='active'`, `activated_at=NOW()` (public JWK + metadata only — never any private bytes), and commit.
3. Publish the public JWK to JWKS. The active signing key in memory is the file key; the registry row only advertises it.
4. Refuse to boot if more than one row is `status='active'`, or if the file key's `kid` does not match the single active registry row (a mismatch means the file and the advertised JWKS would disagree). There is no "private key cannot decrypt" failure mode because nothing is decrypted from the DB.

### 5.4 Planned rotation

<!-- Rewritten in round 1: addressing MF-1 (file-provisioned key material) + MF-5 (canonical status domain; retirement = delete, no `retired` status value). -->

1. Provision a new Ed25519 key into the file/KMS custody path (the next-key file slot) out-of-band; under advisory lock, insert its public JWK as `status='next'` with its RFC 7638 `kid`. No private bytes touch the DB.
2. JWKS immediately serves `active + next + retiring`. New tokens are still signed by the `active` file key.
3. After at least one JWKS cache TTL, promote `next` to `active` (set `activated_at`) and move the old active to `status='retiring'`, set `retiring_at = NOW()`; the file/KMS active-key slot is swapped to the new key in lockstep. The earliest removal instant is `retiring_at + overlap` (§5.2), computed, not stored.
4. New tokens are signed only by the new active file key.
5. Verifiers accept keys present in JWKS. `retiring` keys stay published until `retiring_at + overlap`.
6. After `retiring_at + overlap`, set `retired_at = NOW()` for audit and **delete** the row (and decommission its file/KMS slot); it is no longer served or accepted. Retirement is the `retiring` → removed lifecycle — there is no `retired` status value (the canonical CHECK forbids it).

### 5.5 Emergency compromise

<!-- Rewritten in round 1: addressing MF-5. Compromise is modeled as emergency rotation + immediate row deletion, not a `compromised` status value (the canonical CHECK only allows active/next/retiring). -->

If a private key is suspected compromised:

1. Provision and promote a new `active` key immediately (a fresh file/KMS key + an inserted `status='active'` registry row), demoting any prior active.
2. **Delete the compromised key's registry row outright** and decommission its file/KMS slot, removing it from JWKS in the next publish (an operator-approved forced cut, not bounded by the normal overlap window). Revoke affected sessions/token families through `token_revocations`. There is no `compromised` status value — the key simply ceases to exist in the registry and JWKS.
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
| Access-token / ID-token confusion | Access tokens use `typ: at+jwt`, a **resource-server** `aud` (`app:<app_id>` or `control.zeroship.ai`, never the client — SF-1), plus `client_id`; ID tokens use `typ: JWT` and `aud=client_id` and are never accepted at resource-server Bearer arms. | RFC 9068 §2.1, §3, §4; RFC 9700 §2.3, §4.10 | `bearer_rejects_id_token_typ`; `id_token_rejects_as_access_token`; `access_token_aud_is_resource_not_client`. |
| PKCE downgrade | Reject missing `code_challenge`, `plain`, and any method other than `S256`. | RFC 7636 §4.2; OAuth 2.1 §4.1.1 | `authorize_rejects_plain_pkce`; `token_rejects_missing_code_verifier`. |
| Code substitution | `/token` rechecks client id, exact redirect URI, PKCE, consent scope, subject/pairwise derivation, and one-use state. | RFC 6749 §4.1.3, §10.5; RFC 7636 §4.6 | `token_rejects_code_for_other_client`; `token_rejects_code_for_other_redirect`. |
| Token leakage via logs/referrer/URLs | No tokens in query strings except short auth code; token responses no-store; redact token bodies on parse/log errors; Bearer only in Authorization header. | RFC 6750 §2.1, §2.3; RFC 6749 §5.1; RFC 9700 §2.6 | `token_response_has_no_store`; `token_parse_error_redacts_body`; `bearer_query_param_rejected`. Residual: client-side mishandling outside platform; docs warn CLI/SDK. |
| `alg:none` / key confusion | Platform pins EdDSA; verifiers match both `kid` and `alg`; JWKS accepts only OKP Ed25519 for platform issuer; no HS/JWKS fallback. | RFC 7515 §10.7; RFC 9068 §4 | `jwt_alg_none_rejected`; `jwt_hs256_key_confusion_rejected`; `jwt_wrong_kid_rejected`. |
| Replay / nonce substitution | `nonce` required for zeroship OIDC code requests and copied into ID token; gateway/RP validates nonce and `at_hash`; logout token `jti` replay cache. | OIDC Core §2, §3.1.2.1, §3.1.3.6, §3.1.3.7; OIDC BCL §2.6 | `id_token_nonce_mismatch_rejected`; `id_token_at_hash_mismatch_rejected`; `logout_token_jti_replay_ignored`. |
| IDOR on clients/codes | Every code/token/device row is bound to `client_id`; app/client tables are authoritative; token endpoint rechecks binding under lock. | RFC 6749 §3.1.2, §4.1.3, §10.5 | `token_rejects_code_with_different_client_id`; `device_poll_wrong_client_invalid_grant`. |
| Consent CSRF | Consent accept/deny POST uses CSRF token; POST re-runs scope classifier and never trusts rendered UI; grants updated under advisory lock. | RFC 6749 §10.12; RFC 9700 §4.7 | `consent_accept_requires_csrf`; `consent_accept_reclassifies_unknown_scope`. |
| Clickjacking / UI framing of login, consent, device-approval pages | Every OP-rendered HTML response sets `Content-Security-Policy: frame-ancestors 'none'` and `X-Frame-Options: DENY`; the OP UI is never legitimately framed (immersive same-site login, not an embedded consent iframe). | RFC 9700 §4 (consent-page framing); OWASP clickjacking | `consent_page_sets_frame_ancestors_none`; `authorize_ui_sets_x_frame_options_deny`. |
| Device-code phishing | Verification UI shows client name/scopes and requires explicit signed-in user approval; `verification_uri_complete` convenience does not auto-approve. | RFC 8628 §5.4 | `device_verification_requires_explicit_approval`; `device_complete_uri_does_not_skip_approval`. Residual: user may type code into phishing site; user education and origin `auth.zeroship.ai` remain necessary. |
| Device `user_code` brute force | Non-confusable high-entropy code; rate-limit failed attempts by IP plus signed-in user/session context, not by guessed code, so code rotation cannot reset the bound; short 10-minute expiry; failed attempts audited. | RFC 8628 §5.1, §5.2, §6.1 | `device_user_code_rate_limit`; `device_user_code_expires`; `device_code_entropy_shape`. |
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
| Auth-code binding set is exact and test-grade | §3.2 `oauth_authorization_codes`; §4.2. |
| CLI refresh reuse-detection is exact and test-grade | §1.3; §3.2 `oauth_refresh_tokens`; §4.1. |
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
| C6 | `/token` auth-code/device success, token format §2.2, and UserInfo §1.10. |
| C7 | `/token` refresh §1.3, `/revoke` §1.8, refresh design §4.1. |
| C8 | `/authorize` redirect validation step 5 and CLI/client rules in §1.4. |
| C9 | `/device/authorization` §1.4 and device polling §1.5. |
| C10 | JOSE §2.1, JWKS §1.7, token verification in `/revoke` §1.8. |
| C11 | `/authorize` step 1, device step 1, token leakage controls in §6. |
| C12 | Discovery §1.6, JOSE/token issuer rules §2, migration §7. |
| C13 | `/authorize` step 14. |
| C14 | `/authorize` step 13, `/token` auth-code steps 4-8, §4.2. |
| C15 | `/revoke` §1.8, RP-initiated logout `/logout` §1.11, and back-channel logout origination §1.9. |

## Revision log (round 1 — P1-contract reconciliation)

This round reconciles the P0 spec against the same-day canonical schema redesign (`docs/proposals/2026-06-30-auth-schema-redesign.md`). The protocol logic was sound; the data model and key-custody design were stale. Each blocking and should-fix item from `.p0-critique.md` is resolved below.

### MUST-FIX

| Item | Resolution |
| --- | --- |
| **MF-1 (CRITICAL) signing-key custody** | Removed `private_key_enc BYTEA NOT NULL` from §3.2 `signing_keys`; removed the §5.1 AES-GCM AAD / `decrypt_with_keys` path and the §5.3 "generate→encrypt→insert active" bootstrap. The private key now loads from `AUTH_SIGNING_KEY_FILE` (PEM/PKCS#8) at boot (mirrors `GATEWAY_SIGNING_KEY_FILE`); the table holds only public JWK + metadata (`kid`, `alg`, `public_jwk`, `status`, `created_at`, `activated_at`, `retiring_at`, `retired_at`). §5.3 bootstrap now *loads + reconciles* the registry row instead of minting into the DB. Matches schema-redesign §`signing_keys` + §"Signing-key custody requirement". |
| **MF-2 (CRITICAL) table names** | `auth_codes` → `oauth_authorization_codes`; `refresh_tokens` + `refresh_token_families` collapsed into a single `oauth_refresh_tokens` with a `refresh_family_id` column (no families table). Fixed §3.2 (table specs), §4.1 (CLI rotation), and §1.3 steps 3/5. All names are `zeroship.*` single schema. |
| **MF-3 (CRITICAL) auth-code binding set** | §4.2 / §1.2 step 6 / §3.2 rewritten to the canonical `oauth_authorization_codes` columns: `code_hash BYTEA`, `user_id` (not subject/pairwise_sub/sector_identifier), `client_id`, `redirect_uri`, `pkce_challenge`/`pkce_method` (not code_challenge/method), split `requested_scopes`/`granted_scopes` (not `scope`), `nonce`, `auth_time`, `amr`, expiry ≤60s, atomic one-time consume. Pairwise is **re-derived** at `/token` via JOIN to `app_oauth_clients.sector_identifier` + check against `app_user_identities.pairwise_sub`, since the code table stores neither. |
| **MF-4 (CRITICAL) reuse→revoke linkage** | Dropped `token_set_id` everywhere (it does not exist in canonical). Auth-code replay → `invalid_grant` + `auth_code_reuse` audit. The replay→family-revoke path is on the refresh side, keyed on `oauth_refresh_tokens.refresh_family_id` (revoke whole family on replay). Fixed §1.2 steps 5/8, §4.2, §1.3 step 5, and the test wiring (`auth_code_reuse_is_invalid_grant`, `refresh_reuse_revokes_family`). |
| **MF-5 (CRITICAL) signing-key status** | Status domain is now canonical `CHECK (status IN ('active','next','retiring'))` in §3.2/§5.4/§5.5. Dropped `retired`/`compromised` as status values: retirement = `retiring` → row deletion (with `retired_at` audit stamp); compromise = emergency rotation that deletes the key row outright (prose in §5.5). |
| **MF-6 (MAJOR) UserInfo endpoint** | Added §1.10 `GET/POST /userinfo` (OIDC Core §5.3): Bearer access-token auth, RFC 9068 validation reuse, `openid`-scope gate, pairwise `sub` matching the token, scope→claim mapping with relay-alias email. Advertised `userinfo_endpoint` in §1.6 discovery. Required for the Basic-OP conformance gate. |
| **MF-7 (MAJOR) RP-initiated logout** | Added §1.11 `GET /logout` (OIDC RP-Initiated Logout 1.0): `id_token_hint`, exact-match `post_logout_redirect_uri`, local session/family revocation, 303 redirect with `state`. Advertised `end_session_endpoint` in §1.6. Reconciled with §1.9 (this endpoint is the end-user entry point that triggers OP-originated back-channel logout fan-out). |

### SHOULD-FIX

| Item | Resolution |
| --- | --- |
| **SF-1 access-token `aud`** | §2.2 + threat row rewritten: `aud` names the resource server (`app:<app_id>` or `control.zeroship.ai`), never the client (`client_id` carried separately), per RFC 9068 §3. Audience is deterministically selected by client-registry classification (RFC 8707 `resource` is cut). |
| **SF-2 clickjacking/framing** | Added a framing-protection threat-table row + `Content-Security-Policy: frame-ancestors 'none'` / `X-Frame-Options: DENY` on the OP login/consent/device-approval UI (§1.1). |
| **SF-3 client-secret hash** | §1.2 step 2 pins argon2id verification of `oauth_clients.client_secret_hash`, constant-time, no oracle. |
| **SF-4 referrer leakage** | §1.1 code redirect now sets `Referrer-Policy: no-referrer` + a test. |
| **SF-5 stale `token_revocations.sub`** | Renamed to canonical `token_subject` in §1.3, §1.8, §4.1. |
| **SF-6 RFC 8414 path** | §1.6 now serves `/.well-known/oauth-authorization-server` (byte-identical body) alongside `/.well-known/openid-configuration`. |
| **SF-7 concurrency primitive** | §1.2 step 4 pins a single mechanism: the lock-free atomic conditional `UPDATE ... WHERE consumed_at IS NULL AND expires_at > NOW() RETURNING *`. Refresh one-active index re-expressed as `WHERE rotated_at IS NULL AND revoked_at IS NULL`. |
| **SF-8 logout-token `jti` replay cache** | §1.9 names the home: `zeroship.dpop_jtis` reused as the generic seen-jti cache, reaped after the 5-minute window. |
| **SF-9 `pairwise_salt` provenance** | §1.1 step 12 / §2.2 pin the salt: derived via `core::auth::derive_pairwise_salt` from a global secret in `AUTH_PAIRWISE_SALT_FILE` (or KMS), global (not per-sector); per-app uniqueness comes from `sector_identifier`. |

### Preserved (strong parts left intact)

- The 15 §5A → endpoint-step mappings (§0, §8) and the §8 coverage table (updated only for the two new endpoints).
- The threat-model table structure and RFC citations (added two rows, retuned two tests).
- Dual-issuer migration §7 with the verified code sites and the cookie arm correctly excluded (`typ: zeroship-sess+jwt`).
- EdDSA `at_hash` (SHA-512 leftmost-256), pairwise `sub`, and S256 PKCE primitives (§2.1/§2.3).
