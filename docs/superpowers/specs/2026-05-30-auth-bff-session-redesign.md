# @zeroship/auth — BFF session redesign (identity-to-browser, power-server-side)

**Status:** design, doc-only. Worktree `feat/auth-sdk-popup`. Supersedes the **session model** of
`2026-05-29-auth-sdk-design.md` (the "client-held JWT parity / `cacheLocation` memory" model). It does
**not** supersede that doc's scope/pairwise/relay/consent subsystems, which it reuses verbatim —
it changes *what the browser holds* and *where the power token lives*.

**This is a deliberate reversal, decided security-first.** The original build shipped the browser a
gateway **wrapper access token** (a real capability: `scope` claim, `client_id`, `aud`, Bearer-armed at
the gateway) via `POST /__zs/auth/token`, held client-side (`cacheLocation: "memory"`, `CacheManager`,
optional Web-Worker refresh, `navigator.locks`). After reviewing how Auth0 frames the
authentication/authorization split and how the IETF browser-apps BCP ranks deployment patterns, the
target is the **Backend-For-Frontend (BFF)** tier:

> **USER DIRECTIVE (authoritative target, verbatim):** "if an app requires user to login via spa
> oauth consent, we should not send the power token into browser, instead, just user identity token,
> the power token can be exchanged via server side sdk, we will keep the oauth permission grant in
> platform level tables."

The target model is **decided**. This doc designs to it; it does not relitigate it.

> **One refinement of the directive (strictly stronger, §10-D1):** the directive says send "just user
> identity token" to the browser. This design goes one notch further to the strictest BFF reading —
> the identity token is **held server-side and the browser holds no JWT at all**, learning identity via
> a `{ user, … }` projection. That fully satisfies the directive's intent ("no power token in the
> browser"; identity stays browser-*facing*) while removing even the inert identity JWT from
> JS-reachable memory. The "identity-to-browser" framing in the title means **identity is what the
> browser-facing surface exposes** (vs. the power token, which never is), not that a literal JWT sits in
> JS. If the review gate prefers the literal "id token in the body" reading of the directive, §2.2's
> response block shows exactly what to add back.

---

## 0. Grounding

The model below is anchored in three sources, all read in full before writing:

- **Auth0 / BFF research** (`…/tasks/wibozmr06.output`). The load-bearing, *verified* claims:
  - **ID token = authentication, addressed to the client** (`aud == client_id`); Auth0 docs say
    verbatim *"Do not use ID tokens to gain access to an API"* and *"ID Tokens should never be used
    to obtain direct access to APIs or to make authorization decisions."* (verdict: **confirmed**).
  - **Access token = authorization, addressed to a resource server** (`aud == API identifier`,
    carries `scope`), validated **at the resource server on every request**; the client treats it as
    opaque and **never makes the authz decision** — client-side permission awareness is UX-only
    (verdict: **confirmed**).
  - **BFF is the most secure browser-app tier.** The IETF "OAuth 2.0 for Browser-Based Apps" BCP
    (draft-26, 2025-12-04) ranks **BFF > Token-Mediating Backend > browser-based client** in
    *decreasing* security and "strongly recommends" BFF for business/sensitive/personal-data apps. In
    a true BFF "there are no tokens available to extract from the browser"; the browser holds only an
    HttpOnly session cookie; the backend is a confidential client that custodies all tokens and
    proxies API calls injecting the token server-side. The trade is XSS *token* theft → CSRF/session
    duty, mitigated by cookie attributes + anti-CSRF — "a good trade in most cases" (mechanism:
    **confirmed**; the "Auth0 *docs* recommend BFF for regulated apps" attribution: **uncertain**, it
    lives in Auth0 blog + the IETF BCP, which is the right authority for a high-privilege surface).
  - **Step-up auth** gates sensitive ops beyond initial login via a fresh authorization (web:
    `acr_values` + `amr=["mfa"]`; API: deny on missing elevated scope → re-challenge → new token with
    the scope) (verdict: **confirmed**).
  - **Console mapping** (from the research's §4 table): the builder/console is the highest-privilege
    surface — treat it like a regulated app, BFF token custody is non-negotiable, least-privilege +
    audience-scoped, client never decides, step-up for deploy/secret-rotation/Stripe-onboarding.

- **Original build** (`2026-05-29-auth-sdk-design.md`, §1/§2/§8): what shipped — the wrapper access
  token to the browser, the client-held memory cache + refresh machinery, the server-held refresh
  family under `auth.app_session_anchors`, `control.oauth_grants` as the consent ledger, the
  `env.auth` `ZeroShip-User` header carrying identity + scopes to the worker, pairwise `pws_` +
  relay alias.

- **The real code seams** (read directly):
  - `crates/gateway/src/auth_token.rs` — `POST /__zs/auth/token` mints the per-app **wrapper** (the
    capability) and **returns it in the JSON body** (`"access_token": wrapper`, line 519);
    `GET /__zs/auth/session?mint=1` re-mints and returns a wrapper (`mint`/`do_refresh`,
    lines 633–865). This is one of the "power token to the browser" surfaces to remove.
  - `crates/gateway/src/anchors.rs` — `auth.app_session_anchors` custody: encrypted server-held
    refresh family, `WRAPPER_TTL_SECS = 600`, `ANCHOR_ABS_DAYS = 30`, `ANCHOR_MINT_CACHE_TTL_SECS = 5`,
    `__Host-zs_app_anchor` cookie (`HttpOnly; Secure; SameSite=Strict`, 30d no-idle), a **single**
    `cached_access_token`/`cached_access_exp` column pair, `granted_scopes`, the breadcrumb cookie.
    The custody half **stays**; it becomes the BFF's server-side token store. `anchors.rs:62-68`
    + its test (`anchors.rs:583-588`) assert the anchor is a SEPARATE store from `gateway_sessions`
    that must NEVER be cross-validated — "one cookie name ⇒ exactly one table."
  - `crates/gateway/src/sessions.rs` — `auth.gateway_sessions`: the interactive OIDC cookie session
    (`__Host-zs_app_session`, `SameSite=Lax`, 12h/30-min idle), carrying its own `granted_scopes`
    column. **This is the store the live cookie arm reads today.**
  - `crates/gateway/src/router/auth.rs` — TWO live request arms today:
    - the **Bearer/DPoP arm** (`resolve_bearer_user_header`, line 1090) verifies the gateway wrapper
      (`state.wrapper_verifier`, the `iss == public_url` branch at line 1122), enforces `aud == host`,
      the `(client_id, sub)` family-marker revocation, and the `pws_` self-describing-subject
      invariant; the **raw-Hydra branch** (`iss != public_url`) projects a global-UUID Hydra Bearer to
      `pws_`. These serve **non-browser clients**; the SPA stops using them.
    - the **cookie arm** (`resolve_app_session_user_header_inner`, line 1520) reads
      `auth.gateway_sessions` via `sessions::validate(conn, session_id, app_id)`, parses the
      `__Host-zs_app_session` cookie via `oidc_rp::parse_app_session_cookie`, and emits `ZeroShip-User`
      with scopes from the session row's `granted_scopes` (NOT a `control.oauth_grants` join — see
      changeset 0008 below). **This arm reads `gateway_sessions`, not the anchor store.**
  - `db/changelog/changesets/0008_auth_gateway_sessions_granted_scopes.sql` — deliberately
    denormalized `granted_scopes` onto `gateway_sessions` *specifically* so "the gateway emits
    `WorkerUser.scopes` … from this column instead of a cross-schema `control.oauth_grants` join on the
    hot path." Any design that re-introduces that join on a per-request path reverses this decision and
    must justify it (see §4).
  - `crates/gateway/src/dpop_exchange.rs` — `POST /__zs/auth/dpop-exchange` takes a **raw Hydra Bearer**
    + an RFC 9449 DPoP proof and mints a `cnf.jkt`-bound wrapper. This is the **third** wrapper-mint
    surface and the ONLY one used by non-browser DPoP clients. Untouched by this redesign.
  - `crates/runtime/src/auth.rs` + `crates/worker/src/cache.rs:52` — **`env.auth` is REGISTERED AND
    LIVE today.** `AuthPlugin` registers `env.auth.getUser()` / `env.auth.requireUser()` and is pushed
    unconditionally on every worker. This redesign **builds on a live plugin** (its identity reads), it
    does not bootstrap a "planned" one (corrects the stale AGENTS.md note that calls `env.auth` "planned").
    The new `getAccessToken`/`fetchAs` are **NOT** added to this native plugin — they are async and live
    in the JS `@zeroship/auth/server` SDK (§3.2 below).
    <!-- Added in round 2: addressing BLOCKER #1/#2/#6 + the "async HTTP-op machinery in the synchronous
         env.auth V8 plugin" piece. Verified directly: `get_user_callback`/`require_user_callback`
         (auth.rs:95/123) are **synchronous** V8 callbacks that read in-process `RuntimeState` via the
         isolate slot — there is NO async-op (Promise-returning) machinery in this plugin. `getAccessToken`
         MUST be async (a network round-trip). So it is NOT added as a native `env.auth` op at all; it is
         implemented in the **JS server SDK** (`@zeroship/auth/server`) as a `fetch()` to the mint endpoint
         (the runtime already exposes WinterCG `fetch`). The native plugin is left synchronous and
         untouched except for identity reads. This is the corrected §3.2 design. -->
  - `crates/worker/src/main.rs:153-178` (`WorkerConfig`) — **the worker holds NO gateway URL.** Verified:
    its only outbound coordinates are `control_url` + `control_key` (+ optional `db_url`, `blob_store`).
    There is no `gateway_url`, no gateway client, and the dispatch direction is **gateway→worker** only
    (`proxy::forward_dispatch`; `ZeroShip-User` is pushed gateway→worker at `proxy.rs:~378`). A
    worker→gateway mint endpoint would be an **entirely new inter-service link** (new URL config, new
    client, new gateway listener, new re-entrancy into the gateway's in-flight dispatch). It does not
    exist today and is NOT "extending a live plugin." This is why §3.2 locates the mint in the **control
    plane**, which the worker already reaches (below).
  - `crates/control/src/internal.rs` — **the worker→control internal channel already exists.** Workers
    call control-plane internal endpoints (env merge, versions, etc.) authenticating with the
    `control_key` shared secret (`check_auth`, `internal.rs:27-32`; "workers authenticate with the
    control-key shared secret and there is no user principal", `:68/:94/:111`). This is the established,
    documented worker→control link the power-token mint reuses — **no new inter-service link.**
  - `crates/control/src/relay_revoke.rs:16` — **control reaches the `auth` schema over the same DB
    client** ("`… auth.app_user_identities …` reaches both schemas over the same client"). It already
    writes `auth.app_user_identities` + `auth.token_revocations` (`:175/:213`). So control can read
    `auth.app_session_anchors` (the refresh-family custody) and `auth.oauth_grants`/the family marker
    over the shared connection. Anchor `refresh_token_enc` is encrypted with
    `zeroship_core::crypto::{derive_key,encrypt,decrypt}` (`anchors.rs:15`, `core/crypto.rs:61/73/87`) off
    a master secret — control decrypts it with the **same shared master** the platform already shares for
    `PAIRWISE_SALT` (AGENTS.md: "the SAME `PAIRWISE_SALT` value must be configured on gateway + control").
    `crates/control/src/oidc_rp.rs` already speaks Hydra (token exchange / refresh).
  - `crates/core/src/auth.rs:256` (`verify_zeroship_user_header_for_request`) + `oidc_rp.rs:1115-1135`
    (`encode_user_header` → `sign_zeroship_user_header`) — **the `ZeroShip-User` header is HMAC-signed
    with `worker_key`, bound to the request id, and time-bound.** Only the gateway can mint it. This is
    the stateless credential the power-token mint trusts to re-derive the user (§3.2), so **no
    per-request auth-context registry is needed** in either the gateway or control.
  - `sdks/auth/src/{client.ts,internal/transport.ts}` — `AuthClientImpl` holds the session in
    `CacheManager`, exposes `getAccessToken()`/`getAccessTokenWithPopup()`, refreshes under
    `RefreshLock`/`navigator.locks` against `/session?mint=1`, persists in `InMemoryCache` /
    `LocalStorageCache`. The token-holding half is **removed**.
  - `sdks/auth/src/server.ts` — `@zeroship/auth/server` today exposes `getUser`/`requireUser`/
    `isLoggedIn` and **deliberately has NO server-side `signOut`** (lines 74-79: the gateway owns the
    `Set-Cookie` on `POST /__zs/auth/signout`; a worker handler cannot emit it). This redesign
    **respects that decision** — see §3.2.
  - `crates/control/src/authz_guard.rs` — `AuthzGuard` ALREADY authenticates OAuth callers: it
    introspects the Bearer at Hydra (`hydra_introspector.introspect`, line 215), checks `result.aud`
    against `state.expected_oauth_audience` (lines 225-230, else `wrong_audience`), and resolves a
    principal (plus a PAT path). It carries an `mfa_verified` field that is **hardcoded `false` at
    every construction site** (lines 108, 181, 248) — there is no path that sets it true today.
  - `crates/auth/src/ui/login.rs` — the login UI hardcodes `amr: vec!["pwd"]` / `acr:
    Some("urn:zeroship:pwd")` (lines 426-427, 453-454). **No path stamps `amr` containing `"mfa"`.**
    Step-up that requires `amr=mfa` cannot be satisfied until MFA factors land (see §5.3).
  - `crates/control/src/oauth_grants_handlers.rs` — `control.oauth_grants` is the grant ledger
    (`GET/DELETE /me/oauth-grants`, revoke cascade). **Stays** as the source of truth for consented
    scopes.
  - `apps/zeroship-builder/src/server/{oauth.ts,session.ts}` — the console's bespoke confidential RP:
    PKCE, `exchangeCode`/`refreshAccessToken` against Hydra directly, `BUILDER_SCOPES` (a single
    broad set: `apps:*`, `env:*`, `secrets:*`, `deployments:*`), an HMAC-signed `__zs_builder_oauth_user`
    cookie carrying the bare user id. **Replaced** by this model.

---

## 1. The model + threat model

### 1.1 What moves

| Artifact | Original build | **This redesign (BFF)** |
| --- | --- | --- |
| **Browser holds** | Wrapper **access token** (capability: `scope`, `client_id`, `aud`, Bearer-armed) | **No JWT at all** (per §10-D1 HttpOnly-only). Identity is the `{ user, expires_at, auth_time, amr }` projection from `GET /__zs/auth/session`. The `zs-id+jwt` is signed but held **server-side**. **No scopes. No capability.** |
| **SPA → own-app auth** | `Authorization: Bearer <wrapper>` (client-attached) | **HttpOnly `__Host-zs_app_session` cookie** (the `gateway_sessions` store the live cookie arm already reads), validated server-side; gateway injects `ZeroShip-User`. No client-held bearer. The `__Host-zs_app_anchor` (Strict) stays **reload-recovery only** — NOT a live request credential. |
| **Power (access/capability) token** | Minted to the browser at `/token` & `/session?mint=1` | **Never sent to the browser.** Exchanged + held **server-side** — minted in the **control plane** (§3.2) from the server-held grant + rotating refresh family (under `app_session_anchors`). The control plane is the token-custody BFF tier; the worker/console only ever proxies the token, never custodies it. |
| **Consented scopes** | `control.oauth_grants` (already) | **Unchanged** — `control.oauth_grants`, platform-level, queryable, revocable. |
| **Authz decision** | Gateway Bearer arm checks the wrapper `scope` claim; client also gated UX | **Server-side, per-operation, at the resource server** (control plane for the console; worker/app for app ops). Client gating is UX-only. |

### 1.2 Why identity-to-browser + power-server-side (XSS blast-radius collapse)

The original `§8.5` trust model said the quiet part out loud: on `{app}.zeroship.ai` the app is
**arbitrary, AI-generated, first-party JS**, so a client-held session was "fully trusted with its own
user's tokens" — an XSS bug fully compromised that app's session, "the accepted cost of true Supabase
parity." The wrapper's per-app `pws_`/alias bounded the blast radius to one app, but the wrapper was
still a **live capability** sitting in JS-reachable memory for its 10-minute TTL.

This redesign removes the capability from the browser entirely. Under the IETF BCP that is the move
from the *weakest* acceptable tier (browser-based client / in-memory token) to the *strongest* (BFF):

- **XSS can no longer steal a power token, because there is none in the browser.** The identity token
  carries no `scope` and is not accepted by any resource server as authorization. An XSS payload can
  read it and learn *who* the user is on *this* app (the `pws_` + alias — already non-correlatable
  across apps), but it cannot *do* anything with the user's authority that the app's own server
  doesn't already do on its behalf.
- **The HttpOnly session cookie is unreadable by XSS.** SPA→own-app requests authenticate via the
  interactive `__Host-zs_app_session` cookie (the `gateway_sessions` store — see §2.3), server-validated;
  an XSS payload running in the page can ride the cookie (it is same-origin — this is the residual
  CSRF-class risk the BFF trade accepts), but it cannot *exfiltrate* a credential to use elsewhere or
  after the page closes. The blast radius is "actions while the malicious script runs in this tab," not
  "a stealable bearer good for 10 minutes anywhere."
- **The power token's exposure surface is the server**, where the platform — not arbitrary creator JS
  — is the only code in the token boundary. Custody, rotation, and audience-scoping are enforced by
  Rust, not by trusting an npm cache.

<!-- Revised in round 1: addressing CSRF-posture-transfer claim (minor). The same_origin_guard
     lives on /token and /signout, NOT on the app dispatch path; we cannot claim it "transfers
     unchanged." We must specify anti-CSRF for the dispatch path explicitly. -->
**The cost (per the BCP) is CSRF + session-management duty, and it is NOT free — it requires new work on
the app dispatch path.** Two distinct surfaces need anti-CSRF, and they are not the same surface today:

1. **The auth endpoints** (`POST /__zs/auth/token`, `POST /__zs/auth/signout`) already carry
   `same_origin_guard` (`auth_token.rs:177`): `X-ZS-Auth` custom header + exact `Origin` match +
   `Sec-Fetch-Site`. **Unchanged.**
2. **The app dispatch path** (`fetch('/api/…')`, `@zeroship/rpc`) — the path the SPA now authenticates
   purely by cookie. Normal SPA `fetch` to `/api/*` does **not** set `X-ZS-Auth` today, so the
   endpoint-level guard does NOT cover it. `SameSite=Lax` on `__Host-zs_app_session` blocks cross-site
   top-level-GET-driven CSRF, but `Lax` still permits same-site requests, and a state-changing
   `POST /api/*` ridden by an XSS payload is same-site.

   <!-- Rewritten in round 2: addressing minor #8. The round-1 "Sec-Fetch-Site OR a custom SDK header"
        was under-specified and would BREAK raw fetch('/api/..') in non-SDK / raw-JS deploys (the
        zs-standard contract supports these; they do NOT use the @zeroship transport and so never set the
        custom header). Make the PRIMARY check Origin-match (which forged cross-origin/<form> requests
        cannot set under CORS and which raw same-origin fetch DOES send on state-changing requests),
        mirroring the existing same_origin_guard semantics (auth_token.rs:233-246: Origin exact-match
        required; Sec-Fetch-Site enforced-when-present, advisory-when-absent). No mandatory custom header. -->
   **New requirement (P3), specified precisely.** The gateway requires, on **state-changing**
   (`POST`/`PUT`/`PATCH`/`DELETE`) **cookie-authenticated** app requests, this exact conjunction (mirroring
   the existing `same_origin_guard` so the two surfaces behave identically):
   - **`Origin` MUST be present and exact-match the app's own origin** (`https://{app}.zeroship.ai`). This
     is the binding check: under CORS a cross-origin page (or a `<form>` POST) cannot set `Origin` to the
     target's value; a same-origin XHR/`fetch`/RPC always sends the correct `Origin` on state-changing
     requests. **A raw same-origin `fetch('/api/..', {method:'POST'})` from non-SDK / raw-JS app code
     passes** — it sends `Origin` automatically — so this does NOT break the `zs-standard` raw-JS deploys.
   - **`Sec-Fetch-Site`, when present, MUST be `same-origin`** (enforced-when-present, advisory-when-absent
     — exactly the existing guard's posture, so older browsers that omit it are not falsely rejected; the
     `Origin` check carries the defense there).
   - **No custom SDK header is required.** (The `@zeroship` transport MAY additionally send `X-ZS-Request: 1`
     as defense-in-depth, but the gate does NOT require it — requiring it would reject hand-written
     same-origin `fetch` in raw-JS apps, which the deploy contract supports.)
   - Safe (idempotent) `GET`/`HEAD` requests are exempt.
   - **Failure mode for non-SDK callers:** a same-origin raw `fetch` POST → **allowed** (correct `Origin`).
     A cross-origin/`<form>`-driven POST → **403** (wrong/forged `Origin`). An old browser omitting both
     `Origin` and `Sec-Fetch-Site` on a state-changing request → **403 on the missing `Origin`** (the
     guard requires `Origin` present on mutations, as `same_origin_guard` already does at
     `auth_token.rs:~225`). This is the standard Origin-based CSRF defense the IETF BCP calls for on a BFF
     dispatch path. It is **net-new gateway logic** on the dispatch path (the existing `same_origin_guard`
     is wired only to `/token`/`/signout`), and is listed in ADD.

### 1.3 The token taxonomy + the three session stores (what is which, addressed to whom)

<!-- Revised in round 1: addressing BLOCKER #1. The original draft conflated three real stores into
     two and falsely claimed the live cookie arm reads the anchor. Naming all three precisely. -->

There are **two browser-facing JWT-shaped artifacts** and **three distinct server-side session stores**.
The redesign's whole point is that of the two JWT artifacts, only the inert one (identity) is ever in the
browser:

**Browser-facing artifacts:**

1. **Identity token** (NEW shape — authentication). `aud == client_id` (per Auth0's ID-token rule),
   `sub == pws_`, carries `email`(alias)/`email_verified`/`name`/`iss`/`exp`/`auth_time`/`amr`. **No
   `scope`.** Defines what the gateway signs to assert "who am I / am I logged in." Per the §10-D1
   HttpOnly-only decision it is **held server-side** (keyed to the gateway session), not handed to the
   browser; the SPA reads the equivalent `{ user, … }` projection. Never accepted as authorization by
   any resource server.
2. **Power token** (the access/capability token — authorization). **Audience-scoped** (`aud`-narrowed),
   short-lived, `scope`-bearing. Minted **server-side in the control plane** from the server-held grant +
   refresh family (§3.2). **Never in the browser.** Least-privilege is enforced by the mint ceiling +
   per-op `require`, not necessarily by a narrowed `scope` claim — for v1's real-Hydra tokens the `scope`
   claim is the full consented grant (Hydra returns the full scope on the refresh grant; §3.4). This is
   the artifact the original build wrongly shipped to the SPA as the "wrapper access token."

**The three server-side session stores (all real, all distinct in the code — do NOT conflate):**

| Store | Cookie | SameSite / lifetime | Role today | Role after redesign |
| --- | --- | --- | --- | --- |
| **`auth.gateway_sessions`** (`sessions.rs`) | `__Host-zs_app_session` | **Lax**, 12h / 30-min idle | The **live request cookie arm** (`resolve_app_session_user_header_inner`, `router/auth.rs:1520`) reads THIS via `sessions::validate`. Today only the interactive server-rendered `/__zs/auth/callback` (`dispatch.rs:1366`) creates it; the SDK popup flow does NOT. | **The SPA's live request credential.** The SDK popup `/token` flow now ALSO creates a `gateway_sessions` row + sets this cookie, so the existing live cookie arm authenticates SPA requests **with no new arm**. |
| **`auth.app_session_anchors`** (`anchors.rs`) | `__Host-zs_app_anchor` | **Strict**, 30d, no idle | Reload-recovery only: holds the encrypted server-held refresh family + a single cached-wrapper slot; read ONLY at `/__zs/auth/session?mint=1` to re-mint the browser wrapper. `anchors.rs:62-68` asserts it must NEVER be cross-validated against `gateway_sessions`. | **Reload-recovery anchor (gateway-written) + the BFF refresh-family custody store the CONTROL PLANE reads for the power-token mint** (§3.2 / §3.1). The single cached-wrapper slot is dropped; the per-`(audience,scopes)` cache moves to `auth.app_power_token_cache`. Still NOT a live request credential; the two-store invariant holds. <!-- Revised round 2: control reads/rotates it for the mint (BLOCKER #1 locus). --> |
| **Wrapper / raw-Hydra Bearer + DPoP arm** (`router/auth.rs:1090`, `dpop_exchange.rs`) | none (header-borne) | token `exp` | Authenticates **non-browser** OAuth clients: gateway wrappers (`iss == public_url`) and raw-Hydra Bearers (`iss != public_url`); DPoP-bound wrappers from `/dpop-exchange`. | **Unchanged for non-browser clients** (CLI, server-to-server). The SPA stops using it entirely. v1's server-side power tokens are real Hydra tokens (introspection path), NOT wrappers; the wrapper arm survives for `/dpop-exchange` (the internal-audience wrapper format is deferred — §3.4 / §7). <!-- Revised round 2: internal-wrapper deferred. --> |

The **two stores must never cross-validate** (`anchors.rs` test at `:583-588`: presenting a
`__Host-zs_app_session` value must NOT resolve as an anchor, and vice-versa). The redesign keeps that
invariant: the live cookie arm reads `gateway_sessions`; the anchor is read only on reload-recovery.

---

## 2. What the browser gets: identity (no JWT) + own-app auth

### 2.1 The identity-token shape

The popup/consent flow ends by establishing identity — **not** by handing the wrapper access token to
the browser. The identity assertion is a gateway-signed JWT (reuse the gateway ed25519 wrapper-signing
key + kid rotation/overlap story from the original `§1.2`, so it is independently verifiable and
rotatable — but it is a *distinct token type*, stamped `"typ": "zs-id+jwt"`, so it can never be confused
with a power token at any verifier). **Per the §10-D1 HttpOnly-only decision this JWT is held
server-side** (keyed to the gateway session); the browser sees only the `{ user, … }` projection. The
shape below is what the gateway signs and stores:

```json
{
  "iss":  "<gateway issuer>",
  "aud":  "oac_<base62>",          // the app's client_id — per Auth0's ID-token rule (aud == client_id)
  "sub":  "pws_…",                 // per-app pairwise subject; NEVER the global usr_ UUID
  "email":"{token}@{relay_domain}",// relay alias, never the real inbox; null if email scope not granted
  "email_verified": true,
  "name": "…",
  "typ":  "zs-id+jwt",             // identity token type tag — rejected by every power-token verifier
  "iat":  …, "exp": …,             // short (e.g. 10 min), refreshed via the anchor; jti for diagnostics
  "auth_time": …                   // for step-up freshness checks (§5.3)
}
```

**Deliberately absent:** `scope`, `client_id` *as a binding claim* (the SPA is not a resource-server
client), `cnf`/DPoP, and any `wraps`/raw-Hydra lineage. The token is inert: it answers *who*, full stop.

<!-- Revised in round 1: addressing minor #1. The existing at+jwt typ gate ALREADY rejects zs-id+jwt
     on the wrapper path — no new "disjoint typ verifier" is needed. And the Anon-route behavior must
     be stated precisely (Invalid Bearer ⇒ anonymous, not flat 401). -->
**Type separation is load-bearing — and it is ALREADY enforced by the existing code, no new verifier
required.** The gateway's wrapper verifier (`wrapper_token.rs::Verifier::verify`, line 433) hard-checks
`header.typ == "at+jwt"` (RFC 9068). A token stamped `typ: "zs-id+jwt"` routed to the wrapper path
(`iss == public_url`, `router/auth.rs:1122`) fails that typ gate → `BearerOutcome::Invalid`. So the
identity token is structurally non-authorizing **via the typ check that exists today** — we do NOT add a
"disjoint typ verifier"; we rely on the `at+jwt` gate already rejecting anything that is not an access
token. Concretely, the only new wiring is that the identity minter stamps `zs-id+jwt` (so it is never
mistaken for `at+jwt`) and the gateway's *identity* verification path (used by `/session`) accepts ONLY
`zs-id+jwt`.

**The 401-vs-anonymous behavior is policy-dependent (be precise):**

- On a `User`/`Admin` route, an `Invalid` Bearer → `401` (`router/auth.rs:278`). So an identity token
  presented as `Authorization: Bearer` on a protected route fails → `401`. ✔
- On an `Anon` (public) route, an `Invalid` Bearer is served **anonymously** (`user_header: None`,
  `router/auth.rs:262`), not `401`. So an id-token-as-Bearer on a public page is silently treated as
  not-logged-in — which is correct (a public page renders for anonymous users), but it is NOT a flat
  `401`. Either way the identity token never authorizes anything; that is the load-bearing property.

This is the structural enforcement of Auth0's "do not authorize with an ID token": even if app JS
attaches the identity token as a bearer, no resource server honors it.

### 2.2 How it is issued (the flow change)

The interactive popup + PKCE + consent flow is **unchanged through the code exchange** (popup → top-level
nav to `auth.zeroship.ai` → Hydra login/consent → `/__zs/auth/popup-callback` relay → `postMessage` →
`exchangeCodeForSession`). What changes is the **terminal step at `/__zs/auth/token`**:

`POST /__zs/auth/token` (rewritten — `auth_token.rs::token`):
1. Same-origin guard (`X-ZS-Auth` + exact `Origin` + `Sec-Fetch-Site`) — **unchanged**.
2. Code→token exchange against Hydra (`exchange_code_public`) — **unchanged**.
3. Validate the id_token (sig/iss/`aud == client_id`/exp, `verify_id_token`) — **unchanged**.
4. Encrypt + store the rotating refresh family under a new `auth.app_session_anchors` row; set the
   HttpOnly `__Host-zs_app_anchor` cookie + the breadcrumb — **unchanged** (this is the BFF refresh-family
   custody; the anchor remains reload-recovery only).
5. **NEW — create a `gateway_sessions` row + set `__Host-zs_app_session`.** This is the addition that
   gives the SPA a *live request credential* on the store the live cookie arm already reads. Call
   `sessions::create(&conn, &NewSession { … })` (the same call the interactive `/__zs/auth/callback` makes
   at `dispatch.rs:1366`) and emit `set_app_session_cookie` (`__Host-zs_app_session`, `SameSite=Lax`,
   12h). The actual `NewSession` shape (`sessions.rs:38-47`) takes `user_id: &str` (the **global UUID
   string**; `create` parses it to a `Uuid` internally, `sessions.rs:65`, since `gateway_sessions.user_id`
   is `UUID`), `email`/`name`/`avatar_url` as `Option<&str>`, plus `email_verified` and
   `granted_scopes: &[String]` (the consent grant set):
   ```rust
   sessions::create(&conn, &sessions::NewSession {
       user_id: &global_uuid.to_string(),     // &str; parsed to UUID in create()
       app_id: &route.client_id_app_id,       // the app_id the cookie arm validates against
       email: claims.email.as_deref(),        // Option<&str> — REAL email; relay-swapped only on read (§2.3)
       name: claims.name.as_deref(),
       avatar_url: claims.picture.as_deref(),
       email_verified: claims.email_verified,
       granted_scopes: &granted_scopes,        // the consent grant set
       // auth_time + amr — NEW columns (ADD §7); see step 5b. NewSession gains these two fields.
       auth_time: claims.auth_time,            // from the validated id_token
       amr: &claims.amr,                       // from the validated id_token (e.g. ["pwd"])
   })
   ```
   <!-- Added in round 1: addressing BLOCKER #1. The SDK popup flow previously created ONLY an anchor;
        the SPA rode a browser wrapper for live requests. With the wrapper gone, the SPA needs a live
        credential, and the existing cookie arm reads gateway_sessions — so we create that row here. -->
   <!-- Revised in round 2: addressing minor #7 (real NewSession shape: user_id: &str, Option<&str>
        fields, granted_scopes: &[String]) AND BLOCKER #3 (auth_time/amr are NOT on gateway_sessions
        today — they are added as columns + NewSession fields, see step 5b + ADD §7). -->
5b. **NEW (schema, ADD §7) — carry `auth_time` + `amr` onto `gateway_sessions`.** Verified: today
   `auth.gateway_sessions` (`0002_auth.sql:195-209` + `0008`) has NO `auth_time` and NO `amr` columns —
   they exist only on `auth.users`/`auth.sessions` (`0002_auth.sql:45-47`), which the gateway path never
   reads. The step-up gate (§5.3) needs a fresh `auth_time`, and the SPA reads `auth_time`/`amr` via the
   projection (§2.3), so both must live on the row the gateway path actually loads. **Add two columns**
   (`auth_time TIMESTAMPTZ`, `amr TEXT[] NOT NULL DEFAULT '{}'`) to `auth.gateway_sessions` and the
   corresponding `NewSession` fields + `validate()`/`AppSession` reads. They are populated here from the
   **validated id_token claims** (`verify_id_token` already runs at step 3; `auth_time`/`amr` are standard
   OIDC claims Hydra issues). Pre-launch: this is a new changeset, not an ALTER-with-backfill.
6. **CHANGED:** derive `pws_` + relay alias (unchanged helpers `pairwise_sub` / `relay_alias_for`) and
   **mint an *identity token* (`zs-id+jwt`)**, NOT a power wrapper.

**Response — recommended (HttpOnly-only identity, per §10 Q1, now decided):** the identity token is
**NOT returned in the body**; the browser holds zero JWTs. The SPA learns identity via the `{ user }`
projection (readable from `GET /__zs/auth/session`). The two cookies are the SPA's only credentials.

```
200 OK
Cache-Control: no-store
Set-Cookie: __Host-zs_app_session=<gw_session_id>; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=43200
Set-Cookie: __Host-zs_app_anchor=<anchor_id>; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=2592000
Set-Cookie: zs.<app_ref>.is.authenticated=true; SameSite=Lax; …          (breadcrumb)

{ "user": { "id":"pws_…", "email":"…@{relay_domain}", "name":"…",
            "avatar":null, "email_verified":true },
  "expires_at": <gateway_session exp> }
```

**There is no `access_token` field and no `scope` field — and (per the Q1 decision) no `id_token`
field either.** The browser never receives a power token, and now holds no JWT at all — only the user
projection + two HttpOnly cookies. The `zs-id+jwt` is still minted (it is the signed assertion the
gateway stamps to carry `auth_time`/`amr` for step-up freshness), but it is held **server-side** keyed
to the gateway session; the SPA reads `auth_time`/`amr` via the `{ user }` projection when it needs them
for step-up UX. (Compare today's body, `auth_token.rs:519` `"access_token": wrapper`.)

> **Why HttpOnly-only and not token-in-body:** §10 Q1 weighed the two. Returning the id token in the
> body lets the SPA decode `exp`/`auth_time` without a round-trip, but it is strictly weaker (a JWT in
> JS-reachable memory, even an inert one, is one more thing XSS can read and one more shape to confuse).
> The strictest BFF reading is "no tokens in the browser at all." We take that. The cost — no
> client-decodable `exp`/`auth_time` — is absorbed by surfacing `expires_at`/`auth_time`/`amr` in the
> `{ user, expires_at }` projection. §2.1's token *shape* still defines what the gateway signs and holds;
> it is simply not handed to the browser.

`GET /__zs/auth/session` (rewritten):
- Reads the `__Host-zs_app_session` cookie first and returns the **identity** projection
  (`{ user, expires_at, auth_time, amr }`) from the live `gateway_sessions` row. `auth_time`/`amr` come
  from the **new `gateway_sessions.auth_time`/`amr` columns** (§2.2 step 5b) — they are NOT re-derived and
  NOT read from `auth.users` (the gateway path never touches that table). If the gateway session is
  absent/expired but the `__Host-zs_app_anchor` is valid (reload-recovery), it runs the anchor refresh
  path: rotate the server-held family (existing per-node single-flight + cached-token short-circuit),
  **re-create a fresh `gateway_sessions` row + re-set `__Host-zs_app_session`**, mint a fresh server-held
  `zs-id+jwt`, and return the projection. `?mint=1` forces that refresh.

  <!-- Added in round 2: addressing MAJOR #5 — the rewritten /session sources identity from the
       gateway_sessions row, whose `email` column holds the REAL inbox (CITEXT, written from claims.email).
       If /session returned session.email directly it would LEAK the real address. The cookie arm and the
       current /token//session handlers all relay-swap (router/auth.rs:1585-1587, auth_token.rs:434/618);
       the new read path MUST do the same, fail-closed. -->
  - **MANDATORY relay-email swap on this read path (privacy invariant, fail-closed).** The
    `gateway_sessions.email` column stores the **real** inbox (`CITEXT`, written from `claims.email` at
    session-create, `sessions.rs:25/70`). The SPA-facing `{ user }` body MUST NEVER contain it. The
    `/session` handler resolves the per-app `pws_` + relay alias exactly as the cookie arm does — via
    `project_pairwise` / `relay_alias_for` (`router/auth.rs:1585-1587`; the current handlers at
    `auth_token.rs:434`/`:618`) — and emits the **relay alias** as `user.email`, or **empty string on no
    active alias** (fail closed; never the real address). The same swap applies to the `/token` `{ user }`
    body (§2.2 response). Regression test (P2/P3): assert the `/session` and `/token` `{ user }` bodies
    never contain `claims.email` / `session.email`, and emit `""` when no relay alias is active.
- The `do_refresh` path is unchanged *except* it (a) no longer reshapes the rotated token into a
  browser wrapper, and (b) re-establishes the gateway session (carrying `auth_time`/`amr` forward onto the
  re-created row from the rotated id_token claims). The rotated raw Hydra access JWT stays server-side (it
  is now used only by §3's server exchange). `expires_at` describes the **gateway session** lifetime (when
  the SPA should re-probe / the anchor will silently re-establish it), not a capability lifetime. **No JWT
  is ever in any SPA-facing body.**

### 2.3 How the SPA authenticates its own app requests

<!-- Rewritten in round 1: addressing BLOCKER #1. The original draft claimed the SPA rides the anchor
     cookie via the "existing cookie arm keyed on the anchor store." FALSE: the existing cookie arm
     reads gateway_sessions, a different table/cookie/SameSite. We do NOT promote the Strict anchor to
     a live credential. Instead the live request credential is the gateway_sessions cookie (which the
     existing arm already reads), created in the popup /token flow (§2.2 step 5). -->

The SPA does **not** attach a bearer. Its requests to its own app (`fetch('/api/…')`, `@zeroship/rpc`
calls) are same-origin and ride the **HttpOnly `__Host-zs_app_session` cookie** — the `gateway_sessions`
store, which the **existing live cookie arm already reads**:

- **Which cookie the SPA presents on every request:** `__Host-zs_app_session` (Lax, 12h). This is the
  credential the SDK transport relies on the browser to attach automatically (same-origin cookie). The
  SDK attaches **no** `Authorization` header.
- **The arm that authenticates it is unchanged in substance:** `resolve_app_session_user_header_inner`
  (`router/auth.rs:1520`) parses `__Host-zs_app_session`, calls `sessions::validate`, projects the
  per-app `pws_` (`project_pairwise`), reads scopes from the session row's `granted_scopes` (NO
  `control.oauth_grants` join, per changeset 0008), swaps in the relay email, and emits the signed
  `ZeroShip-User`. **We do not need a new arm** — we needed the popup flow to *populate* this store
  (§2.2 step 5), which it now does.
- **The anchor (`__Host-zs_app_anchor`, Strict, 30d) is NOT a live request credential.** It stays
  reload-recovery only, read solely by `GET /__zs/auth/session` to re-establish the gateway session +
  identity after the 12h gateway session lapses. This preserves the `anchors.rs` invariant ("one cookie
  name ⇒ exactly one table"; the two stores are never cross-validated). Per-request reads hit
  `gateway_sessions`, not the anchor — so there is no new per-request anchor read cost.
- **Precedence when both cookies are present (the normal steady state):** the live arm reads
  `__Host-zs_app_session` **only**. `__Host-zs_app_anchor` is ignored on the dispatch path entirely — it
  is consulted only by `/__zs/auth/session` when the gateway session is missing/expired. There is no
  collision to resolve on the hot path because exactly one store (`gateway_sessions`) is consulted there.
- **`gateway_sessions` for SPA apps is KEPT and is now the SPA's primary live credential** (it was
  previously created only for the interactive redirect flow; the popup flow now creates it too). It is
  not removed for SPA apps.

> **Anti-CSRF on this path is NEW (see §1.2 for the precise rule):** because the SPA now authenticates
> state-changing `/api/*` and RPC requests by `SameSite=Lax` cookie alone, the gateway requires, on
> non-idempotent cookie-authenticated app requests, **`Origin` present + exact-match** (the binding check)
> **and `Sec-Fetch-Site == same-origin` when present** (advisory-when-absent). **No mandatory custom
> header** — a raw same-origin `fetch('/api/..',{method:'POST'})` from a non-SDK / raw-JS deploy passes
> because the browser sends `Origin` automatically; only cross-origin/`<form>`-forged requests are
> rejected. The `same_origin_guard` on `/token` does NOT cover this path; this is added in P3.

`getSession()` / `getUser()` (client SDK) return **identity only**, fetched from `GET /__zs/auth/session`
(no client-held JWT):
```ts
interface Session { user: User; expires_at: number; authTime?: number; amr?: string[]; }
                    // NO access_token, NO token_type, NO id_token
interface User { id: string /* pws_ */; email: string|null; emailVerified: boolean;
                 name: string|null; avatar: string|null; }  // NO scopes on the client surface
```
`getAccessToken()`, `getAccessTokenWithPopup()`, `Session.access_token`, `Session.scopes`, and
`User.scopes` **are removed from the client surface** (see §6). The SPA cannot obtain a power token; it
asks its own server to act on its behalf, and the server holds the power token.

---

## 3. The server-side power-token exchange

The power token is exchanged + held **server-side**, minted in the **control plane** (§3.2) from the
server-held grant + rotating refresh family, surfaced via a **JS server SDK** (`@zeroship/auth/server`).
Consumers: **the console calling the control plane** (the v1 case, §3.3), and — once non-control external
audiences land — a worker/app calling an external resource with the user's scopes. (An app calling its
**own** worker needs no power token: it already has the request's authenticated `ZeroShip-User`, §3.4.)

### 3.1 The BFF custody store (reuse `app_session_anchors`) + a per-audience token cache

The original `auth.app_session_anchors` already holds, per `(app, global_user)`: the **encrypted
server-held rotating refresh family** (`refresh_token_enc`, AAD-bound to `(client_id, sub)`), the
`refresh_family_id` lineage, `granted_scopes`, and a **single** cached-token column pair
(`cached_access_token` / `cached_access_exp`, `anchors.rs:160-161`) under a hardcoded 5-second window
(`ANCHOR_MINT_CACHE_TTL_SECS`, sized for reload-storm coalescing of one browser wrapper). This is the
BFF's server-side refresh-family custody store. The redesign keeps the custody mechanics (encrypt,
single-flight rotation, no DB conn held across Hydra, 30-day absolute / Hydra-`invalid_grant`
termination).

<!-- Revised in round 1: addressing minor #2. The single cached_access_token column CANNOT hold a
     per-(audience,scopes) map, and the 5s TTL is wrong for server-side power tokens. This is a real
     schema change, listed in ADD — not a reuse of the existing column. -->
**New cache structure (schema change, listed in ADD).** A server-side power token is keyed by
`(anchor_id, audience, scopes-hash)`, not by anchor alone — one anchor may hold tokens for several
audiences (the control plane, the app's own worker, distinct external APIs). The single
`cached_access_token` column cannot represent that. Add a **new table** `auth.app_power_token_cache`:

```
auth.app_power_token_cache(
  anchor_id      uuid       references auth.app_session_anchors(id) on delete cascade,
  audience       text       not null,
  scopes_hash    bytea      not null,   -- sha256 of the sorted least-privilege scope set
  token_enc      bytea      not null,   -- AES-256-GCM (reuse zeroship_core::crypto), AAD-bound to (anchor_id, audience)
  expires_at     timestamptz not null,
  primary key (anchor_id, audience, scopes_hash)
)
```

**TTL is bounded by the power token's own `exp`, not 5s.** The 5-second window exists to coalesce a
reload *storm* (many tabs re-minting one browser wrapper at once); server-side power tokens amortize
Hydra round-trips across many app requests over the token's full life, so a 5s TTL would force a Hydra
down-scope on nearly every call. The cache entry lives until `expires_at` (minus a small skew), then a
fresh down-scope replaces it. (This table is written + read by the **control plane** — where the mint now
runs, §3.2 — over the shared `auth` schema; it is encrypted with the shared crypto master.)

<!-- Revised in round 2: addressing minor #9. Enumerate the FULL no-back-compat cascade of dropping
     cached_access_token/cached_access_exp, and state how reload-storm coalescing is preserved under the
     new per-(audience,scopes-hash) cache. -->
**Dropping `cached_access_token`/`cached_access_exp` — the full same-PR cascade (no-back-compat).** These
two columns are the SPA browser-wrapper cache; with the browser wrapper gone they are unused by every
surviving path, so they are removed. Per the no-back-compat rule, **every producer/consumer changes in
the same PR**, verified against the code:
- **`Anchor` struct fields** `cached_access_token` / `cached_access_exp` (`anchors.rs:160-161`) — removed.
- **`NewAnchor` field** `cached_access_token` (`anchors.rs:178`) and its bind in **`create`**
  (`anchors.rs:196-211`) — removed (the INSERT column list + the `&params.cached_access_token` bind).
- **`update_minted`** (`anchors.rs:251-277`) — the entire function exists only to write
  `cached_access_token`/`cached_access_exp` after a `?mint=1`; it is **deleted** (the server-side cache is
  now `app_power_token_cache`, written by control).
- **`row_to_anchor`** (`anchors.rs:355-360`) — the two `row.get("cached_access_*")` reads removed.
- **`mint` short-circuit** (`auth_token.rs:683-694`) — the "serve the in-window cached WRAPPER" block that
  reads `anchor.cached_access_token` is **deleted** (there is no browser wrapper to serve from cache).
- **Tests** that assert the cache window (e.g. the `cache_ttl` / `ANCHOR_MINT_CACHE_TTL_SECS` tests in
  `anchors.rs`/`auth_token.rs`) — updated/removed in the same PR. `ANCHOR_MINT_CACHE_TTL_SECS` itself is
  removed (its only consumer was the browser-wrapper cache).
- **Reload-storm coalescing is preserved by a DIFFERENT mechanism, not lost.** The 5s window coalesced
  many tabs re-minting **one browser wrapper** at once. After the redesign the browser does **not** mint
  anything — reloads hit `GET /__zs/auth/session`, whose anchor reload-recovery already runs the
  **existing per-node single-flight on the refresh-family rotation** (`mint`'s single-flight, kept). So a
  reload storm now coalesces at the **family-rotation single-flight** (one Hydra refresh per node per
  anchor in flight), not at a cached-column window — strictly the same coalescing guarantee, on the path
  that still exists. The **server-side power-token** path additionally coalesces via the
  per-`(anchor,audience,scopes-hash)` `app_power_token_cache` (a cache hit avoids Hydra entirely for the
  token's full life), which is a *stronger* amortization than the 5s window it replaces. No coalescing
  regression.

### 3.2 Server-side power-exchange — the control-plane mint + the worker→control path

<!-- Rewritten in round 2: addressing BLOCKER #1 (no worker→gateway link exists), BLOCKER #2 (no
     per-request auth-context registry exists), BLOCKER #6 (worker→gateway re-entrancy into the in-flight
     dispatch), and the "async HTTP-op in the synchronous env.auth V8 plugin" piece. Three corrections:
     (1) the mint runs in the CONTROL PLANE — the only service the worker already reaches (control_url +
     control_key, internal.rs) — NOT the gateway (the worker has no gateway URL and would re-enter the
     in-flight dispatch). (2) getAccessToken is a JS-SDK fetch(), not a new native env.auth op (the plugin
     is synchronous; getAccessToken must be async). (3) the user is re-derived from the ECHOED SIGNED
     ZeroShip-User header (HMAC+request-id+time-bound, gateway-minted) — verified statelessly — so NO
     per-request registry is needed and a worker can never name a different user. -->

**`env.auth` is a LIVE registered namespace** (`crates/runtime/src/auth.rs`; `AuthPlugin` pushed
unconditionally in `worker/cache.rs:52`), today exposing `getUser()`/`requireUser()` as **synchronous**
V8 callbacks that read the request's user from in-process `RuntimeState` (`auth.rs:95/123`). There is
**no async-op (Promise-returning) machinery in this plugin.** `getAccessToken` is a network round-trip,
so it **cannot** be a native `env.auth` op without inventing async-op plumbing the plugin does not have.
Therefore `getAccessToken`/`fetchAs` are implemented in the **JS server SDK** (`@zeroship/auth/server`)
as a `fetch()` to the mint endpoint — the runtime already exposes WinterCG `fetch`. The native plugin
stays synchronous and is left untouched except for identity reads. Existing surface it must not break:
`getUser`/`requireUser` and their error mapping (`requireUser` throws → 401 at dispatch). Per the
pre-launch no-back-compat stance, the SDK and every consumer change in one patch.

**Where the mint runs — the control plane, not the gateway, and not the worker (resolves §10 Q3,
re-decided in round 2).** Custody and reach were audited directly against the code:

| Service | Has anchor + refresh family custody? | Reachable from the worker? | Re-entrancy risk? |
| --- | --- | --- | --- |
| **Worker** | No (`WorkerConfig` = `control_url`/`control_key`/`db_url` only) | n/a (it is the caller) | n/a |
| **Gateway** | Yes (`auth.app_session_anchors`, `wrapper_issuer`) | **No** — the worker has no gateway URL; the only link is **gateway→worker** dispatch | **Yes** — a worker→gateway call lands on the gateway's single public listener *while it is awaiting the very dispatch that triggered it* (`forward_dispatch`), re-entering the per-thread compio pool mid-request |
| **Control plane** | Reaches `auth.*` over the shared DB client (`relay_revoke.rs:16`); can decrypt `refresh_token_enc` with the shared master; `oidc_rp` speaks Hydra | **Yes** — the worker already calls control internal endpoints with `control_key` (`internal.rs`) | **None** — control is a separate service on a separate listener; no in-flight dispatch to re-enter |

The gateway has the custody but is **unreachable from the worker** and would create a re-entrant call
into its own in-flight dispatch (BLOCKER #6). The control plane has **none of those problems**: the
worker→control internal channel already exists, control reaches the `auth` schema (so it reads the
anchor's refresh family + the family marker + the grant ceiling) and decrypts the family with the shared
crypto master, and control's `oidc_rp` already talks to Hydra for the external-audience down-scope (§3.4).
**The mint runs in the control plane.** The cost — control now reads `auth.app_session_anchors`, a table
the gateway also writes — is a deliberate shared-custody widening, called out in ADD; it is bounded
(read-only family decrypt + Hydra refresh; the gateway stays the only **writer** of the anchor cookie /
reload-recovery path) and is far smaller than the net-new worker→gateway link the gateway locus would
require.

**New internal endpoint — `POST /internal/power-token` (control plane, ADD).** A privileged
worker→control surface on the **existing** control internal router (`internal.rs`; `control_key`
shared-secret auth, NOT public — same trust boundary as the env-merge / versions endpoints the worker
already calls).

```
POST /internal/power-token                  (control internal router; control_key-gated)
Authorization: Bearer <control_key>          -- the EXISTING worker↔control shared secret (internal.rs
                                                check_auth); rotate via config
ZeroShip-User: <the SIGNED header the gateway minted for THIS request>
                                             -- base64(JSON).<request_id>.<iat>.<hmac>, HMAC-signed with
                                                worker_key by the gateway (oidc_rp::encode_user_header).
                                                The worker ECHOES it back verbatim; it does not construct it.
X-ZS-App-Id: <app uuid>                       -- the dispatching app (for the (app, user) anchor lookup)
Content-Type: application/json

{ "audience": "<resource server>", "scopes": ["…"] }

→ 200 { "access_token": "<power token>", "expires_at": <unix>, "scopes": ["…"] }
  403 { "error": "scope_required" | "consent_required" | "step_up_required" }
  401 { "error": "unauthenticated" }    -- ZeroShip-User HMAC invalid / expired / app mismatch
```

**Control re-derives the user from the GATEWAY-SIGNED `ZeroShip-User` header — it does NOT trust a
worker-supplied identity, and it needs NO per-request registry.** The worker passes only `(audience,
scopes)` + the **echoed** `ZeroShip-User` header (the exact value the gateway minted for the in-flight
request) + the app id. That header is `base64(JSON).<request_id>.<iat>.<hmac>`, HMAC-signed with
`worker_key` (`oidc_rp.rs:1115`, `core/auth.rs:256`). Only the gateway can produce a valid one. Control
verifies the HMAC + freshness with the shared `worker_key`
(`verify_zeroship_user_header_parts_at`-equivalent; control already holds `worker_key`, it signs the
worker dispatch bearer with it at `api.rs:670`), recovers the per-app `pws_` + identity from the verified
JSON, and resolves `(global_user_id, client_id)` from the `(app_id, pws_)` anchor row. A compromised
worker cannot ask for a token "as" another user: it cannot forge a `ZeroShip-User` HMAC, and the one it
holds is bound to the current request's authenticated user. **This is why no keyed per-request
auth-context store is needed** (BLOCKER #2): the identity travels as a self-verifying signed token, not
as a request-id the receiver must look up. The request-id inside the signed header bounds replay (it is
the in-flight dispatch's id + an `iat` freshness window); the short token TTL + the
per-`(anchor,audience,scopes-hash)` cache bound the rest; the endpoint is rate-limited per `control_key`.

> **Why the echoed-signed-header binding, not a request-id lookup (BLOCKER #2 detail).** The gateway
> generates `request_id = Uuid::new_v4()` at `dispatch.rs:510` purely as an idempotency / in-flight
> dedupe key and HMAC-encodes the resolved user into `ZeroShip-User` (`encode_user_header`) — it does
> **not** retain the user in any keyed store (`GateState`, `lib.rs:86`, has no per-request user map). So
> "look the user up by request-id" has nothing to look up. Echoing the **signed** header turns the user
> identity into a capability the receiver verifies offline, which is exactly the trust model the worker
> dispatch path already uses (the worker verifies this same header before trusting it,
> `handler.rs:65` / `verify_zeroship_user_header_for_request`). Control reuses that verifier. No new
> state, no registry.

```ts
// @zeroship/auth/server  (the "." export; thin wrapper over the LIVE env.auth.*)
export const auth = {
  // identity (unchanged — backed by the live env.auth.getUser/requireUser → ZeroShip-User)
  getUser(): User | null,
  requireUser(): User,                 // throws → 401 at dispatch
  isLoggedIn(): boolean,
  // NOTE: NO server-side signOut. (See box below — this respects the existing server.ts decision.)

  // NEW — server-side power-token exchange (never returns the token to the client).
  // Implemented in JS (this SDK) as a fetch() to the control-plane mint endpoint — NOT a
  // native env.auth op (the native plugin is synchronous; this is async). See §3.2.
  /** Obtain a scoped, audience-bound, short-lived power token for the CURRENT user,
   *  minted server-side (in the CONTROL PLANE, via the internal endpoint above) from the
   *  server-held grant + refresh family. Audience and scopes are least-privilege: the call
   *  FAILS (not silently broadens) if a requested scope is not in the grant for this app (§4). */
  getAccessToken(opts: {
    audience: string;          // the resource server this token is FOR (required — no broad token)
    scopes: string[];          // least-privilege subset; must be ⊆ the granted scopes (§4)
  }): Promise<{ accessToken: string; expiresAt: number; scopes: string[] }>;

  /** Convenience: a pre-bound fetch that injects the power token server-side for an
   *  outbound call, so app code never handles the raw token at all (BFF-proxy style). */
  fetchAs(opts: { audience: string; scopes: string[] }):
    (input: RequestInfo, init?: RequestInit) => Promise<Response>;
};
```

> **No server-side `signOut` — respecting the documented `server.ts` decision (major #5).** The current
> `sdks/auth/src/server.ts` (lines 74-79) **deliberately omits** a server-side `signOut`: the gateway
> owns the `Set-Cookie` clear on `POST /__zs/auth/signout` (guarded by `X-ZS-Auth` + exact-Origin), and
> a worker handler cannot emit `__Host-` `Set-Cookie` through the gateway proxy. A worker-returned
> `Response` has **no mechanism** to clear the HttpOnly `__Host-zs_app_session` / `__Host-zs_app_anchor`
> cookies (those are gateway-owned). So `signOut` stays **client-only** (`@zeroship/auth/client`
> `client.signOut()` POSTs to `/__zs/auth/signout` with the same-origin guard). This redesign does NOT
> add `auth.signOut(): Response` to the server SDK; that would overturn a correct, documented decision
> and is not implementable as drawn.

<!-- Rewritten in round 2: BLOCKER #1/#2 — the mint runs in CONTROL (the worker reaches it), the user is
     re-derived from the echoed SIGNED ZeroShip-User header (no per-request registry, no gateway link). -->
Mechanics of `getAccessToken` (the worker→control call; the mint runs control-side per the locus above):
1. **App/worker side (JS):** `auth.getAccessToken({audience, scopes})` (the JS SDK) issues a `fetch()`
   `POST <control_url>/internal/power-token` with `Authorization: Bearer <control_key>`, the **echoed
   signed `ZeroShip-User` header** (the value the gateway minted for this request — the worker reads it
   off the inbound dispatch and forwards it verbatim), `X-ZS-App-Id`, and the `(audience, scopes)` body.
   The app never sees the anchor or refresh family. (The `control_url`/`control_key` are surfaced to the
   SDK by the runtime; the worker already holds them.)
2. **Control side — verify + resolve the user from the signed header** (NOT from any worker-named field):
   verify the `ZeroShip-User` HMAC + freshness with the shared `worker_key`; recover the per-app `pws_` +
   identity from the verified JSON; look up the `(X-ZS-App-Id, pws_)` anchor row to resolve
   `(global_user_id, client_id)`. A bad/expired/forged header → `401 unauthenticated`.
3. **Control side — enforce the grant ceiling** (§4): the scope ceiling is the **denormalized
   `anchor.granted_scopes`** (read off the anchor row control just loaded — consistent with the
   no-hot-path-join design); reject (`scope_required`/`consent_required`) if `opts.scopes ⊄ ceiling`. See
   §4 for the revoke-freshness reconciliation (a revoke deletes the anchor's refresh family + stamps the
   family marker, so a stale ceiling cannot mint).
4. **Control side — cache check:** look up `auth.app_power_token_cache` by `(anchor_id, audience,
   sha256(sorted scopes))`; if a non-expired entry exists, decrypt + return it (no Hydra round-trip).
5. **Control side — mint** a power token **audience-scoped** to `opts.audience` and `scope`-limited to
   `opts.scopes`, short-lived, from the server-held refresh family (see §3.4 — both v1 audience classes
   are **real Hydra down-scoped tokens** minted via control's `oidc_rp`; the internal-audience wrapper
   format is deferred because the wrapper issuer lives in the gateway, not control). Decrypt the family
   with the shared crypto master, rotate it server-side (single-flight), re-encrypt the rotated family
   back onto the anchor. Encrypt + cache the minted token in `auth.app_power_token_cache` until its `exp`.
6. **Control side — return** `{ access_token, expires_at, scopes }` to the worker; the worker hands it to
   the server caller only. The browser is never in this path.

This is the literal realization of the directive's "the power token can be exchanged via server side
sdk." It mirrors Auth0's "one narrow token per API, never a broad token": `audience` is required and
`scopes` is least-privilege, so an exfiltrated *server-side* token (a much harder target) caps blast
radius to one audience.

### 3.3 Console power-exchange (the v1 consumer — control plane)

The console (modeled as the `console` pseudo-app, §6) obtains its control-plane power token via the
**same §3.2 control-plane mint** — its server-side calls use `@zeroship/auth/server`, audience-bound to
the control plane:

```ts
// console (pseudo-app) server-side — never ships the token to the console browser
const { accessToken } = await auth.getAccessToken({
  audience: CONTROL_PLANE_AUDIENCE,    // external-audience class → a REAL Hydra aud-narrowed token (§3.4)
  scopes: ["apps:read"],               // the SPECIFIC scope this operation needs, not BUILDER_SCOPES
});
await controlPlane.listApps({ bearer: accessToken });   // injected server-side
```

<!-- Revised in round 2: the mint runs in the CONTROL PLANE (§3.2), not a gateway internal RPC and not a
     bespoke console confidential-client tier (§6). And per §3.4 the token is aud-narrowed (real, Hydra);
     scope narrowing is NOT relied on (ory/hydra#3311). -->
**The `accessToken` here is a real Hydra-issued, `aud=CONTROL_PLANE_AUDIENCE` access token** (§3.4),
minted in the control plane (§3.2), so it composes with the control plane's **existing** verification
with no new verifier: `AuthzGuard` already introspects the Bearer at Hydra
(`hydra_introspector.introspect`, `authz_guard.rs:215`) and checks `result.aud ==
state.expected_oauth_audience` (`authz_guard.rs:225-230`). It is **not** a gateway wrapper — the control
plane has no wrapper-verifier and gains none. Its `scope` claim is the full consented grant (Hydra
reality, §3.4); least-privilege is the mint ceiling + per-op `require` (§5.1), not a narrowed `scope`.

The console browser holds **no JWT and no control-plane bearer** — only the user projection + the two
HttpOnly cookies (`__Host-zs_app_session`, `__Host-zs_app_anchor`). Control-plane bearers live entirely
in the console server tier. See §6 for the migration off `apps/zeroship-builder/src/server/{oauth,session}.ts`.

### 3.4 Power-token format + the Hydra down-scope reality (resolves §10 Q2)

<!-- Rewritten in round 2: addressing major #4. The round-1 draft asserted the external path obtains a
     down-scoped token "via grant_type=refresh_token with audience + scope parameters" and that this
     "composes with existing code." VERIFIED FALSE on two counts: (1) refresh_token_public (oidc_rp.rs:427)
     sends ONLY grant_type/refresh_token/client_id — no audience, no scope, no RFC 8707 resource: the
     down-scope is NET-NEW code. (2) Per RFC 8707, refresh tokens stay bound to the FULL original grant,
     and Ory Hydra specifically does NOT honor scope-narrowing on grant_type=refresh_token — open issue
     ory/hydra#3311: "request an access token with narrower scope did not work and always return all
     scope" (it returns the full granted scope). So scope-narrowing CANNOT be relied on at the Hydra
     layer. Per feedback_verify_constants, we do not assert unverified protocol behavior. The design below
     is corrected to what Hydra actually supports (the `audience` param) + platform-enforced least-privilege. -->

**What Hydra actually does on the refresh grant (verified, not assumed):**

- `OidcRp::refresh_token_public` (`oidc_rp.rs:427`) today sends **only** `grant_type=refresh_token` +
  `refresh_token` + `client_id`. It sets **no `audience`, no `scope`, no RFC 8707 `resource`**. A
  down-scoping refresh is therefore **net-new code** (a new method / new params), listed in ADD — it is
  NOT "composing with existing code."
- **`audience` narrowing on the refresh grant: supported.** Hydra accepts an `audience` parameter on the
  token request and stamps the resulting token's `aud` accordingly (this is how the control plane's
  `expected_oauth_audience` check is satisfiable). The down-scope sets `audience = CONTROL_PLANE_AUDIENCE`.
- **`scope` narrowing on the refresh grant: NOT reliable on Hydra.** Per RFC 8707 a refresh token is
  bound to the **full original grant**, and Ory Hydra specifically returns the **full granted scope** on
  `grant_type=refresh_token` regardless of a narrower `scope` parameter (open issue ory/hydra#3311). So
  **we do not depend on Hydra to narrow the scope claim.** Least-privilege is enforced by the platform,
  not the token's `scope` claim (see below).

**How least-privilege is actually enforced (platform-side, not Hydra-side):**

1. **The mint refuses to exceed the ceiling.** Control rejects the call (`403 scope_required`) if
   `opts.scopes ⊄ anchor.granted_scopes` (§4). The caller cannot obtain authority beyond the grant even
   though the resulting Hydra token may carry the *full* granted scope.
2. **The resource server checks the SPECIFIC op scope per request.** The control plane's `AuthzGuard`
   already runs `require(Action, Resource)` per handler (`authz_guard.rs`, §5.1). A list call needs
   `apps:read`; a token whose `scope` claim is the full grant still passes only the handlers whose
   required scope it contains — and the platform never grants the console a broad `BUILDER_SCOPES` grant
   in the first place (§6), so the "full grant" is itself least-privilege by consent. The per-op
   `require` is the binding least-privilege check, exactly Auth0's "validate at the API, check scope
   per-endpoint, every request."
3. **(Optional hardening, deferred) true scope-narrowed tokens** would require either a Hydra version /
   config that honors `scope` narrowing on the refresh grant (track ory/hydra#3311) or minting a
   platform wrapper with the narrowed `scope` for resource servers that accept wrappers. Neither is
   required for v1 correctness because (1)+(2) already bound authority.

**Power-token format by audience class (v1):**

- **External-audience = the control plane AND genuinely external resources.** A **real Hydra-issued
  access token**, `aud`-narrowed via the new down-scoping refresh (above). This composes with the control
  plane's **existing** introspection + `expected_oauth_audience` check (`authz_guard.rs:215-230`, §5.1) —
  **no new verifier on the control plane.** The `scope` claim is the full granted scope (Hydra reality);
  least-privilege is mint-ceiling + per-op `require` (above).
- **Internal-audience = the app's own worker.** The round-1 "gateway-minted wrapper" format is **deferred
  out of v1.** With the mint relocated to the **control plane** (§3.2), the wrapper `Issuer` lives in the
  *gateway*, which control does not call — so control cannot mint a gateway wrapper without a new
  control→gateway link (the very dependency we removed). For v1, an app calling its **own** worker does
  not need a fresh power token at all: the worker already has the request's authenticated
  `ZeroShip-User` (identity + scopes) for own-app authorization (§5.2). `getAccessToken` is for calling a
  **different** resource server (the control plane, or a deferred third-party API), all of which are the
  external/Hydra class. If a future internal-audience-wrapper need appears, it is an explicit ADD (move
  the wrapper issuer into a control-callable surface, or add the control→gateway link then) — not part of
  v1.

For v1, the in-scope audience is **the control plane** (real Hydra token, `aud`-narrowed). Generic
external third-party APIs (the relay-style "Token Vault" analogue) and internal-audience wrappers are
deferred — same Hydra down-scope machinery plus, respectively, a per-third-party audience registry or a
control-reachable wrapper issuer.

---

## 4. Grant in platform tables

`control.oauth_grants` is the **source of truth** for consented scopes — already keyed by
`(user_id, client_id)`, where `user_id` is the global `auth.users(id)` UUID and `client_id` is the
per-app `oac_<base62>` (1:1 with `(global_user, app)`). It is written once at `accept_consent`
(`crates/auth/src/ui/consent.rs::upsert_oauth_grant`) and is the single ledger (the original `§5.2`
already dropped the parallel `auth.app_grants`).

<!-- Revised in round 1: addressing major #3. The original draft both (a) said the mint consults
     control.oauth_grants as source of truth AND (b) said anchor.granted_scopes is the ceiling — these
     diverge after a revoke and the draft never said which wins. It also re-introduced the cross-schema
     hot-path join that changeset 0008 deliberately removed. Resolved below: the ceiling is the
     DENORMALIZED anchor.granted_scopes (hot-path consistent), freshness comes from the family-marker
     check + short TTL, NOT a per-call control.oauth_grants join. -->

#### 4.1 Two scope sources, reconciled — ceiling is denormalized, freshness is the family marker

There are two places the consented scope set lives: the authoritative `control.oauth_grants.granted_scopes`
and the **denormalized copy** on the anchor (`anchor.granted_scopes`, copied at anchor-create,
`anchors.rs:163`). The original draft consulted the authoritative table on **every** mint — which
re-introduces exactly the cross-schema hot-path join changeset 0008 engineered away ("emit
`WorkerUser.scopes` from this column instead of a cross-schema `control.oauth_grants` join on the hot
path"). We do **not** do that. The mint path reads the **denormalized `anchor.granted_scopes`** as the
ceiling, consistent with the platform's no-hot-path-join decision. Concretely:

- **The scope ceiling on the mint path = `anchor.granted_scopes`** (no `control.oauth_grants` join). The
  requested `opts.scopes` must be `⊆` this set; otherwise `403 scope_required`.
- **Revocation freshness does NOT depend on re-reading `control.oauth_grants`.** It comes from two
  mechanisms that already exist and a one-line addition:
  1. **The family marker (authoritative, cross-node).** `DELETE /me/oauth-grants/{client_id}` runs the
     transactional cascade (`oauth_grants_handlers.rs::revoke_grant` → `relay_revoke::revoke_grant_cascade`):
     it DELETEs the `control.oauth_grants` row, stamps the relay alias revoked, AND writes the
     `auth.token_revocations` family marker for `(client_id, pws_)` (`revoke_family`, `NOW()`-stamped into
     `revoked_after`). The mint path checks this marker
     (`zeroship_core::wrapper_revocation::is_family_revoked_since(conn, client_id, sub, since)`, the same
     primitive the wrapper Bearer arm uses at `router/auth.rs:785`) **before minting**. A revoked family ⇒
     `403 consent_required`. Authoritative; does not read `control.oauth_grants`.
     <!-- Revised in round 2: addressing minor #10. The primitive's signature (core/wrapper_revocation.rs:51)
          is is_family_revoked_since(db, client_id, sub, iat: i64) and tests EXISTS(... revoked_after >
          to_timestamp(iat)). The wrapper Bearer arm passes claims.iat (the token's OWN issue time), NOT
          anchor.created_at — and verified anchors.rs:254 never slides created_at on rotation. So the
          round-1 "since = anchor.created_at" was the WRONG baseline. The correct "since" for a fresh mint
          is the mint instant (now). -->
     - **The `since` baseline for a server-side mint = the mint instant (`now`), exactly mirroring the
       wrapper arm's `claims.iat`.** The wrapper Bearer arm passes the *token's own* `iat` (the moment that
       token was minted): a token is dead iff a revoke landed *after* it was issued. For a server-side mint
       we are deciding whether to issue *now*, so the question is "has a revoke landed for this family at
       all (up to now)?" Concretely the mint computes `is_family_revoked_since(conn, client_id, pws_,
       now_minus_lookback)` where the lookback covers the family's life (the anchor's `abs_expires_at`
       window). Using `anchor.created_at` would *also* work for the all-or-nothing revoke model (any revoke
       is stamped after anchor-create), but it is the wrong *mental* baseline and breaks if `created_at`
       ever drifts; we tie the baseline to the family's validity window, not to a column that is
       deliberately never slid (`anchors.rs:254`).
     - **Worked timeline (concrete timestamps).** Anchor created at `T1=10:00` (`created_at`, never slid).
       User revokes the grant at `T2=11:00`: the cascade writes `token_revocations.revoked_after = 11:00`
       for `(client_id, pws_)` and DELETEs the Hydra consent session. A `getAccessToken` at `T3=11:05`
       checks `EXISTS(... revoked_after > to_timestamp(since))`. With `since ≤ 11:00` (either `T1=10:00` or
       a family-window lookback) the predicate `11:00 > since` is **true** ⇒ revoked ⇒ `403`. If the user
       then re-consents at `T4=11:10`, a **new** consent + a **new** anchor are created (`created_at=11:10`,
       a new `(client_id, pws_)` family lineage / fresh `refresh_family_id`); the stale `revoked_after=11:00`
       marker is `< 11:10`, so it no longer trips the new anchor — and the cascade's marker key is
       `(client_id, pws_)`, which is the **same** key, so the design relies on the new anchor's mint check
       using `since ≥ 11:10` (the new family window), which the `now`-anchored lookback satisfies. (Marker
       key confirmed: `is_family_revoked_since` filters `WHERE client_id = $1 AND sub = $2`,
       `core/wrapper_revocation.rs`, i.e. exactly `(client_id, pws_)`.)
  2. **Hydra `invalid_grant` on family rotation.** Revocation also revokes Hydra consent sessions
     (`hydra_revoke_consent_sessions` in the cascade), so the next `grant_type=refresh_token` down-scope
     returns `invalid_grant` → anchor-dead (delete the row + cache). Any external (Hydra) power token
     stops being mintable.
  3. **Short token TTL.** An already-minted, still-cached power token is bounded by its own `exp`
     (minutes), not by the 30-day anchor.
- **Why this is safe even though `anchor.granted_scopes` is denormalized:** revocation in this platform
  is **all-or-nothing per `(user, app)`** — `revoke_grant_cascade` DELETEs the whole grant row; there is
  no partial-scope-narrowing API. So the only divergence a stale `anchor.granted_scopes` can cause is
  "the app was fully revoked but the anchor copy still lists scopes" — and that case is caught by the
  family-marker check (mechanism 1), which fails the mint regardless of the stale ceiling. There is no
  "narrowed to fewer scopes but anchor still wide" case to mishandle, because narrowing-without-revoke
  does not exist.

> **INVARIANT (not an aside) — anchor.granted_scopes is only safe as the ceiling while
> narrowing-without-revoke does not exist.** The denormalized ceiling is correct **only** because every
> consent change today is all-or-nothing (full revoke). The moment a partial-scope-narrowing API (drop
> `secrets:write` while keeping `apps:read`, without a full revoke) lands, the family marker will NOT fire
> (no revoke was stamped) and a stale wide `anchor.granted_scopes` would let the mint exceed the now-narrowed
> grant. **Therefore: any partial-scope-narrowing path MUST update `anchor.granted_scopes` (every live
> anchor for that `(user, app)`) in the SAME transaction as the `control.oauth_grants` update, and SHOULD
> stamp a (narrowing-flavored) family marker so already-cached tokens re-validate.** This is a hard
> precondition on adding partial narrowing, recorded as an invariant so it cannot be missed.
<!-- Revised in round 2: addressing minor #10 — promoted the partial-narrowing note from an aside to an
     explicit INVARIANT with the concrete same-transaction requirement. -->
- **`anchor.granted_scopes` is written at anchor-create (`anchors.rs:163`)** and re-copied on each
  re-consent (a re-consent widens `control.oauth_grants` then creates a fresh anchor, §10-D5), so it
  tracks widening correctly today; only *narrowing-without-revoke* (which does not exist) would desync it.

#### 4.2 Queryable + revocable (unchanged)

`GET /me/oauth-grants` lists, `DELETE /me/oauth-grants/{client_id}` runs the transactional revoke
cascade above. Because the power token is server-side and short-lived, revocation is *more* effective
than in the original build: there is no browser-cached bearer to outlive the grant; the next
server-side exchange after a revoke fails the family-marker check, and any in-flight server-held power
token is bounded by its short TTL + the cross-node `(client_id, sub)` family marker.

**Net:** consent (what the user agreed to share) lives in a platform table; the *power* to act on it
is minted server-side, per-operation, never exceeding the (denormalized) grant ceiling, with revocation
enforced by the family marker — not a per-call cross-schema join. The "grant" and the "token" are
cleanly separated — the grant is durable + queryable; the token is ephemeral + audience-scoped.

---

## 5. Server-side per-operation authorization

Authorization is enforced **server-side, per-operation, at the resource server** — least-privilege,
audience-scoped. The client never makes the authz decision (UI gating is UX only).

### 5.1 The control plane is the resource server for the console

<!-- Revised in round 1: addressing major #5/§5.1. The control plane ALREADY introspects Hydra tokens
     + checks expected_oauth_audience; that path is not a redesign delta. Credit it honestly, and state
     precisely what the power token IS (a real Hydra down-scoped token, §3.4) so §3.3 + §5.1 compose. -->

Every control-plane operation re-checks the caller's authority for *that* operation. **Most of this
already exists and is NOT a redesign delta.** `AuthzGuard` (`crates/control/src/authz_guard.rs`):
- already authenticates OAuth callers — introspects the Bearer at Hydra
  (`hydra_introspector.introspect`, line 215) and checks `result.aud == state.expected_oauth_audience`
  (lines 225-230, else `wrong_audience`), in addition to a PAT path;
- already resolves `principal_id` and runs `require(Action::X, Resource::Y)` per handler (e.g.
  `oauth_grants_handlers.rs` gates `AccountRead`/`AccountWrite`).

So "the control plane validates an audience-scoped token + checks scope per-op" is **largely the path
that exists today**. The redesign's actual deltas are narrow and honest:

1. **What the caller presents** changes from the console's single broad client-held token to a
   **per-operation, audience-scoped power token minted server-side** (§3.3). Critically — per §3.4 —
   **that token is a REAL Hydra-issued, `aud=CONTROL_PLANE_AUDIENCE` access token, not a gateway
   wrapper.** This is what makes it compose with the existing introspection + audience check **with no new
   verifier on the control plane.** (The control plane does NOT learn to verify gateway wrappers; that
   would be an explicit ADD and is unnecessary because we use real Hydra tokens for the control-plane
   audience.) Its `scope` claim is the full consented grant (Hydra reality, §3.4); least-privilege is the
   mint ceiling + per-op `require`, not a narrowed `scope`.
2. **Per-op authorization** is enforced by the control plane's `require(Action, Resource)` per handler —
   Auth0's "validate at the API, check scope per-endpoint, every request." An invalid/absent token is
   `401`; an action the principal/grant does not cover is `403` (both already implemented).
   <!-- Revised in round 2: addressing major #4 ripple. Do NOT claim the token's `scope` claim is the
        narrowed per-op scope — per §3.4 Hydra returns the FULL granted scope on the refresh grant
        (ory/hydra#3311), so the token's `scope` is the full (already least-privilege-by-consent) grant,
        not a per-op narrowing. The per-op binding check is require(Action,Resource) + the mint ceiling,
        NOT a narrowed `scope` claim. -->
   - **The binding least-privilege check is `require(Action, Resource)` + the mint ceiling (§3.4 / §4),
     not a narrowed `scope` claim on the token.** Because Hydra returns the full granted scope on the
     refresh grant, the presented token's `scope` is the *full consented grant* (which is itself
     least-privilege — the console no longer holds the broad `BUILDER_SCOPES`, §6). The control plane's
     `require` gates each endpoint to the action it needs; the mint already refused to issue beyond
     `anchor.granted_scopes`. So least-privilege holds end-to-end without depending on Hydra scope
     narrowing. If `require` were ever to read the token's `scope` directly for a per-op decision, it must
     account for it being the full grant (the principal-based `require` does this correctly today).

### 5.2 The worker/app is the resource server for app ops

<!-- Revised in round 1: addressing BLOCKER #1 ripple. The live SPA path reads gateway_sessions, so
     ZeroShip-User.scopes for SPA requests is sourced from gateway_sessions.granted_scopes (the cookie
     arm, changeset 0008), NOT the anchor. The anchor's granted_scopes is the MINT ceiling (§4), a
     different path. -->
For app-level operations the worker re-checks per-op against `ZeroShip-User.scopes` (the kernel contract
from the original `§1.4`). For SPA requests those scopes are sourced from
**`gateway_sessions.granted_scopes`** — the column the live cookie arm already reads (changeset 0008,
no `control.oauth_grants` join). (The anchor's `granted_scopes` is a *different* path: it is the ceiling
for the server-side power-token mint, §4, not the per-request worker scope source.) The worker also
re-checks any route-level `required_scopes` (`Rule.required_scopes`, original `§5.3`); the gateway
enforces `required_scopes` against the resolved cookie session before dispatch. The app's own RPC
handlers (`@zeroship/rpc`) re-check `ctx.user.scopes` for sensitive procedures. **Client gating is
UX-only:** the SPA may hide a button, but the worker/gateway is the enforcement point.

> **The client genuinely cannot self-authorize now.** In the original build the SPA held a `scope`-bearing
> wrapper and *could* have made (wrong, non-binding) client-side decisions. Here the SPA holds no token
> with scopes at all — there is nothing to make a client-side authz decision *from*. `hasScope()` is
> removed from the client surface; scope-awareness for UX, if an app wants it, is a server round-trip
> (`getUser()` returns identity only; an app that wants to gray out a button asks its own server, which
> knows the grant).

### 5.3 Step-up auth for destructive console ops

<!-- Rewritten in round 1: addressing major #4. The original gate required `amr` containing "mfa",
     which NO token can satisfy today: login.rs hardcodes amr=["pwd"] (lines 426-427) and control's
     mfa_verified is hardcoded false (authz_guard.rs:108/181/248). An amr=mfa gate would 403 every
     deploy/secret-rotation/Stripe/delete forever — a DoS on the highest-value ops. Step-up is
     REDEFINED in terms of what exists today (auth_time recency + a fresh prompt=consent
     re-authorization for the elevated scope); the amr=mfa factor is gated behind an MFA-enablement
     slice and is NOT part of the v1 enforcement. -->
<!-- Revised in round 2: addressing BLOCKER #3 (auth_time source) — for the control-plane power token the
     auth_time comes from the HYDRA token's `auth_time` claim (standard OIDC, populated by Hydra),
     re-checked at the control plane; it does NOT come from gateway_sessions (the console talks to control,
     not the gateway dispatch path). The SPA-facing auth_time (for UX) comes from the new
     gateway_sessions.auth_time column (§2.2 step 5b). Also reconcile with the EXISTING mfa_age_seconds
     recency knob (authz_guard.rs:19, authz/eval.rs:172). Also adjust the "elevated scope" guarantee for
     the §3.4 Hydra-returns-full-scope reality: recency comes from a fresh max_age re-auth, not from
     scope narrowing. -->

Gate the irreversible/money-moving control-plane operations behind **step-up auth** (Auth0's
"high-value op ⇒ re-challenge ⇒ fresh, recent authorization"). Applies to: **production deploy**,
**secret rotation**, **Stripe Connect onboarding**, **app deletion**.

**v1 step-up = elevated scope present + fresh `auth_time` (NO `amr=mfa` requirement).** The MFA substrate
does not exist yet — `crates/auth/src/ui/login.rs` hardcodes `amr: vec!["pwd"]` / `acr:
Some("urn:zeroship:pwd")` (lines 426-427, 453-454) and `crates/control/src/authz_guard.rs` hardcodes
`mfa_verified: false` (lines 108, 181, 248). A gate that required `amr` containing `"mfa"` could
**never pass** today and would 403 every deploy/secret-rotation/Stripe-onboarding/delete forever — a
denial of service on exactly the highest-value ops. So v1 enforces only what is producible:

- The console server marks these `Action`s as **step-up-required**.
- **Where `auth_time` comes from (the buildable source).** The console calls the **control plane** with a
  real Hydra power token (§3.4), so the control plane reads the **`auth_time` claim on that Hydra token**
  — a standard OIDC claim Hydra populates at the authorization it issued from. This is the same value the
  existing `AuthzGuard.mfa_age_seconds` knob expresses ("seconds since the authenticating event",
  `authz_guard.rs:19`, consumed by the authz engine at `authz/src/eval.rs:172` as the recency input to
  `require`). **v1 wires step-up to that existing recency knob:** compute `mfa_age_seconds = now -
  token.auth_time` and require it `≤ STEP_UP_MAX_AGE` (e.g. 300s) for step-up `Action`s. (It is named
  `mfa_age_seconds` historically; in v1 it is an *auth-recency* age, not an MFA age — the MFA factor is
  P8.) `auth_time` is NOT sourced from `gateway_sessions` for the console path; `gateway_sessions.auth_time`
  (§2.2 step 5b) is the **SPA-facing** copy used only for UX hints (below).
- On a step-up `Action` the control plane checks: the presented Hydra power token carries the elevated
  scope (e.g. `apps:deploy`, `secrets:write`, `billing:write`) in its (full, §3.4) grant **AND**
  `now - auth_time ≤ STEP_UP_MAX_AGE`. The elevated scope guarantees the op is within what the user
  consented; the fresh `auth_time` guarantees a *recent deliberate re-authentication*. `amr=mfa` is
  **not** checked in v1.
- **How recency is refreshed (not via scope narrowing — §3.4).** Because Hydra returns the full grant on
  the refresh grant, a plain server-side refresh does NOT advance `auth_time` (it reuses the original
  authorization). To get a *fresh* `auth_time`, the console server runs an **interactive re-authorization**
  (popup) with `max_age=0` (force a fresh prompt) — and `prompt=consent` if the elevated scope is not yet
  in the grant — which makes Hydra issue a new authorization with a current `auth_time`. The subsequent
  token (via the §3.4 down-scope/refresh of the *new* family, or directly from the new code exchange)
  then carries the recent `auth_time`. So step-up recency is driven by `max_age`, which Hydra honors,
  not by scope narrowing, which it does not.
- If not satisfied → `403 step_up_required` with a `WWW-Authenticate` challenge naming the required scope
  + the max-age (and, post-MFA, the `acr_values`). The console server runs the fresh re-authorization,
  the user re-authenticates (and re-consents if widening), and the op proceeds with a token whose
  `auth_time` is now within the window.
- The browser still never holds the elevated power token; only `auth_time`/`amr` are surfaced to it via
  the `{ user }` projection (from `gateway_sessions.auth_time`/`amr`, §2.2 step 5b) for UX —
  "re-authenticate to deploy" — and those are advisory, never the enforcement point.

**MFA-elevation follow-on (out of scope for v1, explicit dependency).** Once MFA factors land (login UI
can challenge a second factor and stamp `amr` containing `"mfa"`; `authz_guard` populates `mfa_verified`
from the introspected `amr`), the step-up gate is tightened to additionally require `amr ∋ "mfa"`. That
is a **strictly-later slice** gated on MFA enablement — it is NOT part of P7. P7 ships the
`auth_time`-recency + elevated-scope enforcement, which IS satisfiable today.

---

## 6. The builder/console specifically

The console is the **highest-privilege surface** (app CRUD, deploy, route registry, env/secrets,
Stripe Connect). It currently runs a **bespoke confidential RP**:

- `apps/zeroship-builder/src/server/oauth.ts` — its own PKCE, `buildAuthorizationUrl`/`exchangeCode`/
  `refreshAccessToken`/`revokeToken` against Hydra directly, a **single broad `BUILDER_SCOPES`** set
  (`apps:read/write/deploy`, `env:read/write`, `secrets:read/write`, `deployments:read/rollback`),
  `subjectFromAccessToken` verifying the Hydra access JWT with `jose`.
- `apps/zeroship-builder/src/server/session.ts` — an HMAC-signed `__zs_builder_oauth_user` cookie
  carrying the bare user id.

<!-- Rewritten in round 2: addressing MAJOR #6. The console is console.zeroship.ai, a DISTINCT host, NOT
     an {app}.zeroship.ai. The whole /token + anchor + gateway_sessions + pws_ machinery is keyed on the
     per-app host via resolve_route (auth_token.rs:73-110), which 503s unless the host is a provisioned app
     with oauth_client_id + sector_identifier (auth_token.rs:420-428). The round-1 draft asserted the
     console "reuses __Host-zs_app_session" + the popup /token flow but never explained how
     console.zeroship.ai gets a route entry / oauth_client_id / sector_identifier / anchor / gateway_sessions
     row, nor how __Host- (host-bound) cookies span console→control. This is now spelled out. -->

**The host problem (must be resolved first).** The popup `/token`/`/session`/anchor/`gateway_sessions`
machinery is **keyed on the per-app host** by `resolve_route` (`auth_token.rs:73-110`): it looks the host
up in the route cache and **503s `client_not_provisioned`** unless that host has an `oauth_client_id`
(`:97`) and a `sector_identifier` (consumed by `pairwise_sub`, `:420-428`). `console.zeroship.ai` is a
distinct host that has **none** of that scaffolding today. So the console cannot "just reuse" the SPA
paths without being modeled as something the route cache + auth endpoints recognize.

**Decision — model the console as a first-class pseudo-app ("console" app).** Provision
`console.zeroship.ai` as a platform-owned app record so the **existing** `/token`/`/session`/anchor/
`gateway_sessions` paths apply verbatim — no bespoke console auth tier, no fork of the auth endpoints:

1. **Provision a `console` route entry + OAuth client + sector identifier.** Seed a control-plane app
   record for `console.zeroship.ai` with its own `oauth_client_id` (a confidential-or-public client whose
   `audience` includes `CONTROL_PLANE_AUDIENCE`) and a `sector_identifier`, so `resolve_route` resolves it
   and `pairwise_sub` can derive `pws_`. This is a one-time platform seed (a migration/bootstrap row), not
   a creator action. The console is then an app the gateway/auth endpoints already know how to serve.
2. **Login via `@zeroship/auth` (identity), exactly like any app.** The console SPA logs in via the
   popup/consent flow on its own host and holds **no JWT** — only the two HttpOnly cookies
   (`__Host-zs_app_session` live credential + `__Host-zs_app_anchor` reload-recovery, both `__Host-` →
   bound to `console.zeroship.ai`) and the `{ user }` projection. The bespoke `buildAuthorizationUrl`/
   `exchangeCode` client-side flow and the `__zs_builder_oauth_user` HMAC cookie are removed.
   `apps/zeroship-builder/src/server/{oauth,session}.ts` are deleted; the console SPA uses the standard
   `@zeroship/auth` client, and there is **no bespoke confidential-client server tier** anymore.
3. **BFF token custody is server-side, in the control plane (not a console server tier).** Per §3.2 the
   power token is minted **in the control plane** from the console pseudo-app's anchor + grant. The
   console's `getAccessToken` is the §3.2 path: the console's server-rendered/SSR calls (or its
   server-side data loaders) call `auth.getAccessToken({ audience: CONTROL_PLANE_AUDIENCE, scopes:[…] })`,
   which issues the worker→control internal call (the console pseudo-app runs as a normal worker app, so
   it has the same `control_url`/`control_key` + the echoed signed `ZeroShip-User`). The control plane
   re-derives the user from the signed header, mints a real Hydra `aud=CONTROL_PLANE_AUDIENCE` token (§3.4),
   and the console injects it server-side. The control-plane bearer **never** reaches the console browser.
4. **Cross-host cookie scoping is resolved by NOT crossing hosts with cookies.** `__Host-` cookies are
   host-bound, so `__Host-zs_app_session` on `console.zeroship.ai` does **not** travel to
   `control.zeroship.ai` — and it must not. The console→control calls do **not** rely on a cookie: they
   are **server-side** calls carrying the Hydra **Bearer** (injected by the BFF), which is exactly what
   `AuthzGuard` expects (§5.1). The browser↔console hop uses the host-bound `__Host-zs_app_session`
   cookie; the console-server↔control hop uses the Bearer. No cookie is asked to span hosts.
5. **Least-privilege, per-op, not the broad `BUILDER_SCOPES`.** Each control-plane call requests the
   *specific* scope its `Action` needs (`apps:read` for a list, `apps:deploy` for a deploy), so the
   console's consented grant is the union of those least-privilege scopes — never a single broad
   `BUILDER_SCOPES` bundle. The control plane re-checks per-op via `AuthzGuard` (§5.1); per §3.4 the
   Hydra token's `scope` claim may be the full grant, so the binding check is `require(Action, Resource)`,
   not the token's `scope`.
6. **Step-up for destructive ops** (§5.3): deploy, secret rotation, Stripe onboarding, app deletion
   require a recent `auth_time` (via a `max_age=0` re-auth popup) + the elevated scope present in the
   grant, checked at the control plane through the existing `mfa_age_seconds` recency knob. (The `amr=mfa`
   factor is a later slice gated on MFA enablement — §5.3; v1 does NOT require it.)
7. **Sign-out stays client-driven** (`client.signOut()` → `POST /__zs/auth/signout` on the console host);
   there is no server-side `signOut` (§3.2).

> **Alternative considered + rejected: keep a bespoke console confidential-client server tier.** That
> would let the console hold a confidential client + its own cookie without the pseudo-app seed — but it
> directly contradicts goal "replace `oauth.ts`/`session.ts`" (it keeps a hand-rolled RP + cookie), it
> re-introduces the broad-client risk the redesign exists to remove, and it duplicates the
> anchor/`gateway_sessions`/relay/pairwise machinery the platform already has. The pseudo-app model reuses
> all of it. If a future requirement forces a confidential client specifically for the console (e.g. a
> server-only refresh the pseudo-app's public client can't hold), that is an explicit ADD; it is not
> needed for v1.

This makes the console consistent with the platform invariant "the console frontend is dumb about
authorization; the control plane enforces it" — the same logic as "the gateway is dumb; the worker
runs app logic."

---

## 7. Migration delta vs. the built code

Pre-launch, no back-compat (`AGENTS.md`): rip out, do not shim. Every producer/consumer in one patch.

### REMOVE
- **Power token to the browser.** `auth_token.rs::token` no longer returns `"access_token": wrapper`
  (line 519); `session`/`mint`/`do_refresh` no longer return a power wrapper to the browser. The
  wrapper-as-browser-artifact is deleted from the SPA-facing JSON.
- **The browser-facing wrapper mint paths** at `POST /__zs/auth/token` and `GET /__zs/auth/session?mint=1`
  no longer mint a *browser* wrapper at all. (The wrapper *machinery* — `Issuer`/`Verifier`/the Bearer
  arm — stays; see KEEP. What is removed is "mint a wrapper and hand it to the SPA.")
- **The anchor's single `cached_access_token`/`cached_access_exp` browser-wrapper cache** (`anchors.rs`)
  — the SPA no longer holds a wrapper to coalesce, so this column pair is dropped (replaced by the
  per-audience `auth.app_power_token_cache`, §3.1; pre-launch, no shim). **Full cascade in the same PR**
  (§3.1): `Anchor` fields (`anchors.rs:160-161`), `NewAnchor` field + `create` bind (`:178`, `:196-211`),
  `update_minted` (deleted, `:251-277`), `row_to_anchor` reads (`:355-360`), the `mint` cached-wrapper
  short-circuit (`auth_token.rs:683-694`), the `ANCHOR_MINT_CACHE_TTL_SECS` const, and the cache-window
  tests.
- **Client-held token machinery** (`sdks/auth/src/`): `CacheManager`, `InMemoryCache`,
  `LocalStorageCache` (`internal/cache.ts`); the `cacheLocation`/`useRefreshTokens` options; the
  Web-Worker refresh (`internal/worker.ts`); `RefreshLock`/`navigator.locks` (`internal/locks.ts`);
  `mintUnderLock`/`inflightMint`. The SPA no longer caches or refreshes a token because it holds none.
- **Client `getAccessToken*` + scope surface:** `client.ts` `getAccessToken()`,
  `getAccessTokenWithPopup()`, `hasScope()`; `Session.access_token`, `Session.scopes`, `User.scopes`,
  `token_type` (`types.ts`); `transport.ts` `sessionMint()` returning an `access_token`. `requestScopes`
  becomes a *server*-driven step-up (it asks the console/app server to re-consent), not a client token
  mint.

### CHANGE
- **`POST /__zs/auth/token`** (a) creates a `gateway_sessions` row + sets `__Host-zs_app_session`
  (`sessions::create` with the real `NewSession` shape — `user_id: &str`, `Option<&str>` fields,
  `granted_scopes: &[String]`, **plus the new `auth_time`/`amr` fields** — §2.2 step 5/5b), (b) keeps the
  anchor create, and (c) returns `{ user, expires_at }` (with the **relay-swapped** email, §2.3) and
  **no `access_token`, no `scope`, and no `id_token`** (the `zs-id+jwt` is minted but held server-side,
  per the §10 Q1 HttpOnly-only decision).
- **`GET /__zs/auth/session[?mint=1]`** returns the identity projection (with the **mandatory relay-email
  swap**, §2.3) from the live `gateway_sessions` row, sourcing `auth_time`/`amr` from the **new
  `gateway_sessions` columns**; when the gateway session is gone but the anchor is valid, it runs anchor
  reload-recovery (rotate family, **re-create the `gateway_sessions` row + re-set
  `__Host-zs_app_session`** carrying `auth_time`/`amr` forward, mint a server-held `zs-id+jwt`). The
  rotated raw Hydra access JWT stays server-side for §3. **No JWT in any SPA-facing body; the real email
  never appears.**
- **SPA app-auth via the `gateway_sessions` cookie arm** (`resolve_app_session_user_header_inner`,
  `router/auth.rs:1520`): **substantively unchanged** — it already reads `gateway_sessions`, relay-swaps,
  and emits `ZeroShip-User`. The redesign makes the popup flow *populate* this store (above); the arm
  itself needs only the new anti-CSRF conjunction on state-changing requests (next item). **The anchor
  store is NOT promoted to a live request credential** (preserves the `anchors.rs` two-store invariant).
- **Anti-CSRF on the cookie-authenticated dispatch path** (`router/auth.rs`, NEW): on state-changing
  (`POST`/`PUT`/`PATCH`/`DELETE`) cookie-authenticated `/api/*` + RPC requests, require **`Origin` present
  + exact-match** (binding) **and `Sec-Fetch-Site == same-origin` when present** (advisory-when-absent) —
  **no mandatory custom header**, so raw same-origin `fetch` in non-SDK / raw-JS deploys passes (§1.2).
  Idempotent `GET`/`HEAD` are exempt.
- **The gateway wrapper verification arm rejects `zs-id+jwt`** — this is the **existing `at+jwt` typ
  gate** (`wrapper_token.rs::Verifier::verify`, line 433), not a new verifier. An identity token routed
  to the wrapper path fails the typ check → `Invalid` (→ `401` on User/Admin routes; anonymous on Anon
  routes, §2.1). The only new wiring is the identity minter stamping `zs-id+jwt` and the `/session`
  identity-verification path accepting only `zs-id+jwt`.
- **The client SDK contract** (`client.ts`/`transport.ts`/`types.ts`/`react.tsx`): `getSession()`/
  `getUser()` return identity only (fetched from `/__zs/auth/session`, no client-held JWT); `useAuth()`
  drops `getAccessToken`/`getAccessTokenWithPopup`/`hasScope`.
- **Console RP** (`apps/zeroship-builder/src/server/{oauth,session}.ts`): replaced per §6.

### ADD
- **Identity-token issuance** (gateway): a `zs-id+jwt` minter (reusing the ed25519 key + kid overlap),
  held server-side keyed to the gateway session; the `/session` identity-verification path that accepts
  only `zs-id+jwt`. (No "disjoint typ verifier on the Bearer/DPoP arms" — the existing `at+jwt` gate
  already rejects `zs-id+jwt`, §2.1.)
- **`gateway_sessions.auth_time` + `gateway_sessions.amr` columns** (new changeset): `auth_time
  TIMESTAMPTZ`, `amr TEXT[] NOT NULL DEFAULT '{}'`, plus the `NewSession`/`AppSession`/`validate()` field
  threading; populated from the validated id_token claims at session-create (§2.2 step 5b). Required by
  the step-up gate (§5.3) and the SPA projection (§2.3). <!-- Added in round 2: BLOCKER #3. -->
- **`gateway_sessions` create in the popup flow** (`auth_token.rs::token` calls `sessions::create` +
  sets `__Host-zs_app_session`) and **re-create on anchor reload-recovery** (`session`/`do_refresh`),
  with the mandatory relay-email swap on the `{ user }` read path (§2.3). <!-- relay swap: MAJOR #5 -->
- **Anti-CSRF on the cookie-authenticated dispatch path** (gateway): the **`Origin` exact-match +
  `Sec-Fetch-Site`-when-present** conjunction on state-changing app requests; **no mandatory custom
  header** (raw-JS deploys pass) (§1.2). <!-- Revised in round 2: minor #8. -->
- **Internal worker→control minting endpoint** `POST /internal/power-token` (CONTROL PLANE, on the
  existing `internal.rs` router; `Authorization: Bearer <control_key>` + the **echoed gateway-signed
  `ZeroShip-User` header** the control plane verifies statelessly + `X-ZS-App-Id`; control re-derives the
  user from the signed header — **no per-request registry, no worker→gateway link, no re-entrancy into
  the gateway's in-flight dispatch**, §3.2). <!-- Rewritten in round 2: BLOCKER #1/#2/#6. -->
- **A down-scoping refresh on control's `oidc_rp`** (net-new): a method that sends `grant_type=refresh_token`
  **with `audience = CONTROL_PLANE_AUDIENCE`** (Hydra honors `audience`). It does NOT rely on Hydra `scope`
  narrowing (ory/hydra#3311 — Hydra returns the full grant); least-privilege is the mint ceiling + per-op
  `require` (§3.4). Today's `refresh_token_public` (`oidc_rp.rs:427`) sends none of these params.
  <!-- Added in round 2: MAJOR #4. -->
- **Control reads `auth.app_session_anchors` (refresh-family custody)** over the shared `auth` schema
  (`relay_revoke.rs:16` precedent), decrypting `refresh_token_enc` with the shared crypto master. This is
  a deliberate shared-custody widening (control reads/rotates a table the gateway also writes), bounded to
  the mint path. <!-- Added in round 2: the cost of locating the mint in control (BLOCKER #1). -->
- **`auth.app_power_token_cache` table** keyed by `(anchor_id, audience, scopes_hash)`, encrypted,
  TTL = power-token `exp`, **written + read by the control plane** (§3.1). Drop the anchor's single
  `cached_access_token` column + its full cascade (`Anchor`/`NewAnchor`/`create`/`update_minted`/
  `row_to_anchor`/`mint` short-circuit/`ANCHOR_MINT_CACHE_TTL_SECS`/tests; §3.1). <!-- minor #9. -->
- **Server-side power-exchange SDK** (§3): `@zeroship/auth/server` `getAccessToken({audience,scopes})`
  + `fetchAs(...)`, implemented as a **JS `fetch()` to the control mint endpoint** — NOT a native
  `env.auth` op (the native plugin is synchronous; this is async, §3.2). v1 audience class = external
  (control plane, real Hydra `aud`-narrowed token); internal-audience wrapper format is **deferred** (the
  wrapper issuer lives in the gateway, which control does not call — §3.4). Scope ceiling = denormalized
  `anchor.granted_scopes` + family-marker freshness (§4) — **no per-call `control.oauth_grants` join.**
- **`console` pseudo-app provisioning** (control seed): a route entry + `oauth_client_id` (audience ⊇
  `CONTROL_PLANE_AUDIENCE`) + `sector_identifier` for `console.zeroship.ai`, so the existing
  `/token`/`/session`/anchor/`gateway_sessions`/`pws_` paths apply to the console verbatim (§6).
  <!-- Added in round 2: MAJOR #6. -->
- **Step-up enforcement** (control plane): `step_up_required` `Action` marking + the
  recency check wired to the **existing `mfa_age_seconds` knob** (`authz_guard.rs:19`, `authz/eval.rs:172`)
  computed from the Hydra token's `auth_time` claim, + the elevated-scope-present check + the
  `403 step_up_required` challenge naming `max_age` (§5.3). **NOT** the `amr=mfa` check (later
  MFA-enablement slice). <!-- Revised in round 2: BLOCKER #3 / major #4. -->

### KEEP (verbatim or near-verbatim)
- **The anchor + server-held refresh family** (`anchors.rs`, `auth.app_session_anchors`) — now the BFF
  custody store. Encryption, single-flight rotation, no-conn-across-Hydra, 30-day/`invalid_grant`
  termination: unchanged.
- **`control.oauth_grants`** as the consent ledger; the revoke cascade; `GET/DELETE /me/oauth-grants`.
- **`env.auth` identity + scopes → `ZeroShip-User`** (the kernel contract, `WorkerUser.scopes`).
  `env.auth.{getUser,requireUser}` is **already registered + live** (`runtime/src/auth.rs`,
  `worker/cache.rs:52`) and stays **synchronous + untouched** except for identity reads. **§3's
  `getAccessToken`/`fetchAs` are NOT added to this native plugin** — they are async and live in the JS
  `@zeroship/auth/server` SDK as a `fetch()` to the control mint endpoint (§3.2). So the native surface
  does not grow; the kernel stays small. <!-- Revised in round 2: the plugin is synchronous; getAccessToken
  is JS-SDK fetch, not a native op (the "async HTTP-op in the synchronous env.auth plugin" piece). -->
- **Pairwise `pws_`** (`derive_pairwise`, §6.2) and the **relay alias** (`relay_alias_for`,
  Subsystem 5) — the identity token's `sub`/`email` reuse them directly.
<!-- Revised in round 1: addressing major #7. Audited who actually MINTS wrappers vs who CONSUMES the
     wrapper Bearer arm. The KEEP rationale was right by accident but wrong in its stated reason. -->
- **The wrapper / raw-Hydra Bearer / DPoP arms — with an honest audit of who mints vs. who consumes.**
  The mint/consume picture today:
  - **Wrapper MINT surfaces (3):** `POST /__zs/auth/token` and `GET /__zs/auth/session?mint=1` (both
    same-origin **SPA** endpoints) and `POST /__zs/auth/dpop-exchange` (`dpop_exchange.rs`, takes a raw
    Hydra Bearer + DPoP proof). The first two are the SPA-facing mints this redesign **stops handing to
    the browser**; `dpop-exchange` is the only wrapper mint a non-browser DPoP client uses.
  - **Non-browser clients (CLI, server-to-server) do NOT get a gateway wrapper from any SPA endpoint.**
    They present **raw-Hydra Bearer tokens** (the `iss != public_url` branch, `router/auth.rs`) or, for
    DPoP, go through `/dpop-exchange`. Those paths are **untouched** by this redesign. So the original
    KEEP claim ("the wrapper is still minted for CLI/server-to-server") was wrong in its reason — those
    clients were never on the SPA wrapper-mint path; they are on raw-Hydra / dpop-exchange, which
    survive unchanged.
  <!-- Revised in round 2: the internal-audience wrapper format is now DEFERRED (§3.4 — the mint runs in
       control, the wrapper Issuer lives in the gateway). So the wrapper arm's surviving consumer is
       /dpop-exchange, plus the non-browser raw-Hydra path. Re-stated honestly. -->
  - **After this change, the wrapper Bearer *verification* arm (`iss == public_url`) survives to serve
    `/dpop-exchange`-minted DPoP wrappers.** v1's server-side power tokens are **all real Hydra tokens**
    (§3.4 — the internal-audience gateway-wrapper format is deferred because the wrapper `Issuer` lives in
    the gateway and the mint now runs in control), so they ride the raw-Hydra/introspection path, not the
    wrapper arm. Non-browser clients (CLI, server-to-server) ride raw-Hydra Bearers (`iss != public_url`)
    or `/dpop-exchange` — **untouched**.
  - **Is the wrapper `Issuer` now dead code?** The wrapper **`Verifier` + Bearer arm** stay reachable via
    `/dpop-exchange`-minted wrappers. The wrapper **`Issuer`** is still used by `/dpop-exchange` itself
    (it mints the DPoP-bound wrapper), so it is **not** dead. The two **SPA** wrapper-mint call sites
    (`/token`, `/session?mint=1`) stop calling `Issuer` — but `/dpop-exchange` keeps it live. No
    truly-unreachable wrapper code is introduced; the SPA-facing browser-wrapper logic is what's removed.
    (If a future internal-audience wrapper need lands, the choice is to move/duplicate the `Issuer` into a
    control-reachable surface or add the control→gateway link — an explicit ADD, §3.4 — not part of v1.)
  - **Net:** this redesign narrows *who* gets a power token (no SPAs) and removes the SPA wrapper-mint;
    it does not delete the power-token machinery, and it leaves the non-browser (raw-Hydra + DPoP) paths
    untouched.

---

## 8. What this resolves from the architectural review

- **"Is the wrapper a capability?" tension — gone.** The original review flagged that the browser-held
  wrapper *was* a capability (a `scope`-bearing, Bearer-armed token) dressed as a privacy projection.
  The redesign removes the wrapper from the browser entirely: per the §10 Q1 decision the browser holds
  **no JWT at all** — only two HttpOnly cookies + the `{ user }` projection — and the capability lives
  server-side. There is no longer a client-held artifact that is simultaneously "just identity" and "a
  usable bearer," and (HttpOnly-only) not even an inert identity JWT in JS-reachable memory.
- **XSS blast-radius collapse.** No client-held capability ⇒ an XSS payload cannot exfiltrate a usable
  power token. The residual risk is same-origin CSRF-class action while the script runs (the BFF
  trade), not a stealable 10-minute bearer. This is the IETF BCP's "no tokens available to extract from
  the browser."
- **The "materially weaker than tokens-never-co-reside-with-app-JS" caveat in `§8.5` is retired.** That
  caveat was the accepted cost of client-held parity. Choosing the BFF tier removes the cost: the power
  token never co-resides with arbitrary creator JS.
- **The console's broad client-held authority is gone.** `BUILDER_SCOPES` as a single broad client
  grant is replaced by per-op least-privilege server-side exchange + step-up — the research's explicit
  recommendation for the highest-privilege surface.

---

## 9. Phased implementation plan (reviewable slices)

<!-- Revised in round 1: addressing minor #6 (open questions blocked the plan). The five load-bearing
     decisions (former Q1-Q5) are now RESOLVED in the body (see §10), so the phase plan no longer sits
     on top of unresolved decisions. P0 records that the decisions are made; P1-P7 are corrected to the
     decided design (gateway_sessions live arm not anchor; internal mint endpoint; HttpOnly-only; no
     amr=mfa in P7). -->

Commit-only, never push. Each slice independently testable; every regression test must fail pre-fix
(one test that exercises the *real* path — live gateway/worker/Hydra, not a shim).

- **P0 — Decisions locked (no code).** The decisions the plan is built on (round-2 corrected): identity is
  **HttpOnly-only** (no browser JWT, §2.2 / §10-D1); v1 power-token format is **external real-Hydra,
  `aud`-narrowed only** (control plane), internal-audience wrapper + generic third-party **deferred**
  (§3.4 / §10-D2); `getAccessToken` is a **JS-SDK `fetch()` to a CONTROL-PLANE mint endpoint** (NOT a
  gateway endpoint, NOT a native op), the user re-derived from the **echoed gateway-signed `ZeroShip-User`**
  (§3.2 / §10-D3); step-up v1 is **elevated-scope present + fresh `auth_time` via `max_age` re-auth, wired
  to the existing `mfa_age_seconds` knob, NO `amr=mfa`** (§5.3 / §10-D4); `requestScopes` becomes
  **re-consent → widen grant** (§10-D5).
- **P1 — Identity-token type + issuance + the typ-gate confirmation (gateway).** Add the `zs-id+jwt`
  type (reuse the ed25519 key + kid overlap); add the identity minter (held server-side, keyed to the
  gateway session); add the `/session` identity-verification path that accepts ONLY `zs-id+jwt`. Confirm
  (test, not new code) the **existing** `at+jwt` typ gate (`wrapper_token.rs:433`) rejects `zs-id+jwt`
  on the wrapper path. Tests: a `zs-id+jwt` presented as `Authorization: Bearer` on a User route → `401`
  (and on an Anon route → served anonymously, not `401`); a real `at+jwt` wrapper still verifies.
- **P2 — `/token` & `/session` go identity-only + populate `gateway_sessions` (gateway).** Add the
  `gateway_sessions.auth_time`/`amr` columns + `NewSession`/`AppSession`/`validate()` threading (BLOCKER
  #3). Rewrite `auth_token.rs::token` to (a) `sessions::create` (real shape: `user_id: &str` global UUID,
  `Option<&str>` fields, `granted_scopes: &[String]`, new `auth_time`/`amr` from the id_token) + set
  `__Host-zs_app_session`, (b) keep the anchor create, (c) return `{ user, expires_at }` with the
  **relay-swapped** email and **no `access_token`/`scope`/`id_token`**. Rewrite `session`/`do_refresh` to
  return the **relay-swapped** identity projection (sourcing `auth_time`/`amr` from the new columns) from
  the gateway session, and on reload-recovery re-create the gateway session from the anchor. Tests:
  `/token` sets BOTH cookies and its body has no JWT/`scope`; `?mint=1` rotates the family + re-establishes
  the gateway session; the raw Hydra access JWT never appears in any SPA-facing body; **the `/session` and
  `/token` `{ user }` bodies NEVER contain the real `claims.email`/`session.email` and emit `""` when no
  relay alias is active** (MAJOR #5 regression test).
- **P3 — SPA live auth via the `gateway_sessions` cookie arm + dispatch-path anti-CSRF (gateway).** The
  cookie arm (`resolve_app_session_user_header_inner`) already reads `gateway_sessions` + relay-swaps +
  emits `ZeroShip-User`; verify it authenticates SPA `/api/*` + RPC with only `__Host-zs_app_session`. Add
  the anti-CSRF conjunction (**`Origin` exact-match + `Sec-Fetch-Site`-when-present, no mandatory custom
  header**) on state-changing cookie-authenticated requests. Confirm the anchor store is NOT consulted on
  the dispatch path. Tests: an SPA request with only `__Host-zs_app_session` reaches the worker with the
  correct `ZeroShip-User`; no client bearer; `required_scopes` enforced server-side; a state-changing
  `POST /api/*` with a **mismatched/absent `Origin`** is rejected; a **same-origin raw `fetch` POST with
  no custom header is ALLOWED** (raw-JS-deploy regression); a `__Host-zs_app_anchor`-only request does NOT
  authenticate a dispatch request.
- **P4 — Server power-exchange: CONTROL mint endpoint + JS `@zeroship/auth/server`.** Add the **control
  plane** `POST /internal/power-token` on the existing `internal.rs` router (`control_key` auth + echoed
  gateway-signed `ZeroShip-User` verification + `X-ZS-App-Id`; control re-derives the user from the signed
  header, §3.2); add control's down-scoping refresh (`grant_type=refresh_token` + `audience`, NOT relying
  on Hydra scope-narrowing, §3.4); add the `auth.app_power_token_cache` table written+read by control
  (§3.1); wire control to read+rotate `auth.app_session_anchors` over the shared schema with the shared
  crypto master. Ship `@zeroship/auth/server` `getAccessToken`/`fetchAs` as a **JS `fetch()`** to that
  endpoint (NOT a native `env.auth` op; NO server-side `signOut`). Tests (faithful, real
  **worker→control→Hydra** hop — not a shim): scope ⊄ `anchor.granted_scopes` → `scope_required`; a
  revoked family → `consent_required` (family-marker check, §4); a granted scope → a real Hydra token with
  `aud=CONTROL_PLANE_AUDIENCE`, short-lived; the browser is never in the path; cache hit on
  `(anchor,audience,scopes_hash)` avoids a second Hydra round-trip; **a forged/absent `ZeroShip-User` HMAC
  → `401` (a worker cannot mint "as" another user — control verifies the gateway signature, never trusts a
  worker-named identity)**.
- **P5 — Client SDK rip-out + identity-only surface.** Remove `CacheManager`/caches/worker/locks/
  `getAccessToken*`/`hasScope`/`Session.access_token`/`Session.scopes`/any client-held `id_token`;
  `getSession`/`getUser` fetch identity from `/__zs/auth/session` (no client JWT); `react.tsx`/`useAuth()`
  updated. Tests: the client never stores or refreshes any token; `getSession()` returns identity only;
  reload-recovery re-establishes the session via the anchor (`?mint=1`).
- **P6 — Console BFF migration (via the `console` pseudo-app).** Seed the `console.zeroship.ai`
  pseudo-app route entry + `oauth_client_id` (audience ⊇ `CONTROL_PLANE_AUDIENCE`) + `sector_identifier`
  (§6) so the existing `/token`/`/session`/anchor/`gateway_sessions` paths apply. Delete
  `apps/zeroship-builder/src/server/{oauth,session}.ts`; the console SPA uses standard `@zeroship/auth`
  login + server-side `getAccessToken({audience: CONTROL_PLANE_AUDIENCE, scopes:[…]})` per-op (real Hydra
  `aud`-narrowed tokens via the §3.2 control mint) + the standard `__Host-zs_app_session` cookie; sign-out
  stays client-driven. Per-op `AuthzGuard` checks (existing introspection + `expected_oauth_audience`,
  §5.1). Tests: console browser holds no control-plane bearer + no JWT; the console-server→control hop
  carries a Bearer, NOT a cookie (host-bound `__Host-` cookies never span to `control.zeroship.ai`); a
  list call's token passes the control plane's existing Hydra introspection; broad `BUILDER_SCOPES` is
  gone (the grant is the union of per-op least-privilege scopes).
- **P7 — Step-up for destructive console ops (elevated-scope + `auth_time`, NO `amr=mfa`).**
  `step_up_required` `Action` marking + the recency check wired to the **existing `mfa_age_seconds` knob**
  (`authz_guard.rs:19`, `authz/eval.rs:172`) computed from the Hydra token's `auth_time` claim + the
  elevated-scope-present check + the `403 step_up_required` challenge naming `max_age` + the console's
  **`max_age=0`** re-auth popup (+ `prompt=consent` if widening) handler that yields a token with a fresh
  `auth_time`. Tests: deploy/secret-rotation/Stripe-onboarding/app-delete with a stale-`auth_time` or
  under-scoped token → `403 step_up_required`; after the `max_age=0` re-auth, a token with recent
  `auth_time` proceeds. **Does NOT** assert `amr=mfa` (that gate is unsatisfiable today; later
  MFA-enablement slice, §5.3). Note: step-up recency rides `max_age` (Hydra-honored), **not** scope
  narrowing (Hydra ignores it — §3.4).
- **P8 (follow-on, gated on MFA enablement) — `amr=mfa` step-up tightening.** Once the login UI can
  challenge a second factor and stamp `amr ∋ "mfa"` and `authz_guard` populates `mfa_verified` from the
  introspected `amr`, tighten the P7 gate to additionally require `amr ∋ "mfa"`. Out of scope for this
  redesign; recorded so the dependency is explicit.

---

## 10. Decisions recorded (formerly open questions) + the one remaining open item

<!-- Revised in round 1: addressing minor #6. Q1-Q5 are load-bearing for P1-P7 and are now DECIDED in
     the body; recording them as decisions (not open questions) so the phase plan does not sit on
     unresolved choices. Only the genuinely-non-blocking Q6 remains open. -->

**D1 — Identity-token transport: HttpOnly-only (decided).** The browser holds **no JWT**; identity is
exposed only via the `{ user, expires_at, auth_time, amr }` projection from `GET /__zs/auth/session`
(§2.2). The strictest BFF reading ("no tokens in the browser at all"); the cost (no client-decodable
`exp`/`auth_time`) is absorbed by the projection. §2.1 still defines the `zs-id+jwt` *shape* the gateway
signs and holds server-side.

**D2 — Power-token format by audience class (re-decided round 2, §3.4).** v1 mints **only real
Hydra-issued access tokens, `aud`-narrowed** (Hydra honors `audience` on the refresh grant). v1 audience =
**the control plane**. **Scope narrowing is NOT relied on** (RFC 8707 binds refresh tokens to the full
grant; Ory Hydra returns the full granted scope on `grant_type=refresh_token`, open issue
ory/hydra#3311) — least-privilege is the mint ceiling + per-op `require`. The round-1 **internal-audience
gateway-minted wrapper is DEFERRED** (the wrapper `Issuer` lives in the gateway, but the mint now runs in
control; an app calling its own worker already has the request's `ZeroShip-User` and needs no fresh token,
§3.4). Generic external third-party ("Token Vault") exchange is also deferred (per-third-party audience
registry).

**D3 — `getAccessToken` execution locus: CONTROL mints, worker calls via JS `fetch` (re-decided round
2, §3.2).** The worker holds no custody and has **no gateway URL** (verified `WorkerConfig`,
`main.rs:153-178`); a worker→gateway mint would be a net-new inter-service link AND re-enter the
gateway's in-flight dispatch. So the mint runs in the **control plane**, which the worker already reaches
(`control_url`/`control_key`, `internal.rs`) and which reaches the `auth` schema (`relay_revoke.rs:16`)
and Hydra (`oidc_rp`). `getAccessToken` is a **JS-SDK `fetch()`** (the native `env.auth` plugin is
synchronous — `auth.rs:95/123` — so it cannot host an async op). The user is re-derived from the **echoed
gateway-signed `ZeroShip-User` header** (HMAC+request-id+time-bound, `core/auth.rs:256`), verified
statelessly — so there is **no per-request auth-context registry** (the gateway keeps none —
`GateState`, `lib.rs:86`) and control **never trusts a worker-named identity**.

**D4 — Step-up substrate: elevated-scope + fresh `auth_time` via `max_age`, NO `amr=mfa` in v1 (decided,
§5.3).** The MFA substrate does not exist (`login.rs` stamps `amr=["pwd"]`; `authz_guard.mfa_verified` is
hardcoded `false`), so a v1 `amr=mfa` gate would be unsatisfiable and would DoS the highest-value ops. v1
checks the elevated scope is present + `auth_time` recency, sourced from the Hydra token's `auth_time`
claim and wired to the **existing `mfa_age_seconds` knob** (`authz_guard.rs:19`, `authz/eval.rs:172`).
Recency is refreshed by a `max_age=0` re-auth (Hydra-honored), **not** scope narrowing (Hydra ignores it).
The `amr=mfa` tightening is P8, gated on MFA enablement.

**D5 — `requestScopes` semantics: re-consent → widen grant (decided).** With the client holding no
power token, "request more scopes" is a **consent** action: re-run the consent popup (`prompt=consent`)
to widen `control.oauth_grants` (and the next anchor-create / family rotation widens
`anchor.granted_scopes`), after which server-side exchanges can mint the wider scope. It is NOT a
client-side "mint a wider token."

**Q6 (still open, non-blocking) — non-browser wrapper TTL / DPoP transparency.** The KEEP list preserves
the wrapper `Issuer`/`Verifier`/Bearer arm + `/dpop-exchange` for CLI/server-to-server/DPoP clients
(§7). This redesign should be **transparent** to those holders — no `WRAPPER_TTL_SECS` (10-min) or DPoP
option changes are intended. Confirm at the review gate that none are needed; this does not block any
phase (the non-browser paths are untouched code).
