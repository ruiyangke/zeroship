# Self-contained platform Auth Server — replace Hydra entirely

**Status:** proposal (active, not shipped) — design + critic/reviser review before any code.
**Date:** 2026-06-30
**Companion ADR:** `docs/decisions/2026-06-30-self-contained-auth-replace-hydra.md` (write with this).
**Supersedes:** the broker-framed analysis in `2026-06-30-decoupled-end-user-oauth.md` (that doc's value was forcing the honest AS-sizing; this doc is the **decided** architecture). **Reframes:** the pluggable-provider epic (`feat/auth-providers`, 11 commits) — Hydra and Supabase stop being the platform AS; see §8 for what survives, repurposes, and is superseded.

> **The decision (operator):** Hydra is too heavy for a closed-world, first-party auth surface. Build the platform's own OAuth2/OIDC Authorization Server **inside `crates/auth`** and **remove Hydra entirely** (both its end-user-OAuth role and its deploy/control token-kernel role). Pre-launch, no back-compat — rip, don't shim.

> **Honest scope statement (round 1).** This is **not** "add `/authorize`+`/token` to an existing IdP." Today `crates/auth` is a **Hydra login/consent _front-end_**: it owns credential *verification* and the user store, but the login/consent/logout *orchestration* is Hydra's (every login completion calls `accept_login` **back into Hydra**; `consent.rs` is a Hydra consent-challenge handler). Becoming self-contained means **rewriting login/consent/logout to stop delegating to Hydra and instead mint platform sessions, auth codes, id_tokens, access/refresh tokens, and originate back-channel logout** — i.e. building a **near-complete OpenID Provider (OP)**, plus re-homing the gateway's full Hydra OIDC *relying-party* stack (~2,700 LoC across `oidc_rp.rs`/`anchors.rs`/`backchannel_logout.rs`) onto platform tokens. The closed-world subset (§4) makes the OP *smaller and auditable*; it does not make it *small*. The build-vs-rent calculus is re-examined honestly in §11.

---

## 1. Decision & scope (locked)

<!-- Rewritten in round 1: addressing CRITICAL #1 — "crates/auth already is the IdP" overstatement -->

1. **`crates/auth` becomes the self-contained platform AS + OP.** Today it is the **credential-verification + user-store half** of the IdP, *driven by Hydra's login/consent challenges* (`login.rs` reads `login_challenge`, calls `admin.get_login`, and finishes by calling `accept_login` back to Hydra — `login.rs:5-8,52-119,335-391`; `consent.rs` is a Hydra consent-challenge handler — `consent.rs:59-317`). The work is to **rewrite the login/consent/logout completion path** so it mints platform artifacts directly, and to **add the OAuth2/OIDC protocol layer Hydra provided** (codes, tokens, id_tokens, JWKS, discovery). That is a near-complete OP build, not a bolt-on.
2. **Identity is self-contained.** `zeroship.users` + password / magic-link / TOTP / social is the primary identity source. **Supabase / Google / GitHub become optional *upstream social* logins (federation), not the AS.**
3. **Two app-facing faces** (operator decision):
   - **Browser / creators → BFF/cookie:** the gateway-signed session cookie + `ZeroShip-User` header (already built in the gateway). Used by the creator dashboard and browser end-user sessions.
   - **CLI / programmatic → OAuth2:** the **device authorization grant** (`zeroship login`) + a **closed-world `/authorize`+`/token`** for programmatic clients. Platform-issued tokens, platform JWKS.
4. **Closed-world AS** (§4): only the grants/flows zeroship itself uses; clients are platform-provisioned; no third-party OAuth clients, no dynamic registration. The closed-world invariants (§7) are load-bearing and have a tripwire.
5. **Remove Hydra entirely** — the sidecar, `HydraAdmin`, the login/consent challenge delegation, Hydra introspection, the `oauth_hydra` schema/role, the ory configs, **the entire `crates/auth/src/hydra_client/` module** (9 files; enumerated at §6.1(8)/P6), **and** the gateway's Hydra *relying-party* stack (§6).

---

## 2. What's already built vs. what is genuinely net-new (accurate reuse)

<!-- Rewritten in round 1: addressing CRITICAL #1 + MAJOR #11 (signing reuse) + the §2 overstatement audit -->

Grounded in the tree as of this branch. The earlier draft over-claimed reuse in three places; corrected here.

| Capability the AS needs | Reuse reality | Where (verified) |
| --- | --- | --- |
| **User store + credentials** (password, magic-link, TOTP, sessions) | ✅ **own it** — genuine reuse | `crates/auth` (`identity/{credentials,password,magic_link,totp,verification}.rs`, `store/users.rs`, `sessions/`) |
| **Login/consent/logout _orchestration_** | ❌ **NOT reuse — rewrite.** Today every login completion calls `accept_login` **back to Hydra**; `consent.rs` is a Hydra consent-challenge handler; even the TOTP attestation is "bound to … this hydra challenge". | `login.rs:52-119,291-301,335-391`; `consent.rs:59-317,597` |
| **Social/upstream federation seam** | ✅ pattern exists | `auth/src/ui/oauth_{google,github}.rs`, `identity/oauth/` |
| **Token _verification_ + JWKS _consume_ + provider seam** | ✅ cheap to extend — `AuthProvider` is a 2-variant enum `{Hydra, Supabase}`; adding a `Platform` JWKS arm is a low-invasive change | `crates/core/src/auth_provider/mod.rs:20-40`, `core/oidc_verify.rs` (alg-pinned JWKS cache) |
| **Device-grant _state machine_** (RFC 8628: device_code/user_code/poll/`slow_down`/expiry) | ⚠️ **only the polling SM + the UI template/form skeleton reuse** — see §3; *token issuance + approver-authn + the user-code-confirm logic are superseded*. The `auth/ui/device.rs` page is itself Hydra+Supabase-coupled (`HydraAdmin`/`AcceptDeviceUserCodeRequest` `:18-19`, `render_supabase_form` `:51,72`); only the askama template + CSRF form skeleton survives. | `control/src/device_handlers.rs:290-343` (poll/`slow_down`); `auth/ui/device.rs:18-19,51,56,72` |
| **Asymmetric Ed25519 JWT _mint_** | ⚠️ **exists, but in a _different crate_, and `crates/auth` has zero JWT-mint today.** The actual mint is `gateway/src/session_token.rs::Issuer::issue` (`session_token.rs:166`, `EncodingKey::from_ed_der` at `:197`). `gateway/src/signing.rs` (255 LoC) is **only a key-loader + RFC-7638 thumbprint — it does not mint JWTs.** `crates/auth` mints only opaque random tokens today (`identity/verification.rs`, `magic_link.rs`). → cross-crate **move/rebuild**, not in-place reuse. | `session_token.rs:166,197`; `signing.rs:1-75` |
| **Per-app client registry (data model)** | ✅ data model exists (today a Hydra *mirror*) | `control.oauth_clients` / `app_oauth_clients` |
| **Pairwise per-app subject** | ✅ | `core/auth::derive_pairwise` (written by the gateway at lazy-mint) |
| **App-declared scope vocabulary + relay-alias-at-consent** | ✅ **exists and must be preserved** (see CRITICAL #2 / §4.2) | `consent.rs::classify_and_authorize`, `load_app_scope_defs`; `store/relay.rs::mint_alias_at_consent:82` |
| **Own-ecosystem email** (verify/magic/recovery) | ✅ built this session | `auth/src/ui/gotrue_email_hook.rs` + `crates/mailer` (provider-agnostic) |
| **Crypto (AES-GCM, HMAC, hashing), PKCE _generation_** | ✅ | `core/crypto`, `core/pkce` |

**Net-new (the OP protocol surface) — bigger than the earlier draft implied:**
`/authorize` (server-side client + redirect exact-match), the **auth-code store**, **PKCE _verification_**, the `/token` **mint** *living in `crates/auth`* (cross-crate JWT-mint port from `session_token.rs`), **OIDC `id_token` mint with `nonce` + `at_hash`** (§5), **JWKS _serving_ + key rotation**, minimal discovery, the **rewritten login/consent/logout completion path** (no Hydra challenges), a **real-but-streamlined consent** that preserves the scope vocabulary + relay-alias minting (§4.2), and making the client registry **authoritative** (drop the Hydra mirror writes). This is the bulk of the project, not a tail.

**Stack constraint (unchanged invariant):** zero-tokio. Everything here is compio/ntex/cyper/`compio-postgres`/`jsonwebtoken`. No tokio, no reqwest. The mint/verify libs are already standardized (`jsonwebtoken` EdDSA across `session_token.rs`, `core/dpop.rs`, `core/oidc_verify.rs`); see §10.

---

## 3. Target architecture

```
 BROWSER (creator dashboard / end-user) ──cookie──▶ GATEWAY ──validates via platform JWKS──▶ worker (ZeroShip-User)
                                                      │  (upstream login dial → crates/auth, not Hydra)
 CLI / programmatic ──OAuth2 (device grant + /token)──┤
                                                      ▼
   ┌──────────────────────────────────────────────────────────────────────┐
   │  crates/auth  =  PLATFORM AS + OP   (public OIDC edge via the proxy)   │
   │  IdP:   users · password · magic-link · TOTP · sessions               │
   │  OP:    /authorize /token /device /jwks /.well-known                   │
   │         id_token mint (nonce + at_hash) · streamlined consent          │
   │  signs platform tokens (its key) · serves platform JWKS               │
   │  reads the authoritative client registry (control writes it)          │
   │  pairwise projection · relay-alias-at-consent · own-ecosystem email   │
   └──────────────────────────┬───────────────────────────────────────────┘
                              │ optional UPSTREAM social/federation (not the AS)
                              ▼
                 Supabase/GoTrue · Google · GitHub   (a human logs in "with X")
```

- **Token kernel moves into `crates/auth`.** It signs platform tokens with its own key and serves the platform JWKS. The gateway + control verify platform tokens via that JWKS (replacing Hydra introspection). The mint code is **ported** from `gateway/src/session_token.rs` (MAJOR #11) — not reused in place.
- **The worker never sees keys** (untrusted V8) — only the HMAC-signed `ZeroShip-User` header. The gateway holds public JWKS only. (Trust-domain separation unchanged.)
- **control** stays the internal/admin plane + the **writer** of the client registry (deploy provisions clients); `crates/auth` *reads + enforces* it. No Hydra admin calls.
- **Public OIDC edge — by proxy, not by bind (corrected, MINOR #9).** `crates/auth` **binds loopback by default** (`main.rs:60-65`; a non-loopback bind is only allowed under `--dev-insecure` and shouts a warning) and is fronted by Caddy / the gateway today. It is the public OIDC edge *via the reverse proxy*, which terminates TLS and serves the `auth.zeroship.ai` vhost. Promoting it is a **proxy-config + vhost** change (TLS termination, rate-limit, the `.well-known`/`/jwks` routes), **not** a change to its bind address.

---

## 4. The closed-world subset (what we build vs. cut)

**Build (own-usage only):**
- **Grants:** auth-code + PKCE (programmatic/CLI redirect clients) and the **device grant** (`zeroship login`). Refresh: see §4.1 — **one consistent model per face**.
- **Client model:** one type, **platform-provisioned** via deploy (no Dynamic Client Registration).
- **Token:** one platform JWT shape, one signing alg, **alg-pinned**, platform issuer, platform JWKS. Plus **OIDC id_tokens** for the OP face (§5).
- **Consent:** a **real, streamlined** consent (§4.2) — first-party *to the platform* but **not** first-party to the end user; it preserves the app-declared scope vocabulary, relay-alias minting, and per-app identity-release record. It is **not** the full third-party multi-client per-scope/remember/consent-CSRF product Hydra ships, but it is **not** "implicit/trivial" either.
- **Discovery + JWKS:** the minimal `.well-known/openid-configuration` + `/jwks.json` the gateway/SDK/CLI consume.

**Cut (the closed world deletes these — the high-risk machinery for untrusted third parties):** implicit/ROPC/hybrid grants, dynamic client registration, multiple client-auth methods, the *third-party* full per-scope "remember this app forever" consent UX, third-party introspection endpoint (we verify locally via JWKS), DPoP-at-mint, PAR/JARM/FAPI.

### 4.1 Refresh / longevity — ONE model per face (no contradiction)

<!-- Rewritten in round 1: addressing MAJOR #6 — §4.1 self-contradiction on anchors reuse -->

The earlier draft both deleted the BFF's reason to keep `anchors.rs` *and* called the anchors pattern "reusable." Decided coherently, per face:

- **BFF / browser face → NO OAuth refresh family. `anchors.rs` is RETIRED for this face.**
  Longevity is the **gateway session cookie** (already exists). Short-lived access is **re-minted from the live session** at the gateway, not from a stored refresh token. Concretely: the server-held refresh-family custody (`anchors.rs`, 700 LoC) and its Hydra `?mint=1`/`grant_type=refresh_token` re-mint glue **go away** for the BFF — there is no refresh token to custody when the session cookie *is* the longevity anchor. This is a deletion, not a reuse.
- **CLI / programmatic face → a NEW platform-issued rotating refresh.**
  The CLI needs offline longevity. We issue a **platform** rotating refresh token with **reuse-detection → family-revoke**. The *concept* (encrypted-at-rest, rotating, reuse→revoke) is the same shape `anchors.rs` implemented, but the implementation is **net-new**: today's `anchors.rs` custodies **Hydra-minted** refresh tokens and learns the 720h ceiling only via Hydra's `invalid_grant` (`anchors.rs:11-16,55-56`). The platform now **owns** the ceiling and the rotation, so this is a rebuild against platform mint, not a lift of `anchors.rs`. If even a platform refresh family is deemed too risky for v1, the CLI re-runs the device grant on expiry (UX cost, security simplicity). **P0 must fully specify the reuse-detection → family-revoke mechanics** (this is one of the two highest-CVE-density net-new pieces — see P0 gate; OAuth 2.1 §6.1 / RFC 9700 §4.14), not assert them.

### 4.2 Consent — streamlined, NOT trivial (the category-error fix)

<!-- Added in round 1: addressing CRITICAL #2 — "trivial/implicit consent" is a category error -->

**Why "implicit because all clients are platform-provisioned" is wrong.** Platform-provisioned means first-party *to the platform*; it does **not** make a creator's app first-party *to the end user*. An end user logging into creator-app X **is releasing identity to a third party** (the creator) — which is exactly when consent matters: cross-app identity leak, relay-alias issuance, scope grant. The existing code already treats each app as a distinct data controller, and that machinery must survive:

- **Per-app relay-email alias minted *at consent*** (`store/relay.rs::mint_alias_at_consent:82`, `app_user_identities`) — the privacy boundary that stops cross-app email correlation.
- **Per-app pairwise subject** (`core/auth::derive_pairwise`, written by the gateway at lazy-mint) — the same boundary for the subject identifier.
- **App-declared scope vocabulary + delegation gate** (`consent.rs::classify_and_authorize`, `load_app_scope_defs`: reserved-OIDC vs app scopes, `CANNOT_GRANT`, `invalid_scope`) — the authorization vocabulary the `/authorize` endpoint must consult.
- **The grant record** (`oauth_grants`) — the audit anchor of "this user released scopes S to app X at time T."

**What we build:** a **streamlined first-party consent** that *keeps all four* but drops the third-party-AS ceremony (multi-client "remember forever" management UI, consent-CSRF state machine sized for untrusted RPs). For a platform-provisioned app with a previously-granted scope set, consent may **auto-approve silently** (the path `consent.rs` already has at `:545` — the "silent" `accept_consent`) — but the relay-alias mint, pairwise projection, scope authorization, and `oauth_grants` write **still happen**. New scopes or first-time release still surface a consent screen. The rewrite removes the *Hydra challenge plumbing* (`consent_challenge`, `accept_consent`, `revoke_hydra_consent_sessions`), **not** the identity-release semantics.

This keeps the "Login with [creator's app]" future the §7 tripwire names *reachable* — trivializing consent would have deleted the scope vocabulary that future depends on.

---

## 5. Retained-critical bits (do NOT cut — full rigor + adversarial review)

<!-- Rewritten in round 1: addressing CRITICAL #2 (consent) + CRITICAL #3 (id_token/nonce) -->

Even closed-world, these are mandatory and each is a CVE class:

1. **PKCE verification** (S256, RFC 7636 §4.6) + the auth-code store (one-time, short TTL ≤ ~60s, bound to client+redirect+PKCE+pairwise+nonce; reject on reuse — RFC 6749 §10.5). **P0 must fully specify the binding set** — it is one of the two highest-CVE-density net-new pieces (see P0 gate).
2. **Redirect-URI exact-match** against the authoritative registry.
3. **Token mint correctness** — claims/aud/exp/iss + **alg-pin** (port the hardening discipline from the Supabase verify work; the mint code itself ports from `session_token.rs`).
4. **OIDC `id_token` mint — `nonce` binding + `at_hash`.** The BFF/RP flow consumes an id_token today (`gateway/src/oidc_rp.rs:268` `verify_id_token(&tr.id_token, …)`, with `nonce` generated and stashed at `:147-172`). A platform OP replacing Hydra for that face **must mint OIDC id_tokens** with the `nonce` it received echoed back, `at_hash` over the access token, correct `aud`/`iss`/`auth_time`/`amr`, and the **pairwise `sub`**. Omitting this is a token-substitution / nonce-replay CVE class. **This is mandatory, not optional.**
5. **Consent identity-release integrity** (§4.2) — relay-alias-at-consent, pairwise `sub`, app-declared-scope authorization, `oauth_grants`. The streamlined consent must not weaken these.
6. **JWKS serve + safe key rotation** (overlap windows, `kid`) — see §10.
7. **Session/token revocation + logout propagation** — the platform must now **originate** back-channel logout tokens (today `backchannel_logout.rs` *receives* Hydra-minted ones). See §6.

---

## 5A. Standards conformance & best practices

<!-- Added in round 2: grounding the OP build in the OAuth2/OIDC standards per operator direction "follow the standard + industry best practices". Sourced from the RFC-verified map at .oauth2-standards-conformance.md. -->

The operator's directive is explicit: **follow the standard + industry best practices.** A closed-world OP is still an OP — the *subset* shrinks, the *conformance bar does not move*. This section is the normative anchor: the governing specs, the baseline we build to (**OAuth 2.1 + RFC 9700 / BCP 240**), the non-negotiable conformance checklist that doubles as P1–P5 acceptance criteria, and the proof gate (OpenID Foundation conformance suite). Every RFC/§ below was verified against IETF Datatracker / RFC Editor / the OpenID Foundation, not memory.

### 5A.1 Governing specs (each OP component → its RFC)

| OP component | Spec(s) → § | Governs |
| --- | --- | --- |
| OAuth core + Bearer | **RFC 6749** (§4.1 auth-code, §3.1.2 redirect, §3.3 scope, §5.1/5.2 token/error, §10.5 codes, §10.12 CSRF) · **RFC 6750** (§2.1 `Authorization: Bearer`, §2.3 no-query) | Auth-code grant, token endpoint, error semantics, bearer presentation |
| PKCE | **RFC 7636** (§4.2 `S256` MTI, §4.6 verify) | Code-interception defense |
| Device grant | **RFC 8628** (§3.2 response, §3.4 polling interval, §3.5 `slow_down`/`authorization_pending`/expiry, §5 security, §6.1 `user_code` charset) | `zeroship login` CLI flow |
| JWT / JOSE | **RFC 7519** (claims+§7.2 validation) · **RFC 7515 JWS** (§4.1.4 `kid`, §10.7 reject `alg:none`) · **RFC 7517 JWK** (§4/§5 JWKS+rotation) · **RFC 7518 JWA** (§3.1 alg values) | Token structure, signing, JWKS, algorithms |
| JWT access tokens | **RFC 9068** (§2.1 `typ: at+jwt`, §2.2 required claims `iss exp aud sub client_id iat jti`, §4 RS validation) | Interoperable, audience-typed access tokens |
| Introspection / Revocation | **RFC 7662** (§2.1/§2.3 first-party, auth the caller) · **RFC 7009** (§2.1/§2.2 `/revoke`, HTTP 200 for unknown) | Token-state lookup + logout/revoke |
| AS metadata + OIDC discovery | **RFC 8414** (§2 fields, **§3.3 issuer exact-match**) · **OIDC Discovery 1.0** (§3 provider metadata) | `/.well-known/{oauth-authorization-server,openid-configuration}` |
| OIDC Core | **OIDC Core 1.0** (§2 id_token claims+`nonce`, **§3.1.3.6 `at_hash`**, §3.1.3.7 13-step validation, **§8.1 pairwise**) | `id_token`, UserInfo, pairwise subjects |
| Back-Channel Logout | **OIDC Back-Channel Logout 1.0** (§2.4 Logout Token, §2.5 push, §2.6 validation — `nonce` MUST NOT be present) | Server-to-server logout to RPs |
| **Native apps (CLI)** | **RFC 8252 / BCP 212** (§4/§8.12 system browser not webview, §6 auth-code+PKCE MUST, §7.3 loopback `127.0.0.1`/`[::1]`, §8.5 public client no secret) | The `zeroship login` redirect client |
| **Mix-up defense** | **RFC 9207** (§2 AS returns `iss` response param, §2.4 client validates, §3 metadata advertise) | IdP-confusion defense |
| DPoP *(noted, cut)* | **RFC 9449** | Sender-constrained tokens — **omitted**: rotation satisfies RFC 9700 for public clients (§5A.4) |
| Resource Indicators *(noted, cut)* | **RFC 8707** | `resource`→`aud` — **omitted while single API**; adopt at the 2nd resource server |

### 5A.2 Baseline — build to OAuth 2.1 + RFC 9700 (BCP 240)

The build targets **OAuth 2.1** (`draft-ietf-oauth-v2-1`) as the consolidated grant baseline and **RFC 9700 / BCP 240** (the OAuth Security BCP, formerly `draft-ietf-oauth-security-topics`) as the normative mitigation checklist. This is the correct greenfield default — it *removes* the insecure parts of RFC 6749 rather than carrying them.

**OAuth 2.1 mandates (beyond 2.0):**
1. **PKCE for ALL auth-code clients** (not just public) — §4.1.1. Generalizes RFC 7636.
2. **EXACT redirect-URI string match, no wildcards/substring/path-suffix** — §2.3.1 (loopback-port carve-out for native apps only).
3. **Implicit grant removed** (`response_type=token` gone) — §10.1.
4. **ROPC removed** — only auth-code, client-credentials, refresh survive.
5. **Public-client refresh tokens MUST be sender-constrained OR rotated (one-time-use + reuse detection)** — §6.1.
6. **No bearer tokens in the query string** — §7.2.6 (header only).

**RFC 9700 key mitigations (attack → required defense):**
- **Code injection/interception** → PKCE (public MUST) — §2.1.1.
- **Mix-up / IdP confusion** → the `iss` response param (RFC 9207) — §4.4.
- **Open redirect** → exact redirect-URI match — §2.1.
- **307 leaks credentials** → redirect with **303 See Other** (browser drops the POST body) — §4.12.
- **Refresh-token replay/theft** → rotation + reuse-detection revokes the whole family — §4.14.
- **Access-token / id-token confusion** → audience-restrict + type `at+jwt`; RS validates `aud`+`typ` — §2.3/§4.10.
- **Token leakage in transit** → TLS everywhere (BCP 195 to the RS) — §2.6.

### 5A.3 CONFORMANCE CHECKLIST (= P1–P5 acceptance criteria)

Non-negotiable. Each item is an acceptance test the corresponding phase must pass; unchecked = not shippable. (Grouped roughly by the endpoint that owns it.)

1. **PKCE `S256`** required on `/authorize`; reject `plain` and reject missing PKCE; `/token` recomputes `BASE64URL(SHA256(verifier))` and compares — **RFC 7636 §4.2/§4.6**.
2. **Exact redirect-URI match** against the authoritative registry (loopback-port exception only) — **OAuth 2.1 §2.3.1**.
3. **CSRF protection** via PKCE and/or `state` echoed verbatim — **RFC 6749 §10.12**.
4. **`nonce`** carried from `/authorize` request into the issued `id_token`; client validates — **OIDC Core §3.1.2.1/§3.1.3.7**.
5. **`iss` authorization-response parameter** returned on every response; advertised in metadata — **RFC 9207 §2/§3**.
6. **Access tokens: `typ: at+jwt` + audience-restricted `aud`** + required claims (`iss exp aud sub client_id iat jti`) — **RFC 9068 §2**.
7. **Refresh rotation + reuse-detection → family revoke** for public clients — **OAuth 2.1 §6.1 / RFC 9700 §4.14**.
8. **CLI: system browser (not embedded webview) + loopback redirect + PKCE** — **RFC 8252 §4/§7.3/§6**.
9. **Device polling discipline:** enforce min `interval`; `authorization_pending`; `slow_down` → client adds **5s**; expire at `expires_in` → `expired_token`; **explicit user approval** before issuing — **RFC 8628 §3.4–3.5/§5.4**.
10. **Reject `alg:none`** (and unexpected `alg`) everywhere a JWT is verified — **RFC 7515 §10.7 / RFC 9068 §4**.
11. **TLS on every endpoint; no tokens in URLs** — **RFC 9700 §2.6 / RFC 6750 §2.1**.
12. **`issuer` consistent** across discovery, token `iss`, and the RFC 9207 `iss` param — one exact `https` URL, no query/fragment — **RFC 8414 §3.3**.
13. **303 (not 307)** on the post-authentication redirect — **RFC 9700 §4.12**.
14. **Authorization codes: single-use, short TTL (≤ ~60s), bound to `client_id`+`redirect_uri`+PKCE** (zeroship adds pairwise+nonce); reject on reuse — **RFC 6749 §10.5**.
15. **Back-channel logout** (signed Logout Token, `events`+`sub`/`sid`, no `nonce`) **+ local session/token revocation** on logout — **OIDC BCL §2.4–2.6 / RFC 7009**.

These map directly onto the retained-critical list (§5) and the phase gates (§9): items 1–3/14 → P3 `/authorize`+`/token`; 4–6/10/12 → P1 id_token+JWKS+mint; 7 → P2 (device) + P5 (CLI refresh); 8–9 → P2 device; 13 → P4 login redirect; 15 → P5 back-channel logout re-home.

### 5A.4 The closed-world cuts are spec-PERMISSIBLE (not corner-cuts)

The §4 subset deletes machinery the specs themselves let a first-party OP omit — it is **standards-compliant**, not a violation:
- **Omitting implicit / hybrid / ROPC is *required*** by OAuth 2.1 §10.1 and RFC 9700 §2.1.2/§2.4 — the correct direction, not a shortcut.
- **Dynamic Client Registration (RFC 7591/7592)** — permissible to skip; DCR exists for open ecosystems. Clients are provisioned out-of-band via deploy.
- **Third-party introspection (RFC 7662)** — scope to first-party RS or skip entirely (every RS validates JWT access tokens locally via JWKS). Optional per RFC 8414 §2.
- **FAPI** — a higher bar for high-risk third-party ecosystems; not required for a closed first-party OP.
- **DPoP / mTLS sender-constraining (RFC 9449 / 8705)** — permissible to omit **provided** refresh rotation + reuse-detection is implemented (satisfies RFC 9700 §2.2.2 for public clients). The closed world meets the bar via rotation.
- **Resource Indicators (RFC 8707)** — hardcode the audience while there is exactly one resource server; adopt at the 2nd API.
- **KEEP pairwise subjects.** `public` subjects are *technically* spec-permissible, but pairwise is already implemented and is a privacy win — **keep, don't drop** (§4.2 / §5(5)). This is the one "spec-optional" item we deliberately retain.

This is the load-bearing point for the operator: **the closed-world subset is a standards-compliant profile of a full OP, not a security corner-cut.** What it removes is the untrusted-third-party surface the specs scope as optional; what it keeps is every MUST.

### 5A.5 Conformance proof gate

Conformance is **proven, not asserted.** The **OpenID Foundation Conformance Suite** (`openid.net/certification`, the public `conformance-suite` harness) — profiles **Basic OP** + **Config OP** (discovery) — exercises discovery, JWKS, id_token validation, nonce/state handling, and the error paths far more thoroughly than hand-written tests. **A green Basic-OP + Config-OP run is the acceptance bar for the Hydra cutover**, wired into the phase gates: a first run after P3 (`/authorize`+`/token`+id_token live), and a full green required at P5 before the gateway RP re-homing is declared done (see §9 P3/P5 gates). Even without paying for the published "Certified" mark, the suite runs locally as a regression gate.

---

## 6. Migration — rip Hydra (pre-launch, no shim), ISSUE-FIRST, dual-issuer transition

<!-- Rewritten in round 1: addressing CRITICAL #4 (P1 verify-before-issue incoherence), CRITICAL #5 (§6.8 blast radius is the bulk), MAJOR #4 (dual-issuer design), MAJOR #7 (two device surfaces) -->

The earlier ordering ("switch verifiers to platform JWKS" first) was **incoherent**: until the platform issues anything, every live token is still Hydra-issued, so cutting verifiers to platform JWKS would reject all live tokens. The correct order is **ISSUE-FIRST with a dual-issuer transition**.

### 6.0 Dual-issuer / dual-JWKS transition (the load-bearing mechanism, MAJOR #4 + §9)

<!-- Corrected in round 2 (MAJOR #1): the BFF session COOKIE is NOT part of the dual-issuer arm — it is already gateway-signed with its own `iss`. -->

**Scope of the dual-issuer arm — only the raw-Hydra-`iss` paths.** The migration touches *only* the surfaces that recognize a **raw-Hydra `iss`** today. Those are NOT the cookie. The gateway session cookie is **already gateway-signed with its own `iss`** (`typ: zeroship-sess+jwt`); `session_token.rs:17-18` says the Bearer/DPoP arms "recognize only a raw-Hydra `iss`, **not this token**." The raw-Hydra `iss` lives on the **access-token + introspection path** (`router/auth.rs:855` "recognized raw-Hydra user-session token (`iss == oidc_rp.issuer`)"; `:718` "introspect the access token as a raw hydra opaque token"). The dual-issuer arm is therefore scoped to exactly these four surfaces:

1. the gateway **Bearer / DPoP access-token** verify path (`router/auth.rs:855`);
2. the **introspection** path (`router/auth.rs:718`);
3. the **`oidc_rp` `id_token` verify** (`oidc_rp.rs:268`, today verifying a Hydra-issued id_token);
4. the **deploy-token `AuthProvider`** path (`crates/core/src/auth_provider`).

The **BFF session cookie is OUT of scope** — it is already platform/gateway-signed, consistent with §4.1 (longevity is the live session cookie, "re-mint from the live session", no stored Hydra refresh). The cookie arm needs **no** dual-issuer cutover.

The migration mechanism over those four surfaces:

1. **Verifiers learn TWO issuers.** The `AuthProvider` enum (`crates/core/src/auth_provider/mod.rs:20`, today `{Hydra, Supabase}`) gains a `Platform` JWKS arm. During migration the four surfaces above accept **both** platform-`iss` (new JWKS) **and** Hydra-`iss` (existing introspection/JWKS). The verifier *selects by `iss`/`kid`*; both are valid in flight.
2. **The issuer cuts over per app behind a flag** (§9). Per-app invariant: **an app is on exactly ONE issuer at a time** (never split — a single token has a single `iss`). **Fleet invariant:** mid-migration the fleet is **dual-issuer** — some apps Hydra, some platform. These are not in tension: per-token single-issuer; per-fleet dual-issuer.
3. **Hydra-`iss` recognition is removed LAST** (P6), once every app is flipped.

The flag is a per-app `issuer = hydra | platform` selector in the app/client registry, read by the gateway when choosing which verifier path + which `/authorize` upstream to dial. It is designed here, not punted.

### 6.1 Phase order (issue → migrate faces → remove Hydra)

1. **Stand up platform token issuance** in `crates/auth` (port the Ed25519 mint from `session_token.rs`; add JWKS-serve + rotation). **Verifiers gain the dual-issuer arm** (accept platform-`iss` AND Hydra-`iss`). *No verifier is cut over yet* — nothing is rejected.
2. **Device grant → platform-backed, single platform device grant** (resolves the two-surface collision, MAJOR #7). There are **two** device surfaces today: `control/src/device_handlers.rs` (560 LoC — a **GoTrue refresh-token broker**: returns a GoTrue `refresh_token` + `anon_key` + `token_endpoint`, requires a verified GoTrue bearer `GoTrueRole("authenticated")` to approve, hard-gates on `ensure_supabase_provider`, looks up email via `fetch_email_verified`) and `crates/auth/src/ui/device.rs` (581 LoC — the RFC-8628 **user-code entry page**, GET+POST). **Target:** a **single platform device grant owned by `crates/auth`.** Reuse: **only the RFC-8628 polling state machine** (`device_handlers.rs:290-343` device_code/user_code/poll/`slow_down`/expiry) + the **template/form skeleton** of the user-code entry page (`auth/ui/device.rs`). <!-- Corrected in round 2 (MINOR #2): ui/device.rs is itself Hydra+Supabase-coupled --> Note `auth/ui/device.rs` is **not** clean reusable UI: it imports `HydraAdmin` + `AcceptDeviceUserCodeRequest` (`device.rs:18-19`), validates the typed code "through Hydra's public" endpoint (`:56`), and renders `render_supabase_form` (`:51,72`). Only the askama template + the GET/POST form + CSRF skeleton is reusable; the **device-confirmation logic is net-new** (rip the Hydra device-challenge accept + the Supabase form). **Net-new (honest):** platform **token issuance** (replaces the GoTrue-refresh-broker output), **approver-authn** (replaces the GoTrue-bearer approver), and **the user-code-confirm logic** (replaces the Hydra device-challenge accept) — these are *superseded, not generalized*. `zeroship login` → platform device flow → **platform-issued** deploy token (no Supabase, no Hydra introspection).
3. **CLI `/authorize`+`/token`** (closed-world subset) for programmatic redirect clients: auth-code store + PKCE verify + the platform mint + id_token mint (§5).
4. **BFF upstream → rewritten `crates/auth` login (no Hydra challenges).** This is the big rewrite (CRITICAL #1): `login.rs` stops reading `login_challenge` / calling `get_login` / `accept_login`; it mints a platform session + (for the RP/BFF) a platform auth-code/id_token directly. The gateway RP (`oidc_rp.rs`) dials `crates/auth`'s `/authorize`, not Hydra's `/oauth2/auth`.
5. **Streamlined consent (§4.2)** — rewrite `consent.rs` off the Hydra consent-challenge plumbing (`consent_challenge`/`accept_consent`/`revoke_hydra_consent_sessions`) while **preserving** relay-alias-at-consent, pairwise, app-scope authorization, and `oauth_grants`.
6. **Make the client registry authoritative** — drop `HydraAdmin::create_client` from `ensure_app_client`; `crates/auth` enforces redirect/scope from `control.oauth_clients`, and `/authorize` consults the app-declared scope defs (`load_app_scope_defs`) the consent path reads (resolves MINOR #10).
7. **Re-home the gateway RP stack onto platform tokens — THE BULK (§6.2 below).** This is multi-phase, not a final-cleanup line.
8. **Flip apps issuer→platform per the flag** (§6.0), then **remove Hydra-`iss` recognition + Hydra entirely** — sidecar, `HydraAdmin`, `oauth_hydra` schema/role, ory configs, the Hydra arm of `AuthProvider`, **and the entire `crates/auth/src/hydra_client/` module**. <!-- Enumerated in round 2 (MINOR #3) --> That module is **9 files** — `clients.rs`, `consent.rs`, `device.rs`, `jwks.rs`, `login.rs`, `logout.rs`, `mod.rs`, `sessions.rs`, `types.rs` — i.e. a *full second `{login,consent,device}.rs` set distinct from `ui/`*. The `ui/` set is **rewritten** (mint platform artifacts); the `hydra_client/` set is **deleted outright** (it is pure Hydra-admin/public client glue with no platform analogue). Enumerating both makes the rewrite-vs-delete split unambiguous.

### 6.2 Gateway re-homing — the blast radius IS the project (CRITICAL #5)

The gateway is **not** "Hydra-token-shaped at a few call sites." It is a **full Hydra OIDC relying-party**, ~2,700 LoC, all of which must be re-homed onto platform-issued tokens:

- **`oidc_rp.rs` (1,560 LoC)** — a complete OIDC RP: dials Hydra `/oauth2/auth` + `/oauth2/token`, runs a **circuit breaker for ALL outbound Hydra token/introspect/revoke calls** (`:52-107`), generates `state`+`nonce` (`:147-172`), and **verifies a Hydra-issued `id_token`** (`:268-275`), with DPoP / RFC-9068 `at+jwt` assumptions. Re-home: dial `crates/auth`, verify **platform** id_tokens (which the platform must now *mint*, §5). The Hydra circuit breaker is **deleted** — an in-process AS does not need an outbound breaker (state explicitly: the breaker existed because Hydra was a network sidecar).
- **`anchors.rs` (700 LoC)** — custody store for **Hydra-minted refresh tokens**, re-minted via Hydra `/oauth2/token?grant_type=refresh_token`, with the **720h family ceiling Hydra-enforced via `invalid_grant`** (`:11-16,55-56`). Per §4.1: **retired for the BFF face**; **rebuilt as platform-owned rotating refresh for the CLI face**.
- **`backchannel_logout.rs` (425 LoC)** — an RP endpoint that **receives Hydra-POSTed `logout_token`s** against `state.oidc_rp.issuer` (the "canonical hydra issuer string", `:53-56`). Re-home: the platform OP must now **originate** back-channel logout tokens; this endpoint either flips to verifying **platform**-issued logout tokens or is replaced by platform-internal session-revocation fan-out.

Removing Hydra means `crates/auth` must **issue** auth codes, id_tokens (with nonce + at_hash), access tokens, refresh tokens, **and originate** back-channel logout — a near-complete OP — while the gateway re-homes all 2,700 LoC above. **This is the multi-phase bulk of the epic (phases 7–8), not a final cleanup line.**

---

## 7. Closed-world invariants + tripwire

The subset is safe **only while these hold**; encode them as asserts/docs + a review tripwire:
- **No third-party OAuth clients.** All clients are platform-provisioned via deploy. (Tripwire: any request to register an external client / a non-platform redirect host → this design no longer applies; revisit before enabling.)
- **No dynamic client registration endpoint.**
- **Redirect URIs are exact, platform-known values.**
- **One client class, one token shape, one alg.**
- **Consent preserves per-app identity-release** (relay alias + pairwise + scope grant) even when streamlined (§4.2) — the privacy boundary is **not** a closed-world cut.

If any tripwire trips (e.g. "Login with [creator's app]" for third parties, an external developer API platform), **stop** — that's the signal to adopt a full AS product, not extend the subset. Note: because §4.2 keeps the app-declared scope vocabulary, that future is reachable from this design rather than blocked by it.

---

## 8. Relationship to the 11-commit pluggable-provider epic (honest demotions)

<!-- Rewritten in round 1: addressing MAJOR #8 — device flow demotion honesty -->

- **State machine + UI skeleton survive; token model + confirm-logic superseded — the device flow.** The RFC-8628 **polling state machine** (`device_handlers.rs:290-343`) and the **template/form skeleton** of the user-code entry page (`auth/ui/device.rs`) survive and become the platform CLI grant's spine. But its **token model is superseded**: today it brokers a **GoTrue refresh token** (`DeviceTokenResponse` returns `refresh_token`/`token_endpoint`/`anon_key`, `:63-69,415-422`) and its **approver-authn is a GoTrue bearer** (`GoTrueRole("authenticated")`, `:477`); the platform now issues its **own** device token and authenticates the approver via the platform session. The **user-code-confirm logic is also superseded** — `auth/ui/device.rs` is itself Hydra+Supabase-coupled (`HydraAdmin`/`AcceptDeviceUserCodeRequest` `:18-19`, validate-through-Hydra `:56`, `render_supabase_form` `:51,72`), so only the askama template + CSRF form skeleton is lifted; the accept logic is net-new. Honest demotion: **state machine + UI skeleton = survive; token issuance + approver-authn + confirm-logic = superseded.**
- **Survives unchanged:** the own-ecosystem email hook + `crates/mailer` → provider-agnostic, kept.
- **Seam survives, arm swapped:** `AuthProvider` verify seam (`crates/core/src/auth_provider/mod.rs`) → gains a **`Platform` JWKS arm**; the Hydra-introspection arm is dropped at P5 (after dual-issuer cutover, §6.0).
- **Repurposed:** the Supabase identity-bridge / GoTrue login + the `email_confirmed_at` admin lookup → become the **Supabase-as-upstream-social** federation adapter (§3), not a deploy-auth backend.
- **Superseded:** "control verifies GoTrue *access tokens* for deploy" — the platform now issues + verifies its **own** deploy tokens. Honest cost of the decision: part of the just-shipped Supabase deploy-auth work is demoted from "co-equal provider" to "optional upstream social."

---

## 9. Phasing (each phase gated; adversarial review on every auth-touching slice)

<!-- Rewritten in round 1: addressing CRITICAL #4 (issue-first), MAJOR #4/#7 (dual-issuer, device reconciliation), and re-sizing P5→a multi-phase bulk -->

| Phase | Deliverable | Gate |
| --- | --- | --- |
| **P0** | Spec + **threat model** (full attack catalog for the retained surface incl. id_token/nonce) + key-rotation/JWKS-overlap design (§10) + the per-face refresh decision (§4.1) + the dual-issuer flag design (§6.0) + **ops/rollback story**. **Two pieces MUST be fully specified here, not asserted (highest CVE-density net-new surface):** (1) the **CLI rotating-refresh reuse-detection → family-revoke** mechanics (encrypted-at-rest token family, rotation on each use, retired-token reuse → revoke entire chain — OAuth 2.1 §6.1 / RFC 9700 §4.14); (2) the **auth-code-store binding set** (single-use, short TTL ≤ ~60s, bound to `client_id` + `redirect_uri` + PKCE challenge + pairwise + `nonce`; reject on reuse — RFC 6749 §10.5). | Reviewed; this proposal's open items closed; **the two highest-risk net-new pieces (CLI refresh reuse-detection + code-store binding) specified, NOT left "TBD"** |
| **P1 (ISSUE-FIRST)** | `crates/auth` issues + signs platform tokens (port mint from `session_token.rs`) **incl. id_token (nonce+at_hash)**; serves JWKS + rotation; **verifiers gain the dual-issuer arm (accept platform-`iss` AND Hydra-`iss`)** | Platform tokens mint + verify; **zero live tokens rejected** (Hydra-`iss` still accepted) |
| **P2** | **Single platform device grant** (reconcile `control/device_handlers.rs` + `auth/ui/device.rs` into `crates/auth`; reuse RFC-8628 SM + the UI template/form skeleton only — rip the Hydra device-challenge + Supabase form logic from `auth/ui/device.rs:18-19,51,56,72`; net-new token-issuance + approver-authn + user-code-confirm); `zeroship login` → platform deploy token | Deploy e2e passes with platform token; no Supabase/Hydra in the path |
| **P3** | Closed-world `/authorize`+`/token` for CLI/programmatic clients (auth-code store + PKCE verify + id_token mint) | OAuth2 e2e + adversarial review (PKCE/redirect/alg/code-store/id_token-nonce) + **first OpenID Foundation conformance-suite run (Basic OP + Config OP)** against the live OP (§5A.5) |
| **P4** | **Rewrite** BFF upstream = `crates/auth` login (drop Hydra challenges); **streamlined consent (§4.2)** preserving relay-alias/pairwise/scope/`oauth_grants`; registry authoritative | Browser login e2e; consent identity-release review (no cross-app leak) |
| **P5 (BULK)** | **Re-home the gateway RP stack** (`oidc_rp.rs` dial+id_token-verify; `anchors.rs` retire-BFF / rebuild-CLI refresh; `backchannel_logout.rs` originate-platform) onto platform tokens; delete the Hydra circuit breaker | Full RP e2e on platform tokens; blast-radius regression; **full green OpenID Foundation conformance-suite (Basic OP + Config OP) required before re-homing is declared done** (§5A.5) |
| **P6** | **Flip apps issuer→platform per flag**, then remove Hydra-`iss` recognition + Hydra entirely (sidecar, HydraAdmin, `oauth_hydra` schema/role, ory configs, Hydra arm of `AuthProvider`, **and the entire `crates/auth/src/hydra_client/` 9-file module**: `clients.rs`/`consent.rs`/`device.rs`/`jwks.rs`/`login.rs`/`logout.rs`/`mod.rs`/`sessions.rs`/`types.rs`) | Full stack green with no Hydra; dual-issuer arm removed; `grep -r hydra crates/` empty |
| **P7** | Optional upstream social federation (Supabase/Google/GitHub) | Federation e2e |

**Issuer invariants (made consistent, MAJOR #4 + #7):**
- **Per app / per token:** exactly ONE issuer — an app is on `hydra` *or* `platform`, never split; a token carries a single `iss`.
- **Per fleet (mid-migration):** **dual-issuer** — some apps Hydra, some platform; verifiers accept both until P6 removes the Hydra arm.

---

## 10. Risks, ops, rollback & non-goals

<!-- Updated in round 1: addressing MINOR #10 (JOSE lib decided; key-rotation is the real open item) + adding the absent ops/rollback surface -->

- **Risk — owning a security-critical OP forever.** Mitigated by the closed-world subset (smaller, auditable surface), reuse of hardened verify primitives, and adversarial review per slice. The tripwire (§7) bounds it. **But be honest (§11): the closed world makes the OP _auditable_, not _small_.**
- **Risk — the Hydra-removal blast radius IS the bulk** (§6.2): ~2,700 LoC of gateway RP machinery (`oidc_rp.rs`/`anchors.rs`/`backchannel_logout.rs`) plus the login/consent rewrite. Sized across P4–P6, not a cleanup line.
- **Risk — the operator accepts** that part of the just-shipped Supabase deploy-auth work is demoted (§8).
- **Operational surface (new, was absent):** JWKS endpoint caching/`Cache-Control` at the proxy edge; **key-rotation runbook** (generate new `kid` → publish in JWKS → overlap window ≥ max token TTL → sign with new key → retire old `kid` after overlap); monitoring/alerting on mint-failure + verify-failure rates; **no outbound circuit breaker needed** (the AS is in-process — the `oidc_rp` breaker existed only because Hydra was a network sidecar; state its removal explicitly).
- **Rollback / coexistence (new, was absent):** the dual-issuer transition (§6.0) **is** the rollback mechanism. Because verifiers accept both `iss` until P6, a failed per-app cutover is reverted by flipping that app's `issuer` flag back to `hydra` — no token-format migration, no re-login storm. Hydra stays installed until P6, so any phase P1–P5 backs out by flag. After P6 (Hydra removed) rollback is forward-only.
- **Non-goal:** third-party OAuth clients / general-purpose AS (the tripwire).
- **Non-goal:** FAPI/DPoP/PAR unless a tripwire forces it.
- **Open (P0):** the per-face refresh model (§4.1); **key-rotation cadence + JWKS overlap windows** (the JOSE *lib* is **decided** — `jsonwebtoken`/EdDSA, standardized across `session_token.rs`/`core/dpop.rs`/`core/oidc_verify.rs`, mandated by the zero-tokio invariant — so lib selection is NOT an open item); whether the BFF and CLI faces share one issuer/JWKS (recommended) or two.

---

## 11. Build-vs-rent re-assessment (round 1 honesty pass)

The operator has **decided** to build (this section does not relitigate it). But the round-1 verification changes the *honest size* of what "build" means, so the decision should be made with eyes open:

- **This is a near-complete OpenID Provider build**, not a "/authorize+/token bolt-on." The platform must mint auth codes, access tokens, **id_tokens (nonce + at_hash)**, rotating refresh (CLI face), **and originate back-channel logout** — plus rewrite the entire Hydra-challenge-driven login/consent/logout completion path, plus re-home ~2,700 LoC of gateway RP machinery.
- **What the closed world genuinely saves:** the *untrusted-third-party* surface — DCR, multi-client-auth, the full per-scope consent product, implicit/hybrid/ROPC, FAPI/PAR/JARM, third-party introspection. That is a large, genuinely-cut attack surface. The remaining OP is **auditable** (one client class, one token shape, one alg, exact-match redirects).
- **What does NOT shrink:** the CVE-class core (PKCE, code store, id_token/nonce/at_hash, alg-pin, redirect exact-match, key rotation, revocation/logout, relay/pairwise identity-release). These are mandatory at full rigor regardless of closed-world.
- **Net:** the build is justified *if* the operator values (a) deleting the Hydra sidecar + `oauth_hydra` schema/role + ory ops, (b) the zero-tokio in-process simplicity (no outbound breaker, no network introspection), and (c) owning the token kernel end-to-end — **and accepts** owning a security-critical OP forever, sized across ~7 gated phases with adversarial review on each. The earlier draft's framing (5–6 net-new components, "trivial consent", P5-cleanup Hydra removal) **undersized this by roughly the bulk of the work**; this revision corrects the size without changing the decision.

---

## Revision log (round 1)

Each item maps to a verified code citation; nothing below is asserted from memory.

| # | Severity | Critique | Resolution | Evidence |
| --- | --- | --- | --- | --- |
| 1 | CRITICAL | "crates/auth already *is* the IdP" overstated | §1 + §2 + new scope statement: crates/auth is today a Hydra login/consent **front-end**; becoming self-contained **rewrites** login/consent/logout into a near-complete OP. | `login.rs:5-8,52-119,335-391`; `consent.rs:59-317` |
| 2 | CRITICAL | "trivial/implicit consent" category error | New §4.2 + §5(5) + §7: real **streamlined** consent preserving relay-alias-at-consent, pairwise sub, app-declared scope vocabulary, `oauth_grants`. Removed "trivial". | `relay.rs:82`; `consent.rs::classify_and_authorize/load_app_scope_defs/:545` |
| 3 | CRITICAL | id_token + nonce missing from retained-critical | New §5(4): mandatory id_token mint with nonce binding + at_hash + pairwise sub; added to net-new in §2 and to P1/P3 gates. | `oidc_rp.rs:147-172,268-275` |
| 4 | CRITICAL | P1 verify-before-issue incoherent | §6 + §9 redesigned **ISSUE-FIRST** with §6.0 dual-issuer/dual-JWKS transition; per-token single-issuer / per-fleet dual-issuer invariants made consistent. | `session_token.rs:14-32`; `auth_provider/mod.rs:20` |
| 5 | CRITICAL | §6.8 blast radius is the bulk, not one phase | New §6.2 sizes the gateway RP re-homing (~2,700 LoC) as P4–P6 bulk; P5/P6 split in §9. | `oidc_rp.rs` 1560; `anchors.rs` 700; `backchannel_logout.rs` 425 |
| 6 | MAJOR | §4.1 self-contradiction on anchors | §4.1 rewritten to ONE model per face: BFF **retires** anchors; CLI gets a **net-new** platform rotating refresh. | `anchors.rs:11-16,55-56` |
| 7 | MAJOR | two device surfaces unreconciled | §6.1(2)+§9 P2: single platform device grant in crates/auth; reuse only the RFC-8628 SM; token-issuance + approver-authn are net-new. | `control/device_handlers.rs:63-69,290-343,415-422,477`; `auth/ui/device.rs` (581) |
| 8 | MAJOR | §8 device "survives/primary" too rosy | §8 split: state machine survives; token model + approver-authn superseded. | `device_handlers.rs:63-69,415-422,477` |
| 9 | MINOR | §3 public-edge claim | §3 corrected: binds loopback by default; public edge **by proxy**, not by bind. | `main.rs:60-65` |
| 10 | MINOR | §10 JOSE lib listed as open | §10: lib is **decided** (`jsonwebtoken`/EdDSA, zero-tokio); real open item = key-rotation cadence + JWKS overlap. | `session_token.rs`, `core/dpop.rs`, `core/oidc_verify.rs` |
| 11 | MAJOR | signing reuse misattributed | §2 + §3: the Ed25519 JWT **mint** is `session_token.rs::issue`, NOT `signing.rs` (key-loader/thumbprint only); crates/auth has zero JWT-mint today → cross-crate port. | `session_token.rs:166,197`; `signing.rs:1-75` |

Also addressed from the critique's "Missing Concepts": id_token/nonce (§5), relay-alias/pairwise timing (§4.2), the migration/issuer flag (§6.0), the two-device reconciliation (§6.1/§9 P2), rollback/coexistence (§10), and the new-AS operational surface (§10).

---

## Revision log (round 2)

Two jobs this round: **(A)** fold in the OAuth2/OIDC standards-conformance grounding (operator directive: "follow the standard + industry best practices"), and **(B)** close the round-2 critique minor must-fixes. All citations verified against the real tree + the RFC-verified map at `.oauth2-standards-conformance.md`.

### (A) Standards conformance grounding

| Item | Resolution | Where |
| --- | --- | --- |
| Governing-specs table | New §5A.1 — each OP component → its RFC(s)/§ (6749/6750, 7636, 8628, 7519/7515/7517/7518, **9068 `at+jwt`**, 7662/7009, **8414 §3.3 issuer exact-match** + OIDC Discovery, OIDC Core incl. `at_hash` §3.1.3.6 + pairwise §8.1, OIDC BCL, **8252 native apps**, **9207 mix-up `iss`**, DPoP 9449 noted-cut, Resource Indicators 8707 cut). | §5A.1 |
| Baseline = OAuth 2.1 + RFC 9700 | New §5A.2 — states the normative baseline; lists the 6 OAuth 2.1 mandates (PKCE-all §4.1.1, exact-match §2.3.1, implicit+ROPC removed, public-client rotation §6.1, no query tokens) + the RFC 9700 mitigations (PKCE / `iss` 9207 / exact-match / **303 not 307** / RT rotation+reuse-detection / `at+jwt` audience / TLS). | §5A.2 |
| Conformance checklist (~15 items) as acceptance criteria | New §5A.3 — 15 RFC-§-tagged MUST items, explicitly mapped onto the §9 phase gates (P1–P5). | §5A.3 |
| Closed-world cuts shown spec-permissible | New §5A.4 — implicit/hybrid/ROPC omission is *required* by 2.1; DCR/3p-introspection/FAPI/DPoP-or-mTLS(rotation covers it)/Resource-Indicators permissible to omit; **pairwise kept** (privacy keep-don't-drop). Frames the subset as a compliant profile, not a corner-cut. | §5A.4 |
| Conformance proof gate | New §5A.5 + wired into §9: **OpenID Foundation conformance suite (Basic OP + Config OP)** — first run at P3, full green required at P5 before RP re-homing is declared done. | §5A.5, §9 P3/P5 |

### (B) Round-2 minor must-fixes

| # | Sev | Critique | Resolution | Evidence |
| --- | --- | --- | --- | --- |
| 1 | MAJOR | §6.0 over-states cookie arm in the dual-issuer mechanism | §6.0 rewritten: the BFF session **cookie is OUT of scope** (already gateway-signed, `typ: zeroship-sess+jwt`); dual-issuer arm scoped to the **4 raw-Hydra-`iss` surfaces** — Bearer/DPoP access-token, introspection, `oidc_rp` id_token verify, deploy-token `AuthProvider`. Reconciled with §4.1 (BFF longevity = live session, re-mint). Cuts migration scope in the design's favor. | `session_token.rs:17-18`; `router/auth.rs:855,718`; `oidc_rp.rs:268` |
| 2 | MINOR | `auth/ui/device.rs` reuse over-claimed (it is itself Hydra+Supabase-coupled) | §2 device row / §6.1(2) / §9 P2 / §8 corrected: reuse **only the askama template + CSRF form skeleton**; the device-confirm logic is **net-new** (rip the Hydra device-challenge accept + the Supabase form). | `device.rs:18-19,51,56,72` |
| 3 | MINOR | `hydra_client/` deletion under-enumerated | §1.5 + §6.1(8) + §9 P6 gate now enumerate the full **`crates/auth/src/hydra_client/` 9-file** deletion (`clients/consent/device/jwks/login/logout/mod/sessions/types.rs`) — a second `{login,consent,device}.rs` set distinct from `ui/`; rewrite-vs-delete split made unambiguous. | `crates/auth/src/hydra_client/` (9 files) |
| 4 | P0-flag | CLI refresh reuse-detection + code-store binding asserted, not specified | §9 P0 gate + §4.1 + §5(1) now name these as the **two highest-CVE-density net-new pieces** that MUST be fully specified at P0 (not "TBD"): CLI rotating-refresh reuse-detection→family-revoke (OAuth 2.1 §6.1 / RFC 9700 §4.14) + auth-code-store binding set (client+redirect+PKCE+pairwise+nonce, single-use, ≤~60s TTL — RFC 6749 §10.5). | §9 P0, §4.1, §5(1) |

**Net effect:** the design now (a) accurately scopes the OP build (cookie arm excluded from migration, device-UI + `hydra_client/` deletion honest), (b) is grounded in the OAuth2/OIDC standards with an explicit OAuth 2.1 + RFC 9700 baseline and a 15-item conformance checklist tied to the phase gates, and (c) names the OpenID Foundation conformance suite (Basic OP + Config OP) as the cutover acceptance gate.
