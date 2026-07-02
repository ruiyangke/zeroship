# OAuth 2.0 / OIDC Standards-Conformance Map

**Audience:** the team building zeroship's own OpenID Provider (OP) / Authorization Server to replace ORY Hydra.
**Scope:** a CLOSED-WORLD, first-party deployment. Goal stated by the team: "follow the standard + industry best practices."
**Status:** reference map (proposal). RFC + section citations verified against IETF Datatracker / RFC Editor / OpenID Foundation, not memory.

The OP must support:
- Authorization Code flow + PKCE (programmatic / CLI redirect clients)
- Device Authorization Grant (CLI `zeroship login`)
- JWT access tokens
- OIDC `id_token`s
- JWKS + discovery
- a browser BFF session cookie (platform mints its own session)
- token revocation / logout

The OP does NOT need (closed-world cuts): implicit/hybrid/ROPC grants, dynamic client registration, third-party clients, FAPI.

---

## 1. The governing specs list

Each row: the spec, what it governs, and the load-bearing sections.

| Component | Spec | Governs | Key sections |
|---|---|---|---|
| OAuth 2.0 core | **RFC 6749** | Authorization Code grant, token endpoint, client types, `state`, error responses | §4.1 (auth code), §4.1.1 (auth request), §4.1.3 (token request), §3.1.2 (redirect URI), §3.3 (scope), §5.1/5.2 (token / error response), §10.12 (CSRF → `state`) |
| Bearer tokens | **RFC 6750** | How access tokens are presented + error semantics | §2.1 (`Authorization: Bearer`), §2.3 (URI query — NOT RECOMMENDED), §3 (`WWW-Authenticate`), §3.1 (`invalid_request`/`invalid_token`/`insufficient_scope`) |
| PKCE | **RFC 7636** | Code-interception defense for the auth-code flow | §4.1 (`code_verifier`: 43–128 chars, unreserved set, high entropy), §4.2 (`S256` MTI; client MUST use S256 if capable; `code_challenge=BASE64URL(SHA256(verifier))`), §4.3 (request), §4.4–4.6 (server stores + verifies) |
| Device grant | **RFC 8628** | `zeroship login` CLI flow | §3.1 (device authz request), §3.2 (response: `device_code`,`user_code`,`verification_uri`,`verification_uri_complete`,`expires_in`,`interval`), §3.4 (polling at `interval`), §3.5 (`authorization_pending`, `slow_down` → interval +5s), §5 (security), §6.1 (`user_code` entropy / non-confusable charset e.g. `BCDFGHJKLMNPQRSTVWXZ`) |
| JWT | **RFC 7519** | JWT structure + the registered claims | §4.1.1 `iss`, §4.1.2 `sub`, §4.1.3 `aud`, §4.1.4 `exp`, §4.1.6 `iat`, §4.1.7 `jti`, §7.2 (validation) |
| JOSE: signatures | **RFC 7515 (JWS)** | Signing/verifying the JWT; the `alg`/`kid` headers | §4.1.1 (`alg`), §4.1.4 (`kid`), §10.7 (reject `alg:none` unless explicitly unsecured) |
| JOSE: keys | **RFC 7517 (JWK)** | JWKS document at `jwks_uri`; key rotation | §4 (JWK params: `kty`,`use`,`kid`,`alg`), §5 (JWK Set) |
| JOSE: algorithms | **RFC 7518 (JWA)** | Concrete algs (RS256/ES256/PS256), key reqs | §3.1 (`alg` values), §3.3 (RSA ≥ 2048-bit) |
| JWT access tokens | **RFC 9068** | The profile that makes JWT access tokens interoperable | §2.1 (`typ: at+jwt` — SHOULD), §2.2 (REQUIRED claims `iss exp aud sub client_id iat jti`; `sub`=client id for client-only flows), §3 (`scope`), §4 (RS validation incl. `typ` check, reject `alg:none`, match `aud`+`iss`) |
| Introspection | **RFC 7662** | Opaque/JWT token state lookup (first-party RS) | §2.1 (request, `token_type_hint`), §2.2 (`active` boolean response), §2.3 (must auth the caller / protect endpoint) |
| Revocation | **RFC 7009** | `/revoke` for refresh + access tokens, logout | §2.1 (request, `token_type_hint`), §2.2 (HTTP 200 even for unknown token), §2.2.1 (`unsupported_token_type`) |
| AS Metadata | **RFC 8414** | `/.well-known/oauth-authorization-server` discovery | §2 (fields: `issuer`,`authorization_endpoint`,`token_endpoint`,`jwks_uri`,`response_types_supported`,`grant_types_supported`,`code_challenge_methods_supported`,`revocation_endpoint`,`introspection_endpoint`), §3 (well-known path), §3.3 (`issuer` must match exactly) |
| OIDC Discovery | **OpenID Connect Discovery 1.0** | `/.well-known/openid-configuration` | §3 (provider metadata: `subject_types_supported`,`id_token_signing_alg_values_supported`,`userinfo_endpoint`, etc.), §4 (obtaining config) |
| OIDC Core | **OpenID Connect Core 1.0** | `id_token`, UserInfo, pairwise subjects | §2 (ID Token required claims + `nonce`/`at_hash`), §3.1.2.1 (`nonce`), §3.1.3.6 (`at_hash`), §3.1.3.7 (ID Token validation, 13 steps), §5.3 (UserInfo), §8.1 (subject types public/pairwise), §8.2 (pairwise alg / `sector_identifier_uri`) |
| Back-Channel Logout | **OpenID Connect Back-Channel Logout 1.0** | Server-to-server logout to RPs | §2.4 (Logout Token), §2.5 (push to `backchannel_logout_uri`), §2.6 (Logout Token validation; `events` claim, `sid`/`sub`, `nonce` MUST NOT be present) |
| Native apps | **RFC 8252** (BCP 212) | CRITICAL for the CLI redirect client | §4/§8.12 (external/system browser, MUST NOT use embedded webview), §6 (auth code + PKCE MUST), §7.1 (private-use scheme), §7.3 (loopback `http://127.0.0.1:{port}` / `[::1]`), §8.4–8.5 (native = public client, no static secret) |
| Issuer ID (mix-up) | **RFC 9207** | `iss` authorization-response parameter | §2 (AS MUST return `iss`), §2.4 (client MUST validate `iss` and reject mismatch), §3 (`authorization_response_iss_parameter_supported` metadata) |
| Security BCP | **RFC 9700** (BCP 240) | The industry best-practice checklist (was draft-ietf-oauth-security-topics) | see §3 below |
| OAuth 2.1 | **draft-ietf-oauth-v2-1** | The modern consolidated baseline | see §2 below |
| DPoP (optional) | **RFC 9449** | Sender-constrained (proof-of-possession) tokens | Note only — see below |
| Resource Indicators (optional) | **RFC 8707** | `resource` param → token `aud` | Note only — see below |

**DPoP — RFC 9449:** binds an access (and refresh) token to a client-held key via a `DPoP` proof JWT, so a stolen bearer token is unusable. RFC 9700 wants public-client refresh tokens *either* rotated *or* sender-constrained; rotation satisfies that, so DPoP is **OPTIONAL** for this closed world. Adopt later if access tokens ever leave the first-party boundary. (RFC 9449 §4 proof JWT, §5 `Authorization: DPoP`, §6 `jkt` confirmation.)

**Resource Indicators — RFC 8707:** lets the client name the target API via a `resource` parameter so the AS can stamp a correct, narrow `aud`. **Recommended-but-optional** here: with a single first-party API you can hardcode the audience; adopt RFC 8707 the moment there is >1 resource server, to keep tokens audience-restricted per RFC 9700 §2.3. (RFC 8707 §2 request param.)

---

## 2. OAuth 2.1 — the modern baseline (draft-ietf-oauth-v2-1)

OAuth 2.1 is a consolidation of OAuth 2.0 + the security extensions that became consensus; it removes the insecure parts of RFC 6749. **Build to 2.1 — it is the right default for a greenfield OP.** What it MANDATES beyond 2.0:

1. **PKCE for ALL authorization-code clients** (not just public). draft §4.1.1: clients MUST send `code_challenge`/`code_verifier` and the AS MUST enforce them. This generalizes RFC 7636 (which only *required* PKCE for public clients).
2. **Exact redirect-URI string matching.** draft §2.3.1 / §4.1.1: the AS MUST reject any `redirect_uri` that does not exactly match a registered one (RFC 3986 §6.2.1 Simple String Comparison) — **no wildcards, no substring, no path-suffix matching** — with the only carve-out being loopback port numbers for native apps.
3. **Implicit grant removed.** `response_type=token` is gone (draft §10.1 removes features found insecure in RFC 9700).
4. **Resource Owner Password Credentials (ROPC) removed.** Only three grants survive: authorization code, client credentials, refresh token.
5. **Refresh tokens for public clients MUST be sender-constrained OR one-time-use (rotation with reuse detection).** draft §4.3 / §6.1.
6. **Bearer tokens MUST NOT be passed in the query string.** Use the `Authorization` header (draft §5 / §7.2.6; Referer/log leakage).

**zeroship mapping:** the CLI redirect client and device client are public → PKCE everywhere, rotated refresh tokens, exact-match redirects (loopback for `zeroship login`). The browser BFF holds the OAuth client secret server-side (confidential), still uses PKCE under 2.1.

---

## 3. The Security BCP — RFC 9700 (BCP 240)

This is THE industry best-practice doc (formerly `draft-ietf-oauth-security-topics`). It is the checklist the build must satisfy. Attacks → REQUIRED mitigations:

| # | Attack | Mitigation (normative) | RFC 9700 § |
|---|---|---|---|
| A1 | **Authorization code injection / interception** | Public clients **MUST** use PKCE; confidential clients **RECOMMENDED** to use PKCE | §2.1.1 |
| A2 | **CSRF on the redirect** | Use PKCE; or `state` bound to the user-agent session if PKCE not applicable to the leg | §2.1, §4.7 |
| A3 | **Mix-up (multi-AS / IdP confusion)** | Client **MUST** identify which AS responded — use the `iss` response parameter (RFC 9207); distinct redirect URI per AS | §2.1, §4.4 / §4.4.2.1 |
| A4 | **Open redirect** | AS **MUST** use exact redirect-URI matching (except loopback ports); **MUST NOT** expose open redirectors | §2.1 |
| A5 | **307 redirect leaks credentials** | After auth, redirect with **303 See Other** so the browser drops the POST body (never 307/302-with-body) | §2.1, §4.12 |
| A6 | **Refresh-token replay / theft** | Public-client refresh tokens **MUST** be sender-constrained **or** use rotation; detect reuse of a rotated token → revoke the whole chain | §2.2.2, §4.14 |
| A7 | **Access token = ID token confusion** | Audience-restrict access tokens; type them (`at+jwt` per RFC 9068); RS validates `aud` + `typ`; never accept an `id_token` as an access token | §2.3, §4.10 / §4.10.2 |
| A8 | **Token leakage in transit** | TLS everywhere; authorization responses **MUST NOT** travel over unencrypted connections; end-to-end TLS (BCP 195) to the RS | §2.6 |
| A9 | **Implicit grant token leakage** | **SHOULD NOT** use implicit (`response_type=token`) — use code+PKCE | §2.1.2 |
| A10 | **ROPC password exposure** | ROPC **MUST NOT** be used | §2.4 |
| A11 | **Bearer token replay (general)** | Prefer sender-constrained tokens (mTLS / DPoP) where feasible; otherwise short-lived + audience-restricted | §2.2, §4.10 |

All of A1–A8 are directly in scope and **non-negotiable** for zeroship. A9/A10 are satisfied by simply not implementing those grants (closed-world cut, see §5).

### 3b. OpenID Provider certification (proving conformance)

The **OpenID Foundation Conformance Suite** (`openid.net/certification`, the public `conformance-suite` test harness) is how you *prove* OIDC/OAuth conformance rather than asserting it. Relevant profiles for this OP: **Basic OP** and **Config OP** (discovery), plus the OAuth 2.0 / FAPI profiles. Even without paying for the published "Certified" mark, the team **SHOULD run the self-certification suite locally** against the OP as a regression gate — it exercises discovery, JWKS, ID-token validation, nonce/state handling, and the error paths far more thoroughly than hand-written tests. Treat a green Basic-OP + Config-OP run as the acceptance bar for the Hydra cutover.

---

## 4. CONFORMANCE CHECKLIST (the build is tested against this)

Flat MUST-do list, grouped by endpoint, each tagged with its spec §. Treat unchecked = not shippable.

### `/authorize` (Authorization Endpoint)
- [ ] Accept `response_type=code` only; reject `token`/hybrid. — RFC 6749 §4.1.1; OAuth 2.1 §10.1
- [ ] **Require** `code_challenge` + `code_challenge_method=S256`; reject `plain` and reject missing PKCE. — RFC 7636 §4.2; OAuth 2.1 §4.1.1
- [ ] `redirect_uri` matched by **exact string comparison** against registered values (loopback port exception only). — OAuth 2.1 §2.3.1; RFC 9700 §2.1
- [ ] Bind a CSRF token to the browser session (`state` echoed verbatim) even though PKCE is the primary defense. — RFC 6749 §10.12; RFC 9700 §2.1
- [ ] Return the **`iss`** parameter in every authorization response. — RFC 9207 §2
- [ ] Validate `scope`; OIDC requests **MUST** include `openid` to get an `id_token`. — OIDC Core §3.1.2.1
- [ ] Carry `nonce` from request through to the issued `id_token`. — OIDC Core §3.1.2.1, §2
- [ ] Redirect back with **303 See Other** (not 307). — RFC 9700 §4.12
- [ ] Authorization codes: single-use, short TTL (≤ ~60s recommended), bound to `client_id` + `redirect_uri` + PKCE challenge; reject on reuse. — RFC 6749 §4.1.2, §10.5

### `/token` (Token Endpoint)
- [ ] Support `grant_type=authorization_code`, `refresh_token`, `urn:ietf:params:oauth:grant-type:device_code`. — RFC 6749 §4.1.3; RFC 8628 §3.4
- [ ] Verify PKCE: recompute `BASE64URL(SHA256(code_verifier))`, compare to stored challenge, reject mismatch. — RFC 7636 §4.6
- [ ] Re-check `redirect_uri` equals the one from `/authorize`. — RFC 6749 §4.1.3
- [ ] Authenticate confidential clients (BFF); do **not** require a secret from public clients (CLI). — RFC 6749 §2.3; RFC 8252 §8.5
- [ ] **Refresh-token rotation**: issue a new RT on each use, invalidate the old one, and on reuse of a retired RT revoke the entire token family. — OAuth 2.1 §6.1; RFC 9700 §4.14
- [ ] Response is `Cache-Control: no-store`, `Pragma: no-cache`, `application/json`. — RFC 6749 §5.1
- [ ] Error responses use the standard `error` codes (`invalid_grant`, `invalid_request`, `invalid_client`, …). — RFC 6749 §5.2

### Device endpoints (`/device_authorization` + `/token`)
- [ ] `/device_authorization` returns `device_code`, `user_code`, `verification_uri`, `verification_uri_complete`, `expires_in`, `interval`. — RFC 8628 §3.2
- [ ] `user_code` from a non-confusable, high-entropy charset (e.g. `BCDFGHJKLMNPQRSTVWXZ`), shown grouped/dashed. — RFC 8628 §6.1
- [ ] `device_code` is high-entropy and unguessable. — RFC 8628 §5.1
- [ ] Token polling: return `authorization_pending` until approved; return `slow_down` to force the client to **add 5s** to its interval. — RFC 8628 §3.5
- [ ] Enforce the minimum `interval`; reject too-fast polling. — RFC 8628 §3.4
- [ ] Expire `device_code`/`user_code` at `expires_in`; then return `expired_token`. — RFC 8628 §3.5
- [ ] Require explicit user approval at the verification URI before issuing tokens. — RFC 8628 §3.3, §5.4

### `id_token` (OIDC)
- [ ] Required claims: `iss`, `sub`, `aud` (= client_id), `exp`, `iat`. — OIDC Core §2
- [ ] Include `nonce` when present in the request; client must be able to verify it. — OIDC Core §2, §3.1.3.7 step 11
- [ ] Include `at_hash` when an access token is also issued. — OIDC Core §3.1.3.6
- [ ] Signed as JWS with `kid` in header; **never `alg:none`**; RS256/ES256/PS256. — RFC 7515 §4.1.4, §10.7; RFC 7518 §3.1
- [ ] `sub` is stable per (subject, sector) — **pairwise** for per-app isolation. — OIDC Core §8.1
- [ ] Honor the 13-step validation contract clients are expected to run. — OIDC Core §3.1.3.7

### JWT access tokens
- [ ] Header `typ: at+jwt`. — RFC 9068 §2.1
- [ ] Required claims: `iss`, `exp`, `aud`, `sub`, `client_id`, `iat`, `jti`. — RFC 9068 §2.2
- [ ] `aud` names the target resource (audience-restricted). — RFC 9068 §3; RFC 9700 §2.3
- [ ] `scope` claim conveys granted scopes. — RFC 9068 §2.2.3
- [ ] Signed (not `none`); short-lived. — RFC 9068 §4; RFC 9700 §4.10

### JWKS + discovery
- [ ] `jwks_uri` serves the public JWK Set; each key has `kid`, `use:sig`, `alg`, `kty`. — RFC 7517 §4, §5
- [ ] Support key rotation: publish new key before signing with it; keep retired keys until all tokens expire. — RFC 7517 §5 (operational)
- [ ] `/.well-known/oauth-authorization-server` with `issuer`, `authorization_endpoint`, `token_endpoint`, `jwks_uri`, `response_types_supported`, `grant_types_supported`, `code_challenge_methods_supported`, `revocation_endpoint`. — RFC 8414 §2, §3
- [ ] `/.well-known/openid-configuration` with `subject_types_supported` (incl. `pairwise`), `id_token_signing_alg_values_supported`, `userinfo_endpoint`, `device_authorization_endpoint`, `end_session`/`backchannel_logout_supported`. — OIDC Discovery §3
- [ ] `issuer` is an `https` URL with no query/fragment and **exactly matches** the value used to fetch the metadata, the `iss` in tokens, and the RFC 9207 `iss` param. — RFC 8414 §2, §3.3; RFC 9207 §2
- [ ] Advertise `authorization_response_iss_parameter_supported: true`. — RFC 9207 §3

### Revocation / introspection / logout
- [ ] `/revoke` accepts `token` + optional `token_type_hint`; returns **HTTP 200 even for an unknown/already-invalid token**. — RFC 7009 §2.1, §2.2
- [ ] Revoking a refresh token revokes derived access tokens (and rotation family). — RFC 7009 §2.1
- [ ] `/introspect` (first-party RS only) authenticates the caller and returns `active:true/false`. — RFC 7662 §2.1, §2.3
- [ ] Back-Channel Logout: emit a signed **Logout Token** with the `events` claim, `sub`/`sid`, `iss`, `aud`, `iat`, `jti`, and **no `nonce`**; POST to each RP's `backchannel_logout_uri`. — OIDC BCL §2.4, §2.5
- [ ] BFF session + platform session invalidated on logout, alongside token revocation. — (zeroship-specific, see §5)

### Transport / global
- [ ] TLS on every endpoint; reject plaintext. — RFC 9700 §2.6; RFC 6749 §1.6
- [ ] Bearer tokens only in `Authorization: Bearer`; never query string. — RFC 6750 §2.1; OAuth 2.1 §7.2.6
- [ ] Reject `alg:none` and unexpected `alg` everywhere a JWT is verified. — RFC 7515 §10.7; RFC 9068 §4

---

## 5. Closed-world deviations: spec-PERMISSIBLE vs NON-NEGOTIABLE

### MAY cut (the closed world legitimately permits these)
- **Omit implicit / hybrid / ROPC grants.** Not just allowed — *required* to omit by OAuth 2.1 §10.1 and RFC 9700 §2.1.2/§2.4. Cutting them is the correct direction.
- **Omit Dynamic Client Registration (RFC 7591/7592).** First-party clients are provisioned out-of-band; DCR exists for open ecosystems. Permissible to skip.
- **Omit/scope-down Introspection (RFC 7662)** to first-party resource servers only — or skip it entirely if every RS validates JWT access tokens locally via JWKS. Introspection is optional in RFC 8414 §2.
- **Omit FAPI** (financial-grade profile). It is a higher bar for high-risk third-party ecosystems; not required for a closed first-party OP.
- **Omit DPoP / mTLS sender-constraining (RFC 9449 / RFC 8705)** — *provided* refresh-token rotation + reuse detection is implemented (that satisfies RFC 9700 §2.2.2 for public clients).
- **Hardcode the audience / skip Resource Indicators (RFC 8707)** while there is exactly one resource server. Revisit when a second API appears.
- **Use `public` instead of `pairwise` subjects** is *technically* spec-permissible — but the team explicitly does per-app pairwise, so keep pairwise (it is a privacy win, not a requirement to drop).
- **Custom session cookie / BFF session** alongside OAuth tokens — out of band of the OAuth specs, allowed, *as long as* it does not weaken the OAuth invariants (CSRF protection, logout propagation).

### MUST NOT cut (non-negotiable — cutting these breaks conformance/security)
- **PKCE (S256) on the authorization-code flow.** RFC 7636 §4.2; OAuth 2.1 §4.1.1; RFC 9700 §2.1.1. Required for the CLI public client by RFC 8252 §6.
- **Exact redirect-URI matching** (loopback-port exception only). OAuth 2.1 §2.3.1; RFC 9700 §2.1.
- **CSRF protection** via PKCE and/or `state`. RFC 6749 §10.12; RFC 9700 §2.1.
- **`nonce`** flow-through and validation for OIDC `id_token`s. OIDC Core §2, §3.1.3.7.
- **The `iss` authorization-response parameter** (mix-up defense). RFC 9207 §2; RFC 9700 §4.4.
- **Audience + `typ` on access tokens** (`at+jwt`, correct `aud`). RFC 9068 §2; RFC 9700 §2.3 — prevents access-token/id-token confusion.
- **Refresh-token rotation with reuse detection** for public clients. OAuth 2.1 §6.1; RFC 9700 §4.14.
- **System browser (not embedded webview) + loopback redirect + PKCE** for the CLI. RFC 8252 §4/§8.12, §7.3, §6.
- **Device-grant polling discipline** (`interval`, `slow_down` +5s, `authorization_pending`, expiry, explicit user approval). RFC 8628 §3.4–3.5, §5.
- **Reject `alg:none`** and signature-type confusion everywhere a JWT is verified. RFC 7515 §10.7.
- **TLS on all endpoints**; no bearer tokens in URLs. RFC 9700 §2.6; RFC 6750 §2.1.
- **`issuer` consistency** across discovery, token `iss`, and the RFC 9207 `iss` param (single exact `https` URL). RFC 8414 §3.3.
- **303 (not 307) on the post-authentication redirect.** RFC 9700 §4.12.
- **Single-use, short-lived authorization codes** bound to client + redirect + PKCE. RFC 6749 §10.5.
- **Back-channel logout propagation** + local session/token revocation on logout. OIDC BCL §2.4–2.6; RFC 7009.

---

## Sources

- RFC 6749 — OAuth 2.0 Authorization Framework: https://www.rfc-editor.org/rfc/rfc6749
- RFC 6750 — Bearer Token Usage: https://www.rfc-editor.org/rfc/rfc6750
- RFC 7636 — PKCE: https://www.rfc-editor.org/rfc/rfc7636
- RFC 8628 — Device Authorization Grant: https://www.rfc-editor.org/rfc/rfc8628
- RFC 7519 — JWT: https://www.rfc-editor.org/rfc/rfc7519
- RFC 7515 — JWS: https://www.rfc-editor.org/rfc/rfc7515
- RFC 7517 — JWK: https://www.rfc-editor.org/rfc/rfc7517
- RFC 7518 — JWA: https://www.rfc-editor.org/rfc/rfc7518
- RFC 9068 — JWT Profile for OAuth 2.0 Access Tokens: https://www.rfc-editor.org/rfc/rfc9068
- RFC 7662 — Token Introspection: https://www.rfc-editor.org/rfc/rfc7662
- RFC 7009 — Token Revocation: https://www.rfc-editor.org/rfc/rfc7009
- RFC 8414 — Authorization Server Metadata: https://www.rfc-editor.org/rfc/rfc8414
- RFC 8252 (BCP 212) — OAuth 2.0 for Native Apps: https://www.rfc-editor.org/rfc/rfc8252
- RFC 9207 — Authorization Server Issuer Identification: https://www.rfc-editor.org/rfc/rfc9207
- RFC 9700 (BCP 240) — OAuth 2.0 Security Best Current Practice: https://www.rfc-editor.org/rfc/rfc9700
- RFC 9449 — DPoP: https://www.rfc-editor.org/rfc/rfc9449
- RFC 8707 — Resource Indicators: https://www.rfc-editor.org/rfc/rfc8707
- draft-ietf-oauth-v2-1 — OAuth 2.1: https://datatracker.ietf.org/doc/draft-ietf-oauth-v2-1/
- OpenID Connect Core 1.0: https://openid.net/specs/openid-connect-core-1_0.html
- OpenID Connect Discovery 1.0: https://openid.net/specs/openid-connect-discovery-1_0.html
- OpenID Connect Back-Channel Logout 1.0: https://openid.net/specs/openid-connect-backchannel-1_0.html
- OpenID Foundation Conformance / Certification: https://openid.net/certification/
