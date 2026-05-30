# @zeroship/auth — in-app popup login SDK (whole-vision design)

> Status: **DRAFT / round-3 revision applied — pending human review gate.** This file is the durable
> source of truth for the `feat/auth-sdk-popup` loop. Locked decisions + grounded facts are
> preserved verbatim below the fold; the full per-subsystem design follows. Round 1 resolved the
> consent authorization-inversion, the silent-iframe third-party-cookie blocker, the
> client_id-resolution wire-format gap, the dual-rotation refresh race, the Bearer/API-key
> collision + `aud`-vs-`client_id` binding, the F4 pairwise lean, the relay feasibility/topology
> gaps, and the schema type mismatches. **Round 2** relocated the pairwise subject to a deterministic
> gateway header projection, applied the consent self-grant bypass on both the GET and POST paths,
> added gateway id_token validation, the custom-header CSRF posture, the non-authoritative
> breadcrumb, the 10-min browser access TTL + jti denylist, `oauth_client_id: Option`, and the
> pinned relay-email substitution points.
>
> **Round 3** closes three blocker-class soundness gaps round 2 still shipped, plus two real majors:
> (1) the **reload-recovery anchor is now a SEPARATE durable credential** (`auth.app_session_anchors`)
> with its own no-idle-slide, 30-day-capped-at-720h lifetime — the old plan reused
> `auth.gateway_sessions`, whose live `ABSOLUTE_HOURS=12` / `IDLE_MINUTES=30` (`sessions.rs:46-50`,
> `validate()` at :103) would kill the anchor after the first 30-min idle gap, making "return
> tomorrow" recovery impossible (§2/§8.1/§8.3); (2) the **`env.auth` plugin is pinned to BOTH
> registration sites** — the multi-tenant **worker** registers ONLY `DbPlugin` via
> `create_plugins()` (`crates/worker/src/cache.rs:40-46`), so the round-2 "same path as Kv/Storage"
> instruction would have made `env.auth.getUser()` resolve under `zeroship serve` but stay
> `undefined` for every real end-user app; AuthPlugin is stateless (reads `RuntimeState`) so it is
> unconditionally pushed at both sites and the e2e item 5 runs against the **worker** path (§1.4);
> (3) the **`/session?mint=1` rotation is made atomic and idempotent under concurrency** — round 2's
> per-tab/per-reload `?mint=1` was a second concurrent rotator of the single server family, racing
> Hydra reuse-detection; round 3 holds the anchor row lock ACROSS the Hydra refresh round-trip and
> caches the minted access token under the anchor with its own short TTL, so N concurrent minters
> yield exactly one Hydra refresh call (§1.2/§2/O3). Round 3 also: **persists the PKCE transaction in
> `sessionStorage`** (verifier/state/nonce/redirect_uri) so an opener reload mid-popup does not strand
> the flow (Auth0 `TransactionManager` parity, §4.3/§4.4); **demotes the over-built CORS apparatus**
> on the same-origin `/token`+`/session` to an exact-origin reject (no credentialed origin
> reflection, §1.2/§8.3/§8.5); makes the **jti denylist cross-node** by backing it with the
> PG-tiered `auth.token_revocations` table and making the **`(client_id, sub, revoked-after)`
> family marker the PRIMARY mechanism** (signout cannot enumerate live jtis, §8.5); makes the
> **access-token `sub` privacy-safe** by minting a per-app wrapper access token whose `sub` is the
> `pws_` so app JS cannot recover the global UUID (§1.2/§1.3/§6.2/G4); **downgrades the nonce claim**
> to honest signature+client_id binding (a client-echoed nonce is no guarantee against an XSS client,
> §1.2); **unifies the consent grant ledger** so `control.oauth_grants` and the delta-detection read
> the same store, with per-app clients NOT setting `skip_consent` (§5.2); reconciles the
> **Unknown-scope reject with the live `scope_views` render + deploy-ordering** (§5.1/§5.2); fixes the
> **expired-Bearer-on-Anon-route** regression so public pages still serve (§1.3); analyzes **COOP
> across all three popup navigation legs** with a BroadcastChannel fallback (§4.4); and gives the
> **email-alias projection a read-through cache + consent-time creation** off the hot path (§7.1).
> (See the `Added in round 3` markers.)
> Date: 2026-05-29. Branch: `feat/auth-sdk-popup` (worktree `.worktrees/auth-sdk-popup`).
> Discipline: **commit-only, never push**; orchestrate via subagents (pilot mode); every
> fix carries a regression test; tests run the real path (no shims).

---

# Part A — Locked context (preserved; do not relitigate)

## Goal

A Supabase/Auth0-style `@zeroship/auth` SDK. An end user logs into a creator app via an
**in-app popup** (no full-page redirect) using their zeroship identity, **grants that app a
declared set of permissions** via a consent screen, while each app sees only a **pairwise
pseudonymous id + relay email** for that user.

## Locked decisions (from brainstorming Q&A — do not relitigate without the user)

1. **Session model = client-held JWT (true Supabase parity).** Access + refresh tokens held
   in the browser, auto-refresh, `exchangeCodeForSession`, full client token API. User
   explicitly accepted the localStorage XSS tradeoff. → We add the Auth0 mitigation:
   **`cacheLocation: 'memory' | 'localstorage'`, default `memory`**, with silent renewal.
2. **Login UI = popup → our hosted page now; embedded `<LoginForm>` components later (Phase 2,
   separate spec).**
3. **Scope of users = creator apps' end users** (gateway sessions on `{app}.zeroship.ai`).
   NOT the console/control plane.
4. **Consent = declared per-app permission scopes** (GitHub/Slack-style). Needs a scope
   registry, manifest `scopes:` declaration, scope-aware consent UI, per-scope gateway
   enforcement, SDK scope surface.
5. **Identity = pairwise id + relay email** (Apple Hide-My-Email style). Each app sees a
   different stable-per-app `sub` AND a per-app relay/alias email. Hydra pairwise subjects +
   per-app sector salt + outbound email relay service.
6. **Per-app consent ⇒ each app is its own OAuth client.** Replaces today's single shared
   `gateway` client model for the end-user-facing flow.
7. **One comprehensive spec** covering all 5 subsystems (user chose "whole vision, one spec"),
   then implement in reviewable slices.

## Decomposition (build order)

| # | Subsystem | Delivers | Depends on |
|---|---|---|---|
| 1 | **Per-app OAuth + gateway browser-auth foundation** | per-app public PKCE client (control-plane registers on deploy); gateway same-origin `/__zs/auth/{authorize,popup-callback,token,session,signout}`; gateway **Bearer arm** → `ZeroShip-User`; fix orphaned **`env.auth`** wiring | — |
| 2 | **`@zeroship/auth` SDK** | client (popup login, first-party `/session` reload-recovery, `getSession`/`getUser`/`onAuthStateChange`/`refreshSession`/`signOut`, `cacheLocation`, rotation + cross-tab lock) + React adapter + server entry | 1 |
| 3 | **Declared permission scopes** | scope registry, manifest `scopes:`, scope-aware consent UI, per-scope gateway enforcement, SDK scope surface | 1 |
| 4 | **Pairwise subject identifiers** | per-app pairwise subjects + per-app sector salt; `ZeroShip-User.id` becomes per-app `sub` | 1 |
| 5 | **Relay email service** | per-app alias generation + outbound forwarding + lifecycle/revocation | 4 |

Consent *mechanism* lands in 1; declared-scope *content* (3) and pairwise/relay (4,5) swap in
without reshaping the SDK. Embedded components = later Phase-2 spec.

## Grounded facts (verified against code — `git grep`/Explore, 2026-05-29)

### Current architecture
- Auth = Ory **Hydra** (OIDC kernel) + bespoke server-rendered login/consent UI in
  `crates/auth/` (Askama, ntex/compio, zero-tokio). Password + Google/GitHub OAuth + magic
  link + verify + reset + consent + device + `/me`.
- End-user login today = **full-page redirect only**: gateway 302 → Hydra → consent →
  `{app}.zeroship.ai/__zs/auth/callback` → gateway mints opaque session UUID in HTTP-only
  `__Host-zs_app_session` cookie. **Token never reaches the browser.**
- Per request: gateway validates session → HMAC-signed `ZeroShip-User` header → worker.
- Three RP origins: gateway (`__Host-zs_app_session` / `auth.gateway_sessions`), control/console
  (`__Host-zs_console_session` / `auth.console_sessions`), auth UI itself
  (`__Host-zsidp_session` / `auth.sessions`).

### Fact 1 — redirect-URI model (`crates/gateway/src/oidc_rp.rs`, `dispatch.rs:1381`, `ops/auth-clients*.toml`, `docs/reference/auth.md:139`)
- Single shared `gateway` OIDC client; per-app callback URLs **appended to its `redirect_uris`
  per deploy** via `PUT /admin/clients/gateway`. Hydra = exact-match, **no wildcards** (RFC 9700).
- The per-app redirect-URI registration is **designed but NOT yet implemented** in the control
  plane (only third-party client reg exists in `bootstrap_builder.rs`/`oauth_handlers.rs`).
- ⇒ Popup callback `{app}.zeroship.ai/__zs/auth/popup-callback` is **same-origin to the app
  window** → the interactive popup redirects back to our own relay page and `postMessage`s the
  opener. **No cross-origin postMessage, no COOP/`web_message` needed.** (Background silent renewal
  is NOT done via an iframe — it is the first-party `/__zs/auth/session` anchor path; see §2.)
- For per-app consent we register **one public PKCE client per app** (not the shared gateway
  client). Control-plane must create it on app create/deploy + register its redirect URIs.
- ⚠️ **The gateway has NO Host→client_id mapping today.** It builds a SINGLE `OidcRp` with one
  hardcoded `client_id = "gateway"` (`oidc_rp.rs:42-43`, `main.rs`). `lookup_by_name` returns
  `(Uuid, Arc<CompiledRoute>)` (`sync.rs:83`); neither `RouteEntry` (`core/src/types.rs:59`) nor
  `CompiledRoute` (`sync.rs`) carries an OAuth `client_id` or `sector_identifier`. ⇒ "gateway
  injects client_id from Host" is **unbuilt wire-format plumbing**: it requires extending
  `RouteEntry` (a Key Invariant — "Wire formats are explicit contracts") with `oauth_client_id` +
  `sector_identifier`, populated by control's route-sync push and threaded through `OidcRp`. This
  is a designed mechanism, not an open question — see §1.1 + §1.5. <!-- Added in round 1: addressing BLOCKER — client_id resolution is a wire-format change -->
- ⚠️ **Back-channel logout (BCL) is already shipped** for the shared `gateway` client
  (`crates/gateway/src/backchannel_logout.rs`). It verifies the `logout_token` with `aud ==
  state.oidc_rp.client_id` (the single `"gateway"` value) and revokes **all** of a subject's
  sessions via `sessions::revoke_all_for_user(sub)`. Moving to per-app clients (locked decision 6)
  means each per-app client needs its own `backchannel_logout_uri` and the handler must disambiguate
  *which app* from the `logout_token`'s `aud` (= the per-app `client_id`). This is designed in §1.2
  (signout) + §1.1 (client shape with `backchannel_logout_uri`), not left greenfield. <!-- Added in round 1: addressing MINOR — per-app BCL already exists -->

### Fact 2 — gateway Bearer path (`crates/gateway/src/router/auth.rs`)
- Today gateway accepts **cookie sessions + DPoP only**; plain `Authorization: Bearer` is
  explicitly rejected (test `auth.rs:561-569`, `has_dpop_authorization_rejects_bearer`).
- ⚠️ **This rejection is deliberate, not an omission.** The test comment reserves the Bearer
  scheme: *"Phase 7 deliberately doesn't accept bearer; future API-key flows will live on a
  different path."* So the `Authorization: Bearer` header is an **earmarked surface** for a future
  app-API-key flow. Introducing an end-user-session Bearer arm now therefore needs an explicit
  **discriminator** so the gateway can tell "Bearer = Hydra end-user access JWT" from "Bearer = app
  API key" on the same header — see §1.3, which pins the discriminator to the token's `iss`
  (Hydra issuer ⇒ user-session arm; anything else ⇒ the reserved API-key path, 401 until that
  lands). <!-- Added in round 1: addressing MAJOR — reserved API-key Bearer path collision -->
- DPoP path (`resolve_dpop_user_header`, `auth.rs:184-359`) already implements the full
  machinery: extract token → verify (local wrapper JWT via `state.wrapper_verifier`, or Hydra
  `/oauth2/introspect`) → `build_worker_user_from_{wrapper,introspection}` →
  `oidc_rp::encode_user_header()` (`oidc_rp.rs:565-576`).
- Gateway **already holds Hydra JWKS** (`OidcRp.jwks: Arc<JwksCache>`, `oidc_rp.rs:46-79`) for
  local JWT verification. `WorkerUser<'a>` + `encode_user_header(&WorkerUser, worker_key, request_id)`
  are the handoff primitives.
- ⚠️ **`aud` is NOT the client_id today.** The shared `gateway` client sets
  `audience = ["http://api.zeroship.localhost"]` (`auth-clients-dev.toml`) — i.e. Hydra's
  access-token `aud` is the **resource server**, not the client_id. Whether a per-app *public*
  client's access JWT carries the per-app `client_id` in `aud`, in the dedicated `client_id`
  claim, or only as the resource-server `aud` is **unverified**. The per-app audience-binding
  safety property therefore must bind to the **`client_id` claim** (RFC 9068 §3 mandates `client_id`
  on access tokens and Hydra emits it), NOT to `aud`. This is pinned in §1.3 and confirmed against
  live Hydra in Slice 1c. <!-- Added in round 1: addressing MAJOR — aud vs client_id binding -->
- ⇒ Adding a **Bearer arm** in `resolve_auth()` (verify Hydra access-token JWT via JWKS;
  synthesize `ZeroShip-User`) is ~100 lines reusing existing code, plus the issuer-discriminator
  and the per-app `client_id`-claim binding. Worker handoff unchanged. Gate same as cookie (require
  `User`). Risk: plain Bearer has no sender-constraint (replay) — short TTL + HTTPS; DPoP-binding
  available as later hardening (the wrapper path already exists).

### Fact 3 — Hydra public client + token exchange (`ops/hydra-dev.yaml`, `ops/auth-clients-dev.toml`, `crates/gateway/src/dpop_exchange.rs`)
- Hydra **already** configured for public PKCE clients w/ rotating refresh tokens: PKCE enforced
  (`enforced_for_public_clients: true`, `enforced: true`), refresh TTL 720h, **global** access TTL
  1h, code TTL 60s, rotation grace 30s / reuse_count 3 (`ops/hydra-dev.yaml:49-51`). ⇒ The browser
  never holds a raw Hydra access token anyway (it gets the gateway **wrapper**, §1.2), and the
  wrapper's own `exp` is **10 min** (round-3 §8.5) — so the 1h global value is moot for the
  browser-facing path; it bounds only the server-held raw token under the anchor.
- **`strategies.access_token: jwt` is ALREADY SET** (`ops/hydra-dev.yaml`) — gateway can verify
  access tokens locally via JWKS (no per-request introspection). This **closes** the open
  question that had been flagged.
- `oidc.subject_identifiers.supported_types: [public]` is the current setting (`ops/hydra-dev.yaml:61-62`)
  — **pairwise is NOT enabled in Hydra today, and there is no `pairwise.salt` key.** Subsystem 4
  computes the pairwise sub in `crates/auth` (the locked default — see §6); Hydra-native pairwise
  is a contingent optimization, not the baseline. <!-- Added in round 1: addressing MAJOR — Hydra pairwise unverified -->
- ⚠️ **Hydra cookie config is `serve.cookies.same_site_mode: Lax`** (`ops/hydra-dev.yaml:22-23`).
  A `Lax` IdP session cookie on `auth.zeroship.ai` is **NOT sent on a cross-site, iframe-initiated
  request** (the silent-renewal iframe embedded in the app origin). Per Ory's own docs, embedding
  Hydra in a polling iframe requires `same_site_mode: None` **plus** `same_site_legacy_workaround:
  true` **plus** HTTPS — and even then Safari/Firefox third-party-cookie blocking drops it. ⇒ the
  hidden-iframe `prompt=none` silent-renewal path is **removed** from this design; reload-recovery
  is first-party-only via the server-held anchor (see §2). <!-- Added in round 1: addressing BLOCKER — silent-iframe 3p-cookie failure -->
- ⚠️ **Refresh-token reuse detection is a single-family invariant.** Hydra's rotation +
  Automatic Reuse Detection revokes the whole token family on replay of a rotated (consumed)
  refresh token. A single `authorization_code` is **single-use** — it cannot be exchanged twice to
  mint "two parallel refresh tokens." ⇒ exactly **one holder** may rotate a given refresh family.
  This design puts the rotating refresh token **server-side under the anchor only**; the browser
  holds access tokens (and, when `useRefreshTokens`, its *own* family from its *own* code exchange —
  never a copy of the server's). See §2 Sequence 2 + O3. <!-- Added in round 1: addressing MAJOR — dual-rotation reuse race -->
  See §6 for the pairwise mechanism.
- `zeroship-cli` is a live public client (`token_endpoint_auth_method="none"`, `offline_access`,
  `refresh_token` grant) — proof the shape works.
- Hydra has **no CORS** → browser cannot call `/oauth2/token` directly.
- ⇒ Gateway same-origin proxy `POST /__zs/auth/token` forwarding to Hydra `/oauth2/token`,
  mirroring the `dpop-exchange` precedent (`Cache-Control: no-store`). Caddy already proxies
  `/oauth2/*` → hydra. Gateway CORS infra exists (`crates/gateway/src/router/cors.rs`).

### env.auth (`crates/runtime/src/auth.rs`, `core/plugin.rs`, `core/runtime.rs:1182`)
- Per-request user storage IS wired (`set_request_user` in dispatch). The
  `get_user_callback`/`require_user_callback` V8 callbacks **exist** (`auth.rs:66`, `auth.rs:94`)
  but are **orphaned** — no `env.auth` plugin is registered, so `env.auth.getUser()` is `undefined`
  today and even the current SDK's server `getUser()` returns null.
- ⚠️ **Round-3 correction (BLOCKER) — the two runtime paths register DIFFERENT plugin sets, and the
  production WORKER registers ONLY `DbPlugin`.** The round-2 grounded fact ("only db/kv/storage
  register") and the round-2 wiring instruction ("same `register_plugins` path as Kv/Storage") were
  **wrong for the multi-tenant worker**, which is the path every real end-user app runs on: <!-- Added in round 3: addressing BLOCKER — worker registers ONLY DbPlugin; pin AuthPlugin to BOTH sites; AuthPlugin is stateless -->
  - **Worker** (`crates/worker/src/cache.rs:40-46`, `create_plugins()`): registers
    **`vec![DbPlugin::new(url)]`** (and an empty vec if `DB_URL` is unset). **No KvPlugin, no
    StoragePlugin, no AuthPlugin.** This is the runtime every app served through the gateway uses.
  - **CLI / `zeroship serve`** (`crates/cli/src/main.rs:108-165`): registers db (conditional on
    `DATABASE_URL`) + storage (always) + kv (redis or redb). This is the single-tenant dev path only.
  - ⇒ Following the round-2 instruction would add `AuthPlugin` to a Kv/Storage-style helper the
    worker **never calls**, so `env.auth.getUser()` would resolve under `zeroship serve` but stay
    `undefined` for every production end-user app — defeating the G5 server-SDK repair and the
    faithful-e2e gate (item 5). **AuthPlugin must be pinned to BOTH `create_plugins()` (worker) and
    the CLI plugin vector** — see §1.4.
- **AuthPlugin is stateless.** Its callbacks read the per-request user from `RuntimeState` via the
  isolate slot (`get_slot::<SharedState>()`, `auth.rs:73/106`) — it has no DB URL or config to
  construct, unlike `DbPlugin::new(url)`. So it is **unconditionally pushed** at both sites with no
  config plumbing (the worker has no auth-DB config today, and needs none).
- RPC handlers read `ctx.user` (`rpc/ctx_holder.rs`), lazy-`JSON.parse`d.

### Existing SDK + conventions
- `sdks/auth/` exists but minimal + broken: server-only `getUser/requireUser/isLoggedIn/signOut`;
  `signOut` 302s to `/__zs/auth/signout` which **has no gateway handler** (live bug); `dist/` is
  stale. No `signIn*`/`getSession`/`onAuthStateChange`.
- SDK conventions: ESM-only, tsup, ES2022, strict TS, `exports` map, `.attw.json` profile
  `esm-only`, `lint:pkg = publint && attw --pack .`, tests `node --import tsx --test test/*`.
  `zeroship` + `@zeroship/*` always external. Server/client split via **subpath exports** (copy
  `sdks/rpc`: `.`/`./server`/`./client`/`./types`; rpc tsconfig has `lib:['ES2022','DOM']`).
  React adapter ⇒ `./react` subpath + `peerDependencies.react`.

## Auth0 SDK internals to replicate (verified against `auth0-spa-js` main)
- `cacheLocation: 'memory'` DEFAULT (`InMemoryCache` = closure over a plain object),
  `'localstorage'` opt-in (warned against). Pluggable `ICache {set,get,remove,allKeys?}`.
  `CacheManager` above the backend owns keying + expiry. Key
  `@@zsauth@@::<client_id>::<audience>::<scope>`; id-token/user profile is a SEPARATE `@@user@@`
  entry. `expiresAt = floor(now/1000)+expires_in`; on read, evict expired UNLESS a refresh token
  is present.
- Web Worker holds refresh tokens in worker-isolated memory and strips `refresh_token` from
  main-thread responses, ONLY when `window.Worker && useRefreshTokens && cacheLocation==='memory'`.
- `getTokenSilently()`: dedupes concurrent calls (single in-flight promise). **We do NOT
  replicate Auth0's hidden-iframe `prompt=none` fallback** — it depends on reading Hydra's IdP
  session cookie from an iframe embedded in the app origin, i.e. a third-party cookie on
  `auth.zeroship.ai` (Lax by config; Safari/Firefox block it regardless). Our renewal paths are
  (a) browser refresh-grant when `useRefreshTokens`, and (b) the **first-party server-held anchor**
  via `GET /__zs/auth/session` (no iframe, no cross-site cookie). The only time we navigate the
  popup to Hydra is the **interactive** login/step-up flow (a top-level navigation inside the
  popup, where the IdP session cookie *is* first-party to `auth.zeroship.ai`). <!-- Added in round 1: addressing BLOCKER — drop silent iframe -->
  We replace Auth0's `web_message` response mode with our **same-origin relay page** for that
  interactive popup — no Hydra extension needed.
- Refresh rotation: `grant_type=refresh_token`; one-time-use + Automatic Reuse Detection revokes
  the family on replay; Rotation Overlap Period tolerates immediate retry. **Hydra already
  configures this.** Cross-tab serialization via `navigator.locks` (5s `AbortController`) with a
  legacy `browser-tabs-lock` fallback — replicate (single-use rotated tokens require it).
- Session-presence: non-HttpOnly breadcrumb cookie written after every successful token request,
  removed on logout. `checkSession()` early-returns with no network if the cookie is absent;
  `isAuthenticated()` is cache-derived. (We key the breadcrumb on the gateway-returned `app_ref`,
  not Auth0's `clientId` — see §4.3.)
- React parity: `AuthProvider` builds one client; on mount, if URL has `code+state` →
  `handleRedirectCallback` then strip query via `history.replaceState`; else `checkSession()`.
- `loginWithPopup`: open the popup SYNCHRONOUSLY (`window.open('')` inside the click handler,
  BEFORE the async authorize-URL build) to dodge blockers; 400×600 centered; then set
  `popup.location.href`; poll `popup.closed` every 1000ms (PopupCancelled), 60s timeout.

---

# Part B — Full design

## 1. Overview & goals

`@zeroship/auth` becomes a two-surface package: a **browser client** (Supabase/Auth0 parity:
client-held tokens, popup login, silent renewal, `onAuthStateChange`) and a **server helper**
(`getUser`/`requireUser` on top of the gateway's `ZeroShip-User` header). The platform side
adds a same-origin OAuth foundation on the gateway, a per-app public PKCE client, declared
permission scopes with consent + enforcement, pairwise pseudonymous subjects, and an outbound
email relay.

### Goals
- **G1 — In-app popup login.** End users log into a creator app without leaving it. The popup
  hosts our login + consent UI; the app window receives a `code` via same-origin `postMessage`
  and exchanges it for a session.
- **G2 — Client-held session (Supabase parity).** Access + refresh tokens live in the browser,
  default in-memory, auto-refreshed, with the full `getSession`/`getUser`/`refreshSession`/
  `onAuthStateChange`/`signOut` surface.
- **G3 — Declared per-app scopes.** Apps declare permission scopes; the consent screen renders
  them; the gateway enforces them on the access token; the SDK exposes them.
- **G4 — Privacy by default.** Each app sees a per-app pairwise `sub` and a per-app relay email,
  never the global user id or real inbox — **including in the access token the browser holds**: the
  browser receives a gateway **wrapper** token whose `sub` is the `pws_` (§1.2), so app JS decoding
  its own access token cannot recover the global UUID. The raw Hydra access JWT (global UUID `sub`)
  never leaves the gateway. <!-- Added in round 3: addressing MAJOR — G4 holds on the Bearer path because the browser-held token is the pws_ wrapper, not the global-UUID Hydra JWT -->
- **G5 — No new native protocol surface.** The browser client is pure JS (npm package). The only
  Rust changes are gateway endpoints, the `env.auth` plugin, the per-app client lifecycle in
  control, pairwise computation in auth, and the relay handler — all "drivers", not new kernel
  syscalls beyond `env.auth` (which was always planned).

### Non-goals (this spec)
- Embedded `<LoginForm>` credential components (Phase 2, separate spec — Phase 1 keeps credential
  entry on the hosted page).
- Console / control-plane end-user auth (out of scope per locked decision 3).
- Two-way relay reply routing (v1 relay = app→user forwarding only).
- Mandatory DPoP binding for the browser client (the wrapper path exists; plain Bearer + short
  TTL is the Phase-1 posture, DPoP-binding is documented as hardening).

### Design invariants honored
- **Gateway is dumb**: the new endpoints do OAuth plumbing (build URL, proxy token, relay code,
  verify JWT) and forward; no app logic.
- **Native primitives are the kernel**: the browser SDK adds zero Rust. `env.auth` is the single
  planned native namespace we finally register.
- **Pre-launch, no back-compat**: the per-app-client model *replaces* the shared `gateway` client
  for the end-user flow in the same change; no `@deprecated` shims. The redirect flow's
  `/__zs/auth/callback` stays (it's a different, still-valid path), but the SDK is the new
  primary surface.
- **typed_id everywhere**: per-app client id derives from `app_id`; pairwise subjects are stored
  with `pws_…` typed ids; identity-mapping rows use `usr_…`/`app_…`.
- **Zero tokio**: all gateway/auth/control work stays on compio/ntex.

## 2. Architecture — the same-origin transport spine

### Transport principle

Tokens are **held client-side** (memory default) but **acquired and refreshed through
same-origin gateway endpoints** on `{app}.zeroship.ai`. The browser never makes a *background*
cross-site request to Hydra. This gives Supabase-style client tokens while sidestepping ITP /
third-party-cookie breakage: every **background** network hop (token exchange, refresh,
reload-recovery) is first-party to the app origin.

> **The one cross-site hop is interactive and top-level, by design.** During an *interactive*
> login or step-up, the **popup** navigates to `auth.zeroship.ai` (Hydra + our login/consent UI).
> That is a **top-level** navigation in the popup window, so Hydra's `Lax` IdP session cookie is
> first-party to `auth.zeroship.ai` and works in all browsers. We deliberately **do not** use a
> hidden iframe to read that session in the background — a `prompt=none` iframe embedded in the app
> origin would need Hydra's session cookie as a *third-party* cookie (Lax by config, blocked by
> Safari/Firefox), which is the exact ITP failure mode we refuse to depend on. Reload-recovery is
> therefore **first-party-only** via the server-held anchor (Sequence 2), never an iframe. <!-- Added in round 1: addressing BLOCKER — reconcile "no 3p cookie" with the removed iframe path -->

The browser runs a real **public-client PKCE** flow: the SDK generates the verifier and submits it
to the same-origin token proxy. ⚠️ **Round-3 fix (MAJOR) — the in-flight PKCE transaction is
persisted in `sessionStorage`, regardless of `cacheLocation`.** The verifier/state/nonce/redirect_uri
must survive an **opener reload mid-popup** (the user clicks the app, or a slow/mobile provider
round-trip reloads the page while the popup is open). If those lived only in the in-memory flow map,
an opener reload would strand the code with no recoverable verifier (token exchange fails) and the
relay `postMessage` would arrive at a window with an empty flow map (`invalid_state`). The fix
mirrors Auth0-spa-js's `TransactionManager`, which persists the transaction
(`login-code-verifier-<state>` etc.) in `sessionStorage` precisely to survive this — see §4.3/§4.4.
The gateway forwards to Hydra's `/oauth2/token` (which the browser can't reach due to no CORS). <!-- Added in round 3: addressing MAJOR — persist the PKCE transaction in sessionStorage so an opener reload mid-flow does not strand the verifier -->

```
   Browser (app origin)            Gateway ({app}.zeroship.ai)            Hydra (auth.zeroship.ai)
   ────────────────────            ───────────────────────────            ────────────────────────
   @zeroship/auth client    ──►   GET  /__zs/auth/authorize      ──►   302 /oauth2/auth
                            ◄──   302 to Hydra (or login UI)
   popup (top-level nav)    ──►   (login + consent on auth UI)   ──►   /oauth2/auth ... callback
                            ◄──   GET  /__zs/auth/popup-callback  ◄──   302 back with code+state
   postMessage(code,state)  ◄──   same-origin relay HTML page (opener only)
   exchangeCodeForSession   ──►   POST /__zs/auth/token          ──►   POST /oauth2/token (code+PKCE)
                            ◄──   {access,id,expires_in}          ◄──   tokens (server keeps refresh)
   Bearer <access>          ──►   any app route (Bearer arm)     →     worker (ZeroShip-User)
   --- reload ---
   checkSession()           ──►   GET  /__zs/auth/session?mint=1  →    (first-party anchor; no iframe)
                            ◄──   {user, access, expires_at}
```

### Endpoint surface (gateway, per app host)

| Method | Path | Purpose |
|---|---|---|
| GET  | `/__zs/auth/authorize` | Build the Hydra `/oauth2/auth` URL (per-app client resolved from `Host`, PKCE challenge, state, nonce, scopes, prompt) and 302. **Interactive popup only** (top-level navigation in the popup window). The `prompt=none` silent-iframe variant is removed — see §2. |
| GET  | `/__zs/auth/popup-callback` | Same-origin HTML relay page: reads `code`/`state` (or `error`) and `postMessage`s `{type:'zs:authorization_response', response:{code,state}}` to `opener`, target origin = own origin. CSP `default-src 'none'; script-src 'unsafe-inline'; frame-ancestors 'self'`, `Referrer-Policy: no-referrer`, `COOP: same-origin`. |
| POST | `/__zs/auth/token` | **Same-origin-only** proxy to Hydra `/oauth2/token` (`authorization_code` with `code`+`code_verifier`, OR `refresh_token`). `Cache-Control: no-store`. The security boundary is the custom-header + `Origin` + `Sec-Fetch-Site` conjunction (§1.2), **not** CORS; a foreign `Origin` is rejected outright (no credentialed origin reflection — round-3). Returns `{access_token,refresh_token?,id_token,expires_in,token_type}`. Sets the HttpOnly `__Host-zs_app_session` anchor cookie + stores the server-held refresh family. |
| GET  | `/__zs/auth/session` | From the first-party anchor cookie, return the current user and (with `?mint=1`) a fresh access token minted from the **server-held** refresh family. The SOLE reload-recovery path (no iframe, no 3p cookie). Gated by the `zs.<host>.is.authenticated` breadcrumb. |
| POST | `/__zs/auth/signout` | **Fix the missing handler.** Revoke tokens (Hydra `/oauth2/revoke`), clear anchor + breadcrumb, optional Hydra RP-logout. `scope: local|global`. |

Existing `/__zs/auth/callback` (legacy redirect flow) and `/__zs/auth/dpop-exchange` are
unchanged.

### Sequence 1 — popup login (interactive)

```
User clicks "Sign in"
  │
  ├─ SDK: window.open('', 'zs:auth', '400x600 centered')   [SYNC — before any await]
  ├─ SDK: generate verifier+challenge(S256), state, nonce; PERSIST {verifier,state,nonce,redirect_uri}
  │        in sessionStorage (keyed by state) + in-memory flow map (survives an opener reload, §4.3)
  ├─ SDK: authorizeUrl = `${appOrigin}/__zs/auth/authorize?` + {client_id_implicit, challenge,
  │        state, nonce, scope}     (NO prompt — let Hydra's SSO skip fire; client_id gateway-side)
  ├─ SDK: popup.location.href = authorizeUrl
  │
  ▼ popup
  gateway GET /__zs/auth/authorize  → 302 → Hydra /oauth2/auth
  Hydra → 302 → auth UI /login (no IdP session) → password/OAuth → accept_login
  Hydra → consent: declared scopes rendered (Subsystem 3) → accept_consent
  Hydra → 302 → gateway GET /__zs/auth/popup-callback?code=…&state=…
  gateway returns same-origin relay HTML
  relay: window.opener.postMessage({type:'zs:authorization_response',
                                    response:{code, state}}, location.origin)
  relay: window.close()
  │
  ▼ opener (app window)
  SDK message handler: validate e.origin === appOrigin && e.data.type === 'zs:authorization_response'
  SDK: match state → recover verifier from flow map → exchangeCodeForSession(code)
  SDK: POST ${appOrigin}/__zs/auth/token {grant_type:'authorization_code', code, code_verifier,
        redirect_uri:`${appOrigin}/__zs/auth/popup-callback`, mode:'server_anchor'}
  gateway → POST Hydra /oauth2/token → {access, refresh, id, expires_in}
  gateway: store refresh family + family-id under __Host-zs_app_session anchor (HttpOnly);
           respond JSON WITHOUT refresh_token (server_anchor mode); return app_ref
  SDK: cacheManager.set(access + server-validated `user`, app_ref); breadcrumb is set server-side
       on the /token response (SDK may also set it); emit SIGNED_IN  (SDK never decodes the id_token)
       (browser_refresh mode only: SDK runs a SECOND authorize→code→exchange for its own
        refresh family; that refresh_token is forwarded to the Web Worker, never co-mingled
        with the server family)
```

### Sequence 2 — first-party reload-recovery (memory cache, page reload)

```
Page reload → memory cache is empty
  │
  ├─ SDK checkSession():
  │    if cookie zs.<host>.is.authenticated absent → return (anonymous, no network)
  │    else:
  │      GET /__zs/auth/session?mint=1   [first-party anchor cookie ride-along, same-origin]
  │        gateway: parse __Host-zs_app_session → validate auth.app_session_anchors row
  │          (the DEDICATED anchor store — NOT auth.gateway_sessions; see below + §8.1)
  │          if valid && not expired:
  │            LOCK the anchor row, then:
  │              if a cached unexpired access token is stored under the anchor → return it (no Hydra call)
  │              else: mint a fresh access token from the SERVER-HELD refresh token under the anchor
  │                    (Hydra /oauth2/token grant_type=refresh_token; rotation stays server-side),
  │                    store {new refresh, new access, access_exp} under the anchor, then unlock
  │            → 200 { user, access_token, expires_at, scopes }
  │          else (anchor missing/expired):
  │            → 401 { error: "login_required" }
  ▼ on 200: cache the access token in memory, emit SIGNED_IN
  ▼ on 401: clear breadcrumb, stay anonymous (SDK surfaces login_required; app prompts interactive login)
```

⚠️ **Round-3 fix (BLOCKER) — the reload-recovery anchor is a SEPARATE durable credential, NOT a
reuse of `auth.gateway_sessions`.** The live interactive session store hardcodes `IDLE_MINUTES = 30`
and `ABSOLUTE_HOURS = 12` (`crates/gateway/src/sessions.rs:46/50`) and `validate()` (`:103`) rejects
any row whose `idle_expires_at` **or** `abs_expires_at` is in the past, sliding only the 30-min idle
window. A 30-day reload-recovery anchor backed by that store is a **broken contract**: the absolute
lifetime kills it at 12h, and — worse — the 30-min idle timeout kills it after *any* 30-min gap
between visits, which is the normal "close the tab, come back tomorrow" reload-recovery case. The
breadcrumb would say authenticated while `/session?mint=1` returns `401 login_required` the next
morning — the exact desync this design claims to prevent. So round 3 gives the anchor its **own**
store, `auth.app_session_anchors` (DDL §8.1), with **anchor-specific lifetime semantics**: <!-- Added in round 3: addressing BLOCKER — anchor is a separate store with no idle-slide; reconcile against the live 12h/30min gateway_sessions model and the 720h refresh ceiling -->

- **No 30-min idle expiry.** A reload-recovery anchor exists precisely to survive long idle gaps; it
  has **no idle window** — only an absolute lifetime.
- **Absolute lifetime = 30 days, capped at the Hydra refresh-family ceiling (720h ≈ 30d).** The
  anchor's absolute expiry is `min(created_at + 30d, refresh_family_expiry)`. Because Hydra's
  refresh family is `720h` (`ops/hydra-dev.yaml:55`) and its rotation **slides** the family on each
  successful refresh (each `/session?mint=1` rotates and re-anchors the 720h window), an actively-used
  anchor renews its underlying family on every mint, so the practical ceiling is "30 days of total
  inactivity OR the refresh family being revoked," whichever first. At the 720h family ceiling with
  no intervening mint, the next `?mint=1` gets `invalid_grant` from Hydra → the gateway deletes the
  anchor → `401 login_required` → interactive login. This is stated, not left implicit.
- **The 12h/30-min `auth.gateway_sessions` constants and every consumer/test that asserts them are
  UNTOUCHED.** The interactive cookie redirect flow (`/__zs/auth/callback`) keeps using
  `gateway_sessions` with its 12h/30-min semantics; only the new SDK anchor uses
  `app_session_anchors`. No `sessions.rs` constant changes, so the `validate()`-slide tests stay
  green.
- **Regression test (round 3):** create an anchor, advance the clock **> 30 min** with no activity,
  reload, and assert `/session?mint=1` still recovers (would fail against the old 30-min-idle
  `gateway_sessions` model); a second test advances **> 30 days** and asserts `401 login_required`.

**There is no hidden-iframe fallback.** The anchor cookie (`__Host-zs_app_session`) is the SOLE
durable first-party reload-recovery credential — it survives memory-cache loss without any
third-party cookie or cross-site iframe. If the anchor is gone (>30d idle, family revoked, or signed
out elsewhere) the only recovery is an **interactive** popup login. <!-- Added in round 1: addressing BLOCKER — single first-party recovery path -->

**Single-holder refresh invariant.** The rotating refresh-token family that backs the anchor lives
**only server-side** (encrypted under the `auth.app_session_anchors` row). The browser never holds a
*copy* of that token. When `useRefreshTokens` is enabled the browser additionally runs its *own* code
exchange to obtain its *own* independent family (see §1.2) — two **separate** families from two
**separate** codes, each rotated by exactly one holder, so Hydra's reuse detection never fires on a
browser-vs-server race. When `useRefreshTokens` is off (the default), only the server family exists
and every fresh access token comes from `/__zs/auth/session?mint=1`. ⚠️ **Round 3 closes the
remaining same-family race:** multiple tabs/reloads all call `/session?mint=1` against the *same*
server family, so the server family itself needs a single rotator. The mint path therefore holds a
**PostgreSQL advisory lock keyed on the anchor id across the Hydra refresh round-trip** and caches
the minted access token under the anchor, so N concurrent minters produce exactly one Hydra rotation
and share the result — no dual-rotation, no reuse-detection trip (see `GET /__zs/auth/session`
below). <!-- Added in round 3: addressing BLOCKER — same-server-family concurrent mint is serialized by an anchor-id advisory lock -->

> **Why not share one refresh token?** If the browser and server both held the same rotating token,
> whichever refreshed first would rotate it and the other's copy would trip Automatic Reuse
> Detection, revoking the family on the very first reload-then-refresh. `navigator.locks` cannot
> serialize across the browser/server boundary. Hence two independent families (or, by default,
> server-only).

### Sequence 3 — token refresh (rotation)

```
getAccessToken*() finds entry expiring within skew (60s)
  │
  ├─ navigator.locks.request(`zs.refresh.<host>`, {signal: AbortController(5s)}, async () => {
  │    re-read cache under lock (another tab may have refreshed)
  │    if still stale:
  │      if useRefreshTokens && browser holds its own refresh family:
  │        POST /__zs/auth/token {grant_type:'refresh_token', refresh_token}   // browser family
  │        gateway → Hydra /oauth2/token (rotation: new refresh issued, old one one-time-use)
  │        cacheManager.updateEntry(old, new)        [atomic rewrite]
  │      else (default — no browser refresh token):
  │        GET /__zs/auth/session?mint=1             // server family mints; browser never rotates it
  │        cacheManager.setAccessToken(new)
  │      emit TOKEN_REFRESHED
  │  })
  │
  └─ On invalid_grant (browser family reuse-detected → revoked):
       drop browser tokens; fall back to GET /__zs/auth/session?mint=1 (server family still live);
       if that 401s → emit SIGNED_OUT, surface AuthError{code:'invalid_grant'}
```

`navigator.locks` serialization is mandatory **within the browser** for the browser's own refresh
family: rotated refresh tokens are single-use, so two tabs refreshing concurrently would trip
Hydra's reuse detection and revoke that family. The lock key is `zs.refresh.<host>` (the SDK keys
on host, not the unknown client_id — see §4.3). The **server** refresh family is rotated only by
the gateway under its own DB row lock, never by the browser, so the two families never collide.
<!-- Added in round 1: addressing MAJOR — refresh modes; MINOR — host-keyed lock -->

## 3. Subsystem 1 — per-app OAuth + gateway browser-auth foundation

### 1.1 Per-app public PKCE client in Hydra

Each app gets its own Hydra client, created/reconciled by the **control plane**.

- **client_id**: `app_<base62-app-id>` (deterministic from `app_id`; one client per app, stable
  across deploys). Derivation lives next to `registry.rs`/`api.rs`.
- **token_endpoint_auth_method**: `none` (public).
- **grant_types**: `["authorization_code", "refresh_token"]`.
- **response_types**: `["code"]`.
- **scope**: `"openid offline_access profile email"` + the app's declared custom scopes
  (Subsystem 3 mirrors them into this allowlist).
- **redirect_uris**: `[{scheme}://{host}/__zs/auth/popup-callback, {scheme}://{host}/__zs/auth/callback]`
  for every host the app serves (apex + custom domains). Exact-match (RFC 9700) — registered per
  host, no wildcards. ⚠️ **Round-3 fix (MINOR) — bound the array and address Hydra re-validation /
  per-deploy PUT cost.** Two-per-host growth (2×N) is bounded by a **per-app custom-domain cap
  (default 50 domains ⇒ ≤102 redirect_uris)**, well within Hydra's per-client `redirect_uris`
  tolerance (Hydra stores them as a JSON array and validates by exact-match lookup; ~100 entries is
  not a performance concern). The control plane recomputes the **full set idempotently** on each
  domain change and `PUT`s it only when it differs (diff-then-PUT, so a no-op deploy is a no-op PUT);
  concurrent domain attaches are serialized by the same `control.app_oauth_clients` row update. We do
  **not** register a wildcard. The same `sector_identifier_uri` churn concern that pushed us away from
  Hydra-native pairwise (below) does **not** apply to `redirect_uris` because the baseline client is
  `subject_type: public` — Hydra does **not** fetch/re-validate a sector document for a public client,
  so a `redirect_uris` PUT is a cheap local validation, not an outbound JSON fetch. (If F4-A
  Hydra-native pairwise is ever adopted, a **single stable platform-controlled callback origin with a
  signed return-target** — one redirect_uri, sector-stable — is the documented escape from both the
  2×N growth and the sector re-validation; that is the S2 spike's design, not the baseline.) <!-- Added in round 3: addressing MINOR — bound redirect_uris growth, idempotent diff-then-PUT, note public-client has no sector re-validation, signed-return-target escape hatch -->
- **subject_type**: `public` (the **locked default**). The per-app pairwise `pws_…` sub is a
  **gateway header projection** (§6.2, F4-B), **not** computed by Hydra and **not** set at
  `accept_consent` (round 2 relocated it — the subject is bound to the global UUID at `accept_login`).
  We do NOT set Hydra `subject_type: pairwise` / `sector_identifier_uri` in the baseline, because
  Hydra **requires** a `sector_identifier_uri` whenever a pairwise client has >1 redirect_uri (it does
  here: popup-callback + callback, ×N hosts) and re-fetches/re-validates that JSON document on every
  redirect_uri mutation — which fights per-deploy redirect_uri churn. With `subject_type: public`
  there is **no** sector document and thus no such re-validation. <!-- Added in round 1: addressing MAJOR — sector_identifier_uri vs redirect_uri churn; Updated in round 3: pws_ is the gateway projection, not accept_consent; public client has no sector re-validation -->
- **post_logout_redirect_uris**: `[{scheme}://{host}/]`.
- **backchannel_logout_uri**: `{scheme}://{host}/oidc/backchannel-logout` — **per-app** BCL
  endpoint (the existing handler is updated to disambiguate by the `logout_token`'s `aud` = this
  app's `client_id`; see §1.2 signout). Without this each per-app client would have no BCL
  registration and global signout would silently no-op. <!-- Added in round 1: addressing MINOR — per-app backchannel_logout_uri -->

**Lifecycle** (control plane, `crates/control/src/`):
- **On app create** (`api.rs:create_app`): create the Hydra client (`POST /admin/clients`) with
  the apex host's redirect URIs. Reuses the exact `HydraCreateClientRequest` shape already in
  `oauth_handlers.rs` / `bootstrap_builder.rs`.
- **On deploy / domain attach** (`api.rs:deploy`, custom-domain handler): `PUT /admin/clients/<id>`
  to add/remove redirect URIs for the new host set. **This is the designed-but-unimplemented
  per-app redirect registration — implement it here.**
- **On app delete**: `DELETE /admin/clients/<id>` + cascade pairwise/relay cleanup (Subsystems 4,5).
- New control module `crates/control/src/app_oauth_client.rs` holds `ensure_app_client(app)`,
  `sync_app_redirect_uris(app, hosts)`, `delete_app_client(app_id)`; called from the app/deploy
  handlers. Idempotent (upsert), mirroring `bootstrap_builder.rs`.

The control plane stores client bookkeeping in `control.app_oauth_clients` (DDL in §8) so it can
reconcile without re-deriving from Hydra.

### 1.2 Gateway same-origin endpoints

New module `crates/gateway/src/browser_auth.rs` (sibling to `dpop_exchange.rs`), one async ntex
handler per endpoint, registered in `crates/gateway/src/main.rs` alongside the existing
`/__zs/auth/dpop-exchange` resource. All reuse `state.oidc_rp` (Hydra dial URL, JWKS, introspect),
`state.config` (insecure_dev, public_url, worker_key), and `oidc_rp::encode_user_header`.

#### `GET /__zs/auth/authorize`

Query params (from the SDK): `code_challenge`, `code_challenge_method=S256`, `state`, `nonce`,
`scope` (space-delimited), `prompt` (**optional** — omitted in the common case; `login` or
`consent` only for explicit step-up; **`none` is not supported**, since the silent-iframe path is
removed), `redirect_uri` (must be one of the app's registered URIs; defaults to `.../popup-callback`).

⚠️ **Round-2 fix (MINOR) — the popup login does NOT default to `prompt=login`.** Hydra's SSO/skip
path (`login.rs:90-120`, gated by the `remember` flag) only fires when no `prompt=login` is present;
forcing `prompt=login` on every interactive login would re-prompt for credentials even when the
`auth.zeroship.ai` IdP session is live — defeating SSO and the `remember` flag (and making a
"popup login" show a password screen to an already-logged-in user). So the default is **prompt
omitted**: an already-logged-in user gets a seamless re-login (Hydra skips straight to consent or
all the way through if consent is remembered), matching Auth0/Supabase popup behavior.
`prompt=login` is reserved for explicit step-up/re-auth (`getAccessTokenWithPopup` with a
re-auth flag); `prompt=consent` for incremental scope deltas (`requestScopes`, §5.2). <!-- Added in round 2: addressing MINOR — omit prompt by default so Hydra SSO skip fires; reserve prompt=login for step-up -->

The gateway resolves the app's `oauth_client_id` from the request `Host` via the **route cache**
(`RouteEntry.oauth_client_id`, the new wire-format field — see §1.5), then 302s to Hydra
`/oauth2/auth` with:
`client_id, response_type=code, code_challenge, code_challenge_method=S256, redirect_uri,
scope, state, nonce, prompt`. The gateway does **not** mint a stash cookie here (PKCE verifier is
held by the browser, Supabase-style) — this is the key divergence from the existing
`build_authorize_redirect`, which stashes the verifier server-side for the redirect flow.

Add `OidcRp::build_browser_authorize_url(client_id, params) -> String` in `oidc_rp.rs` (no stash).

```
GET /__zs/auth/authorize?code_challenge=…&code_challenge_method=S256&state=…&nonce=…
                         &scope=openid%20profile%20read:billing     (no prompt — SSO skip fires)
→ 302 Location: https://auth.zeroship.ai/oauth2/auth?client_id=app_7Fk…&response_type=code&…
```

#### `GET /__zs/auth/popup-callback`

Returns a tiny same-origin HTML document (no app code). ⚠️ **Round-2 fix (MINOR) — the inline
script uses a per-response CSP nonce, not `'unsafe-inline'`.** Response headers:
`Content-Security-Policy: default-src 'none'; script-src 'nonce-<random>'; frame-ancestors 'self'`,
`Referrer-Policy: no-referrer`, `Cross-Origin-Opener-Policy: same-origin`. The gateway mints a fresh
base64 nonce per response and stamps it on both the CSP header and the `<script nonce>`; this keeps
the page tamper-evident — a future edit that reflected any query param into the DOM under
`'unsafe-inline'` would be an XSS sink, whereas under a nonce an injected script tag would be
blocked. The `frame-ancestors 'self'` restricts who may embed the page; `no-referrer` keeps the
`code` out of any `Referer`. A test asserts **no query param is ever reflected into the response
body**. Inline script (postMessages to the **opener** only — the popup is the only supported
launcher in Phase 1): <!-- Added in round 2: addressing MINOR — CSP nonce instead of 'unsafe-inline' + no-reflection test -->

```html
<!doctype html><meta charset=utf-8><title>…</title><script nonce="<random>">
(function(){
  var p = new URLSearchParams(location.search);
  var state = p.get('state');
  var msg = { type: 'zs:authorization_response', response:
    p.get('error')
      ? { error: p.get('error'), error_description: p.get('error_description'), state: state }
      : { code: p.get('code'), state: state } };
  // Primary: postMessage to the opener (same-origin target).
  try { if (window.opener) window.opener.postMessage(msg, location.origin); } catch (e) {}
  // Round-3 fallback (opener may be severed by COOP across the app→auth→app legs, §4.4):
  // both channels are SAME-ORIGIN, so no cross-origin exposure.
  try { new BroadcastChannel('zs:auth').postMessage(msg); } catch (e) {}
  try {
    localStorage.setItem('@@zsauth@@::relay::' + state, JSON.stringify(msg));
    localStorage.removeItem('@@zsauth@@::relay::' + state);   // fire a one-shot storage event
  } catch (e) {}
  window.close();
})();
</script>
```

`location.origin` as the postMessage target means only the same-origin app window can read it; the
BroadcastChannel and `localStorage` fallbacks are likewise same-origin. The SDK validates
`e.origin === appOrigin` (for postMessage) and matches `state` (all channels) on receipt. No DOM
writes (so the reflected `error_description` is never an XSS sink). ⚠️ **Round-3 (MINOR): the page
does NOT assume `window.opener` survives COOP** — the same-origin BroadcastChannel/`localStorage`
relay covers the Safari/COOP opener-severed case (§4.4). The `script-src 'nonce-…'` CSP permits this
inline script (it is nonce-tagged) and still blocks any injected/reflected script.

**Cross-origin-iframe-embedded apps are explicitly unsupported in Phase 1.** If a creator embeds
their app inside a *cross-origin* parent frame, `window.opener` is the popup's opener (the app
window, same-origin — fine), but a creator who tries to run the SDK *inside* a cross-origin iframe
will see the SDK's `e.origin === appOrigin` check drop the message. The SDK detects this at init
(`window.top !== window.self && document.referrer-origin !== appOrigin`) and rejects with a
distinct `AuthError('config_error', 'cross-origin iframe embedding unsupported')` rather than only
timing out at 60s. <!-- Added in round 1: addressing MINOR — cross-origin iframe diagnostic -->

#### `POST /__zs/auth/token`

⚠️ **Round-3 fix (MAJOR) — `/__zs/auth/token` is SAME-ORIGIN-ONLY; CORS is NOT the security
boundary and credentialed origin-reflection is removed.** The SDK calls `${appOrigin}/__zs/auth/token`
from the app page, i.e. **same-origin** — so the browser never consults CORS on the happy path, and
cross-origin SDK use is explicitly unsupported (§1.2). Round 2's "CORS-enabled, allow-origin =
reflected app origin, `allow-credentials: true`" was both dead weight on the happy path **and** the
classic dangerous pattern: origin-reflection + `allow-credentials` turns any reflection bug (a
subdomain match, an `Origin: null`, a Host-spoofed value) into a credentialed cross-origin
token-mint oracle. Round 3 therefore: <!-- Added in round 3: addressing MAJOR — drop credentialed origin reflection; same-origin-only; reject foreign/null/missing Origin; CORS is not the boundary -->

- **Emits NO CORS allow-origin / allow-credentials headers** on `/token` and `/session`. A
  cross-origin request gets no `Access-Control-Allow-Origin`, so the browser blocks the response —
  the endpoints behave as same-origin-only by omission, which is the correct posture for a
  same-origin endpoint.
- **Rejects any request whose `Origin` is present and not exactly the app's own origin**, and
  rejects `Origin: null` and (for state-changing POST) a missing `Origin` on browsers that send it.
  Exact-string compare against the app host; **never** a substring/subdomain match, never reflection.
- The **anchor cookie is `SameSite=Strict`** and IS sent on these same-origin fetches (a Strict
  cookie rides same-origin requests), so credentials still flow for the legitimate same-origin call
  **without** any `allow-credentials` CORS handshake. Note `/token` SETS the anchor and does not need
  it on input; `/session?mint=1` reads it.

The actual CSRF/abuse defense is the conjunction of three checks, **all required** (these, not CORS,
are the boundary): <!-- Added in round 2: addressing MAJOR — /token CSRF is the custom-header + Origin + Sec-Fetch-Site conjunction, not SameSite; document the Sec-Fetch-Site matrix -->

1. **Custom non-simple header `X-ZS-Auth: 1`** (the SDK always sets it). A cross-site `<form>` or
   simple POST cannot set a custom header without a CORS preflight, and our CORS allowlist only
   reflects the app's own origin — so a foreign origin's preflight fails and the request never
   arrives. This is the primary, browser-version-independent defense.
2. **`Origin`/`Referer` host == app host** (mandatory — the request is rejected if `Origin` is
   present and mismatched; modern browsers always send `Origin` on POST `fetch`).
3. **`Sec-Fetch-Site: same-origin`** — enforced **when present**; treated as advisory only where
   absent. `Sec-Fetch-*` is unavailable on Safari < 16.4 and some older Chromium/Firefox; in that
   window the custom-header (1) + `Origin` (2) checks carry the defense, so the absence of
   `Sec-Fetch-Site` does **not** open the endpoint. The exact unsupported matrix is documented in
   §8.5.

Body is `application/x-www-form-urlencoded` or JSON; the gateway forwards to Hydra `/oauth2/token`:

- **Code exchange**: `grant_type=authorization_code, code, code_verifier, redirect_uri,
  client_id=<app client>`.
- **Refresh**: `grant_type=refresh_token, refresh_token, client_id=<app client>` — accepted only
  when `useRefreshTokens` issued the browser its own family (see below).

The gateway injects `client_id` from the resolved app (the browser never sends it; the browser
sends only `code`/`code_verifier`/`refresh_token`). The request body carries a `mode` the SDK sets:
`mode=server_anchor` (default — server keeps the refresh family, browser gets an access token only)
or `mode=browser_refresh` (`useRefreshTokens` — browser keeps its own family).

**Code-exchange semantics — who holds the rotating refresh token (single-holder invariant):**

- `mode=server_anchor` (default): the gateway does the code→token exchange, **keeps the refresh
  token server-side** (encrypted under the anchor row), and returns to the browser **only** the
  access token + id token (no `refresh_token` in the JSON). Reload-recovery + ongoing refresh both
  go through `/__zs/auth/session?mint=1`, which rotates the server family under a DB row lock.
- `mode=browser_refresh` (`useRefreshTokens`): the SDK runs a **second, independent** authorize →
  popup-callback → code exchange specifically for the browser family. That second code yields the
  browser's own refresh token (returned in the JSON, held in the Web Worker). The server anchor,
  established by the first exchange, holds a **different** family from a **different** code. Two
  codes ⇒ two non-overlapping families ⇒ no shared rotation, no reuse-detection race.

**Server-side id_token validation (the SDK does NOT trust a raw id_token).** ⚠️ **Round-2 fix
(MAJOR).** The browser cannot reach Hydra's JWKS (no CORS), so it cannot verify the id_token's
signature itself. Round 1 had the SDK *decode* the id_token to populate the user profile — trusting
an unverified JWT, which an XSS or a compromised intermediary that influences the same-origin token
response could forge. Auth0-spa-js avoids this by validating the id_token (nonce, iss, aud, exp,
signature) against the OIDC client's JWKS. **We do the equivalent on the gateway, which already
holds Hydra JWKS** (`state.oidc_rp.jwks`): <!-- Added in round 2: addressing MAJOR — gateway validates id_token (sig/nonce/iss/aud/exp) and returns a trusted user projection; SDK never decodes a raw id_token -->

- On every `/__zs/auth/token` code exchange the gateway, after receiving Hydra's response,
  **fully validates the id_token**: EdDSA/RS256 signature via `state.oidc_rp.jwks`, `iss ==
  state.oidc_rp.issuer`, `aud == route.oauth_client_id`, `exp`/`iat` within skew (these four are the
  **load-bearing** binding), and a defense-in-depth `nonce` echo check (round-3: *not* a guarantee
  against an untrusted client — see "Nonce round-trip" below). A signature/iss/aud/exp failure
  returns `400 invalid_token` and no cookie is set.
- The gateway returns a **server-validated `user` projection** in the token response (the same
  shape as `/session`), built from the *verified* id_token claims. **The SDK consumes `user` from
  the response body and never decodes the id_token itself.** The id_token is still returned (for
  parity / opaque pass-through) but is not trusted client-side.

**⚠️ Round-3 fix (MAJOR) — the browser receives a per-app WRAPPER access token whose `sub` is the
`pws_`, NOT the raw Hydra access JWT (which carries the GLOBAL UUID in `sub`).** Round 2 kept Hydra's
`sub` = the global `usr_` UUID end-to-end and only projected `pws_` at the `ZeroShip-User` header.
But in Bearer mode the **browser holds the access token** (`Session.access_token`) and the trust
model (§8.5) puts arbitrary creator JS *inside* the token boundary — so any app's JS could
`base64`-decode its own access token's payload and read the global `usr_` UUID `sub`, correlating the
user across apps. That makes G4 ("each app sees a per-app pairwise sub, never the global user id")
**false on the Bearer path** and undermines mitigation #1 ("a token stolen from app A reveals only
app A's `pws_…`"). The fix reuses the **wrapper-token machinery that already exists** for the DPoP
path (`crates/gateway/src/wrapper_token.rs` `Issuer::issue` → `WrapperClaims{ sub, scope, client_id,
email, … }`, signed by the gateway's own ed25519 key and verified by `state.wrapper_verifier`): <!-- Added in round 3: addressing MAJOR — browser gets a wrapper access token whose sub is pws_ and email is the alias; raw Hydra access JWT (global UUID) never leaves the gateway -->

- On the `/token` code exchange, after validating Hydra's response, the gateway **mints a per-app
  wrapper access token** with `sub = derive_pairwise(hydra.sub, route.sector_identifier)` (the
  `pws_`), `email = relay_alias`, `client_id = route.oauth_client_id`, `scope`, and a 10-min `exp`.
  **This wrapper is the `access_token` returned to the browser.** The raw Hydra access JWT (global
  UUID) is kept server-side under the anchor (for `?mint=1` re-mint) and **never leaves the gateway**.
- The Bearer arm (§1.3) **already** verifies wrapper tokens via `state.wrapper_verifier` (the DPoP
  precedent), so it accepts this wrapper and reads `pws_` straight from its `sub` — no pairwise
  re-derivation needed on the wrapper path. (A wrapper minted **without** a DPoP key simply omits
  `cnf.jkt`; `useDpop` adds the `cnf.jkt` binding exactly as the DPoP exchange does today.) This
  reuses `state.wrapper_issuer` / `state.wrapper_verifier` (`crates/gateway/src/lib.rs:136/147`),
  which the gateway already constructs from its ed25519 signing key — the **same** key requirement
  the shipped `/__zs/auth/dpop-exchange` path has, so no new key material. If the signing key is
  absent (a misconfiguration), `/token` returns `503` exactly as `dpop-exchange` does today, rather
  than silently handing back the raw Hydra token.
- ⇒ **The token the browser holds contains the `pws_`, never the global UUID.** App JS decoding its
  own access token sees only its per-app pairwise sub and relay alias — cross-app correlation is
  impossible, and G4 holds on the Bearer path. The `/session?mint=1` response likewise returns a
  freshly-minted **wrapper** access token (sub = `pws_`), not the raw Hydra token.
- **Test (round-3):** assert the `access_token` the browser receives from `/token` and
  `/session?mint=1` does **not** contain the global `usr_` UUID anywhere in its payload, and that its
  `sub` equals the app's `pws_`.

**Nonce round-trip — honest scope (round-3, MAJOR).** The SDK echoes `nonce` in the `/token` body
and the gateway asserts the id_token's `nonce` claim equals it. ⚠️ **This is NOT the OIDC nonce
guarantee in this trust model, and the spec no longer claims it is.** The gateway is **stateless**
across `/authorize`→`/token` (no server-side record of the expected nonce — the Supabase-style
statelessness this design deliberately chose), so the value it checks against is supplied by the
client. Since the trust model (§8.5) is that arbitrary/XSS creator JS is *in* the boundary and
controls the `/token` request body, that JS could echo any nonce that matches a forged id_token —
the "stored" side of the OIDC nonce contract is the very client we distrust. **The real binding is
signature + `iss` + `client_id`(`aud` for the per-app client) + `exp`** — i.e. "this is a
validly-signed Hydra id_token for this app's client." The nonce check is kept as **defense-in-depth
against accidental cross-flow id_token mixups**, not as a defense against an XSS-controlled client.
We do **not** add a server-side per-flow nonce stash, because that would reintroduce the stateful
`/authorize`→`/token` flow record this design removed to stay stateless; the signature+client_id
binding is the load-bearing guarantee and is sufficient given Hydra is the only id_token signer.
<!-- Added in round 3: addressing MAJOR — stop presenting a client-echoed nonce as an OIDC guarantee under an untrusted-client model; bind on signature+client_id+iss+exp; nonce is defense-in-depth only -->

```
200 OK  (mode=server_anchor)
Cache-Control: no-store
Content-Type: application/json
Set-Cookie: __Host-zs_app_session=<anchor>; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=2592000

{ "access_token":"<gateway WRAPPER jwt — sub=pws_, email=alias, client_id=app_…, 10-min exp>",
  "id_token":"<jwt — opaque pass-through, NOT trusted by the SDK>", "token_type":"Bearer",
  "expires_in":600, "scope":"openid profile read:billing",
  "user": { "id":"pws_…", "email":"…@{relay_domain}", "name":"…", "avatar":null,
            "email_verified":true, "scopes":["openid","profile","read:billing"] } }
  // access_token is the per-app WRAPPER (sub=pws_), NOT the raw Hydra JWT (global UUID) — §1.2 round-3.
  // gateway-validated `user` projection; NO refresh_token to the browser; SDK trusts `user`, not id_token
```

The **anchor cookie** value is a server-side row in **`auth.app_session_anchors`** (the dedicated
anchor store, §8.1 — *not* `auth.gateway_sessions`), holding the app's pairwise sub, the encrypted
server-held refresh family, and the cached minted access token so `/__zs/auth/session` re-mints
without any browser-held refresh token. **The anchor is bound to the access-token family** (it stores
the family's Hydra session id / `jti` lineage), so a stolen browser refresh token alone (in
`browser_refresh` mode) cannot resurrect or impersonate the anchor.

> **Decision (server-held refresh under the anchor — O3, resolved + round-3 hardened).** Exactly
> **one holder rotates each refresh family**, AND the single server family is rotated under an
> **anchor-id advisory lock held across the Hydra refresh**, so concurrent `?mint=1` calls from
> multiple tabs/reloads collapse to one rotation. In the default `server_anchor` mode the *only*
> refresh family is server-side and the browser never holds a refresh token at all — eliminating
> *browser-vs-server* dual-rotation by construction; the round-3 lock + cached-access-token
> eliminates the *server-vs-server* (tab-vs-tab) rotation race that round 2 still had. The discarded
> alternatives were (a) keeping a *copy* of one rotating token in both the browser and the server
> (guaranteed reuse-detection race on the first reload-then-refresh — rejected), (b) "a second
> refresh token from a parallel grant" (impossible: a code is single-use — rejected as
> ill-specified), and (c) letting each `?mint=1` rotate freely and relying on Hydra's 30s/3× grace to
> absorb concurrency (round-2 posture — rejected in round 3 because ≥3 concurrent minters exhaust the
> grace and trip family revocation). `browser_refresh` mode opts into Supabase-style browser-held
> refresh by spending a **separate** code for a **separate** family, never a copy. <!-- Added in round 1: addressing MAJOR — single-holder, no copy, no parallel-grant hand-waving; addressing token endpoint XSS — same-origin check + family-bound anchor; Updated in round 3: anchor-id advisory lock + cached access token kills the tab-vs-tab same-family rotation race -->

#### `GET /__zs/auth/session`

Reads `__Host-zs_app_session`, validates against `auth.app_session_anchors` (the dedicated anchor
store, §8.1 — **not** `auth.gateway_sessions`), returns the server-validated user projection and
(with `?mint=1`) a fresh access token minted from the server-held refresh token. Gated by the
breadcrumb on the client side (the SDK skips this call when the breadcrumb is absent).

⚠️ **Round-3 fix (BLOCKER) — `?mint=1` is atomic and idempotent under concurrency: N concurrent
minters ⇒ exactly ONE Hydra refresh call.** Round 2 had every tab and every reload drive
`/session?mint=1`, and each mint did `grant_type=refresh_token` against Hydra — which **rotates** the
single server-held refresh family. Two tabs (or a tab + a background reload) minting concurrently
were two concurrent rotators of the **same** family: request A reads refresh `R`, request B reads
`R` before A's rotation commits, both POST `R` to Hydra, and Hydra's Automatic Reuse Detection
revokes the whole family on the second use of `R`. The dev config's `rotation_grace_period: 30s` /
`rotation_grace_reuse_count: 3` (`ops/hydra-dev.yaml:49-51`) *softens* this (a rotated token can be
reused up to 3× within 30s and all tokens stay in one chain — per Ory's graceful-rotation design)
but does **not** eliminate it: three concurrent reloads/tabs exhaust the reuse count and the family
is revoked. `navigator.locks` cannot help — it does not serialize across the browser/server boundary,
and the lock key is per-origin-per-profile while the rotation happens **server-side** across
independent browser contexts. <!-- Added in round 3: addressing BLOCKER — hold the anchor row lock ACROSS the Hydra refresh; cache the minted access token so concurrent minters share one rotation -->

The fix makes the rotation **server-serialized and result-shared**, with an explicit locking
boundary:

```
mint(anchor):
  lock = pg_advisory_xact_lock( hash(anchor.id) )         // serializes ALL minters for this anchor
  begin:
    a := read anchor row (cached_access_token, cached_access_exp, refresh_token_enc)
    if a.cached_access_token is present AND a.cached_access_exp > now()+skew:
        return a.cached_access_token                       // share the in-window token, NO Hydra call
    R := decrypt(a.refresh_token_enc)
    resp := POST Hydra /oauth2/token { grant_type=refresh_token, refresh_token=R, client_id }
    if resp == invalid_grant:                              // family revoked/expired upstream
        delete anchor row; return 401 login_required
    store anchor row { refresh_token_enc = encrypt(resp.refresh_token),
                       cached_access_token = resp.access_token,
                       cached_access_exp = now()+resp.expires_in,
                       refresh_family_id = resp.family_lineage }
  commit (releases the xact lock)
  return resp.access_token
```

- The lock is a **PostgreSQL transaction-scoped advisory lock keyed on the anchor id**
  (`pg_advisory_xact_lock`), held **across** the Hydra refresh round-trip and the row write — not
  just the write. A second concurrent `?mint=1` blocks on the lock; by the time it acquires, the
  first has already stored a fresh `cached_access_token`, so the second takes the cache branch and
  returns that token **without a second Hydra call**. This is cross-node correct because the advisory
  lock and the anchor row both live in shared Postgres (the same store backing `dpop_jti`, §8.5).
- **Idempotent under N-way concurrency:** the first minter to acquire the lock does the single Hydra
  rotation; everyone else serves the cached in-window token. The `cached_access_exp` short TTL (=
  the 10-min browser access TTL, §8.5) bounds how long the cache is served before the next rotation.
- **`browser_refresh` mode is unaffected** — that family is browser-held and rotated by the SDK under
  `navigator.locks` (§2/§3); the server anchor family is a *different* family from a *different*
  code, so the two never collide (the single-holder invariant, O3).
- **Regression test (round 3):** fire **N parallel `?mint=1`** against one anchor; assert exactly
  **one** Hydra `/oauth2/token` call is made, all N responses carry a valid access token, and the
  refresh family is **not** revoked (no reuse-detection trip). A second test asserts a mint after the
  cached token expires triggers exactly one new rotation.

⚠️ **Round-2 hardening (MAJOR) — `?mint=1` is a credential-minting surface and must not be
triggerable by a top-level navigation.** A `SameSite=Lax` anchor cookie **is** sent on a top-level
GET navigation, so without a guard a malicious site could navigate the victim to
`…/__zs/auth/session?mint=1` and cause a token mint (even though the response is unreadable
cross-origin, minting in a loop is abuse, and any future caching/logging of the minted token would
leak). The mint path therefore **requires a non-simple request header `X-ZS-Auth: 1`** that the SDK
always sets via `fetch`. A plain top-level navigation cannot set a custom header, so it cannot reach
the mint branch. Additionally the anchor is moved to **`SameSite=Strict`** (§8.3) — it is never
legitimately needed on a cross-site request — and the handler enforces `Sec-Fetch-Site:
same-origin` when present. The non-`mint` read (`GET /__zs/auth/session` without `?mint=1`) returns
only the cached user projection (no fresh token) and is harmless. <!-- Added in round 2: addressing MAJOR — require custom X-ZS-Auth header on mint + SameSite=Strict anchor so a top-level navigation can't mint -->

```
GET /__zs/auth/session?mint=1
Headers (required): X-ZS-Auth: 1        (custom header — absent ⇒ 400 invalid_request, no mint)
→ 200 { "user": {…}, "scopes":[…], "access_token":"<gateway WRAPPER jwt — sub=pws_, 10-min exp>",
        "expires_at":1717000000 }     (wrapper, NOT the raw Hydra JWT; minted under the anchor lock, §1.2)
→ 401 { "error":"login_required" }   (anchor missing/expired, or Hydra invalid_grant on the family)
→ 400 { "error":"invalid_request" }  (mint requested without X-ZS-Auth)
```

#### `POST /__zs/auth/signout`

```
POST /__zs/auth/signout
Body: { "scope": "local" | "global" }
```

- `local`: revoke the server-held refresh family (Hydra `/oauth2/revoke`) and, in
  `browser_refresh` mode, the browser family too; delete the `auth.app_session_anchors` row; clear
  `__Host-zs_app_session` + breadcrumb; and upsert an `auth.token_revocations` `(client_id, sub)`
  family marker so an already-minted wrapper/access token is rejected cross-node (§8.5). The user
  stays signed in on other devices/tabs of **this** app (their anchors are independent) and on
  **other** apps (different per-app client).
- `global`: revoke **all anchor rows for this `(app, user)`** (this app, every device) — done by
  deleting `auth.app_session_anchors` rows for `(app_id, global_user_id)`, revoking each refresh
  family (Hydra `/oauth2/revoke`), **and upserting an `auth.token_revocations` family marker
  `(client_id, sub) → revoked_after = now()`** so any already-minted access/wrapper token is rejected
  cross-node (§8.5) — without touching the IdP session or other apps. This is the "this app,
  everywhere" semantics resolved in O6. We do **not** call Hydra RP-logout for `global`, because the
  existing BCL handler would fan out across *all* the subject's apps. <!-- Added in round 1: addressing MINOR — per-app global signout, no all-apps nuke; Updated in round 3: anchor rows live in app_session_anchors; write a token_revocations family marker -->

**Per-app back-channel logout.** Each per-app client registers its own
`backchannel_logout_uri = {host}/oidc/backchannel-logout` (§1.1). The existing
`crates/gateway/src/backchannel_logout.rs` handler is updated: instead of `aud ==
state.oidc_rp.client_id` (the single `"gateway"` value) it accepts a `logout_token` whose `aud`
matches **any registered per-app client** for the request host, resolves the `app_id` from that
`client_id`, and revokes only **that app's** sessions for the subject —
`sessions::revoke_app_sessions_for_user(app_id, sub)` (new, replacing the all-apps
`revoke_all_for_user`). A true platform-wide "log out of every app" remains a separate, explicit
control-plane action, not a side effect of one app's BCL. <!-- Added in round 1: addressing MINOR — per-app BCL disambiguation -->

Returns `204` with `Set-Cookie` clears. The SDK's server `signOut` and client `signOut` both
target this; **this fixes the live "no handler for /__zs/auth/signout" bug.**

### 1.3 Gateway Bearer arm (`router/auth.rs`)

Add a Bearer arm to `resolve_auth`, ordered **after** the DPoP arm and **before** the cookie arm.

**Discriminating end-user sessions from the reserved API-key path.** The `Authorization: Bearer`
scheme is earmarked for a future app-API-key flow (Fact 2). ⚠️ **Round-3 — the browser holds a
gateway-issued WRAPPER access token (sub=`pws_`, §1.2), so the Bearer arm recognizes two
issuer-discriminated user-session shapes plus the reserved path:** <!-- Added in round 3: addressing MAJOR — Bearer arm primarily verifies the gateway wrapper (sub=pws_, no global UUID); raw Hydra JWT path retained for non-browser clients; reserved API-key path unchanged -->

- **Gateway wrapper** (`iss == gateway issuer`, verified via `state.wrapper_verifier`): the **default
  browser-session token**. Its `sub` is already the `pws_` and its `email` the alias, so **no pairwise
  re-derivation is needed** — the header is built straight from the wrapper claims (exactly the DPoP
  path's precedent). `cnf.jkt` present ⇒ DPoP-bound (`useDpop`); absent ⇒ plain Bearer.
- **Raw Hydra access JWT** (`iss == state.oidc_rp.issuer`, verified via `state.oidc_rp.jwks`):
  retained for **non-browser** OAuth clients that hold a Hydra access token directly (e.g. the CLI, or
  server-to-server). Its `sub` is the global UUID, so this path **does** derive `pws_ =
  derive_pairwise(sub, route.sector_identifier)` before building the header. The browser never uses
  this shape (it gets the wrapper), so a global-UUID `sub` never reaches app JS.
- **Anything else** (opaque, non-recognized `iss`, the future `zsk_…` API-key shape) →
  `BearerOutcome::NotUserSession` → reserved `api_key` arm → `401 unsupported_token`.

A Bearer token is therefore never ambiguously routed.

```
0. DPoP arm (existing).
0b. if Authorization: DPoP present but failed → Unauthenticated (existing).
1. NEW: Bearer arm — if Authorization: Bearer <token>:
     resolve_bearer_user_header(req, state, request_id) -> BearerOutcome
       a. token = strip "Bearer ".
       a'. if route.oauth_client_id is None → Unauthenticated (401 client_not_provisioned;
            never bind to an empty/falsy client_id).
       b. peek `iss` (unverified):
            iss == gateway issuer        → WRAPPER path (step c-wrap)
            iss == state.oidc_rp.issuer  → raw-Hydra path (step c-hydra)
            else                         → BearerOutcome::NotUserSession (reserved API-key → 401)
       c-wrap. verify wrapper via state.wrapper_verifier (ed25519, exp, aud==host) and
            `claims.client_id == route.oauth_client_id`. sub is ALREADY pws_, email IS the alias.
            (DPoP-bound if cnf.jkt present — verified like the DPoP arm.) No pairwise derivation.
       c-hydra. verify as Hydra access JWT via state.oidc_rp.jwks (JwksCache):
            signature (EdDSA/RS256), iss == issuer, exp, nbf/iat,
            and `Some(client_id CLAIM) == route.oauth_client_id`        ← per-app binding
            (RFC 9068 §3 mandates `client_id` on access tokens; Hydra emits it. We bind to
             `client_id`, NOT `aud` — `aud` is the resource-server audience, not the client.)
            Then derive pws_ = derive_pairwise(claims.sub, route.sector_identifier) (global→pws_).
       c'. revocation check (§8.5): reject if claims.jti ∈ the token-revocation denylist OR the
            token's (client_id, sub) family was revoked after the token's iat (family-marker — the
            PRIMARY mechanism, cross-node via PG; §8.5).
       d. enforce required scopes for the matched rule/policy (Subsystem 3); 403 on miss.
       e. resolve (id=pws_, email=alias):
          - WRAPPER path: pws_ and alias come straight from the verified wrapper claims (no derivation).
          - raw-Hydra path: pws_ = derive_pairwise(claims.sub, route.sector_identifier) (§6.2; the
            JWT is never rewritten), alias = relay-alias lookup (read-through cache, §7.1).
          then build OwnedWorkerUser{ id: pws_, email: alias, name, email_verified, scopes }
          → encode_user_header(&WorkerUser, &state.config.worker_key, *request_id).
     if Ok(header) → short-circuit Allowed { user_header: Some(header) } (like DPoP).
     if NotUserSession → Unauthenticated (401 unsupported_token; reserved path).
     if user-session token present but verify failed → see policy-gating note (Anon routes serve
       anonymously; User/Admin routes 401) — round-3.
2. cookie arm (existing).
```

⚠️ **Round-3 fix (MINOR) — a present-but-EXPIRED/invalid user-session Bearer no longer hard-fails an
`Anon` (public) route; it falls through to anonymous.** Round 2 made any present-but-invalid Bearer
yield `Unauthenticated` even on `Anon` routes (copying the DPoP precedent). But the SDK auto-attaches
`Authorization: Bearer` to **every** request, and the browser access token is only **10 min** (§8.5),
so "expired-but-present Bearer" is the **common** case (any tab left open >10 min). Hard-failing it on
public pages would 401 normal users on the app's own public routes until the SDK refreshes —
breaking public pages for logged-in-but-idle users. The DPoP precedent is for an **explicit opt-in**
scheme; the auto-attached Bearer is different. So the round-3 rule, by route policy: <!-- Added in round 3: addressing MINOR — expired/invalid auto-attached Bearer must not 401 public Anon pages; fall through to anonymous on Anon, still 401 on User/Admin -->

- **On `Anon` (public) routes:** an invalid/expired/missing-revoked user-session Bearer is treated as
  **no identity** — the request falls through and the public page is served **anonymously** (the SDK's
  next refresh re-establishes the session for the authed parts). A `NotUserSession` Bearer (reserved
  API-key shape) still yields `401 unsupported_token` even on `Anon` (it is asserting a *different*
  scheme, not an expired user session).
- **On `User`/`Admin` routes:** an invalid/expired Bearer yields `401` (identity required, none
  valid), exactly as before; a valid Bearer missing a required scope yields `403`.
- A **valid** Bearer is treated as fully authenticated regardless of policy (same as DPoP).
- The SDK additionally refreshes a within-skew token **before** firing app requests (`getAccessToken`
  dedup, §3), so the expired-token path is the exception, not the norm.

The `expired-Bearer-on-Anon-route serves the public page`, `invalid-Bearer-on-User-route 401`, and
`oauth_client_id == None` cases are added to the regression matrix (§8.6).

**Per-app binding is the critical safety property.** An access token minted for `app_A` must be
rejected at `app_B`'s host. The Bearer arm resolves the expected `oauth_client_id` from the request
`Host` (via the route cache, §1.5) and rejects tokens whose **`client_id` claim** doesn't match.
⚠️ **We bind to `client_id`, not `aud`** — the shared `gateway` client today sets `aud =
["http://api.zeroship.localhost"]` (the resource server), so binding to `aud` would be wrong. The
exact claim Hydra emits for a per-app *public* client is **confirmed against live Hydra in Slice
1c** before the binding is finalized; if Hydra unexpectedly omits `client_id`, the fallback is to
set each per-app client's `audience` to its own `client_id` and bind to `aud` — decided by the
spike, not assumed. <!-- Added in round 1: addressing MAJOR — bind to client_id claim not aud, verify against live Hydra -->

**Replay window + revocation (round-2 hardening, MAJOR).** Plain Bearer is not sender-constrained,
and because the gateway verifies access JWTs **locally** via JWKS (`strategies.access_token=jwt`),
a revoked-but-unexpired access token still validates — Hydra introspection is bypassed. So
"`signOut` revokes the family" applies to the **refresh** family, not a leaked **access** token,
which stays valid until `exp` regardless of family revocation. Round 1's "1h TTL" is too long a
replay window for a bearer token living in app-readable memory in an arbitrary-AI-code environment.
Round 2 changes three things: <!-- Added in round 2: addressing MAJOR — shorten browser access-token TTL, add a jti denylist for revoked families, quantify the local-verify revocation window -->

1. **The browser-held access token is a 10-minute gateway WRAPPER** (round-3): the wrapper's own
   `exp = iat + 10min` is what the browser holds, independent of Hydra's 1h global access TTL (the raw
   Hydra token stays server-side under the anchor and re-mints via `/session?mint=1`). This bounds the
   post-exfiltration replay window to ≤10 min and means we do **not** need to alter Hydra's per-client
   `access_token_ttl` for the browser path — the wrapper TTL is set at mint time by the gateway.
2. **A CROSS-NODE revocation denylist whose PRIMARY mechanism is a family marker, not per-jti.** ⚠️
   **Round-3 fix (MAJOR).** Round 2 said "reuse `dpop_jti_cache`/`logout_jti_cache`" and claimed
   "near-immediate" revocation — but that is unsound in the multi-node deployment (CHWBL routing,
   `--scale worker`, multiple gate nodes). The two caches are **not** equivalent: `dpop_jti_cache` is
   a **`TieredJtiCache` with a PG tier** (`auth.dpop_jti` table, `crates/core/src/dpop.rs:980` +
   `main.rs:479-483`) — genuinely **cross-node** via shared Postgres — while `logout_jti_cache` is an
   **in-memory `Mutex<HashMap>`** (`crates/core/src/logout_token.rs:99`), per-process only. A token
   replayed against a *different* gate node than the one that processed signout would NOT be in that
   node's in-memory set, so an in-memory denylist degrades to "same as the 10-min TTL" cross-node —
   no revocation benefit at all. Worse, **signout cannot in general enumerate the live access-token
   `jti`s** to deny: at signout the server holds the refresh family, not the set of wrapper/access
   `jti`s minted from it across nodes. So per-jti cannot be the primary mechanism. Round 3 makes the
   **`(client_id, sub, revoked_after)` family marker the PRIMARY mechanism, backed by a shared PG
   table**: <!-- Added in round 3: addressing MAJOR — back the denylist with a shared PG table (cross-node); make the (client_id, sub, revoked-after) family marker the primary mechanism since signout can't enumerate jtis -->
   - **New shared table `auth.token_revocations(client_id text, sub text, revoked_after timestamptz,
     PRIMARY KEY (client_id, sub))`** (Liquibase changeset, §8.1). On `signOut`/family-revoke the
     gateway **upserts** `(client_id, sub) → revoked_after = now()`. This is one row per family, so
     no jti enumeration is needed.
   - The Bearer arm, after verifying the token (wrapper or raw-Hydra), checks: **is there a
     `token_revocations` row for `(token.client_id, token.sub)` with `revoked_after > token.iat`?**
     If so → `Unauthenticated`. Any token issued *before* the revocation is rejected on **every** node
     (the table is shared PG, read-through-cached locally with a short TTL exactly like the
     `dpop_jti` tier). New tokens minted *after* re-login carry a later `iat` and pass.
   - **Per-jti denial is the special case** (a single stolen token revoked without signing out the
     family): the same `auth.token_revocations`-style PG-backed set holds individual `jti`s when the
     caller *does* know one (e.g. an admin revoke of a specific token). It is a `TieredJtiCache`
     (PG-backed, cross-node) — NOT the in-memory `logout_jti_cache`.
   - **Revised honesty:** revocation is now **cross-node immediate** for the family case (a single PG
     upsert visible to every node on its next read-through), bounded by the read-through cache TTL
     (seconds), and the residual replay window for a token *not yet covered by a marker* is the 10-min
     access TTL. The denylist is no longer "same as TTL" in multi-node — the family marker makes it a
     real cross-node revocation primitive.
3. **DPoP-binding is the documented default-recommendation for browser sessions** given the stated
   trust model (arbitrary creator JS). It remains opt-in in Phase 1 (the wrapper path
   `/__zs/auth/dpop-exchange` already mints `cnf.jkt`-bound wrappers with a `jti` replay cache), but
   the SDK surfaces a one-flag `useDpop: true` and the docs recommend it for any app handling
   sensitive scopes. The exact quantified window (≤10 min plain-Bearer; per-request sender-constraint
   with DPoP) is stated in §8.5.

New code: `resolve_bearer_user_header` + `build_worker_user_from_wrapper_claims` /
`build_worker_user_from_jwt_claims` in `router/auth.rs`; the wrapper path reuses
`state.wrapper_verifier.verify(token, host)` (the existing DPoP-exchange verifier); the raw-Hydra
path uses a `verify_access_token(token, expected_client_id) -> Result<AccessClaims>` helper on
`OidcRp` (reuses `self.jwks`, checks the `client_id` claim); both consult the cross-node
`token_revocations` family marker. Regression tests: accept a valid **wrapper** (sub=`pws_`) and a
valid raw-Hydra token for matching `client_id`; reject wrong `client_id`; reject expired; reject bad
signature; **reject a non-recognized-`iss` token as `NotUserSession` (reserved API-key path)**;
**reject a token whose `(client_id, sub)` family was revoked after its `iat`** (cross-node); 403 on
missing scope; an expired Bearer on an `Anon` route serves the public page (round-3).

### 1.4 `env.auth` plugin (runtime)

New `AuthPlugin` (namespace `"auth"`) in `crates/runtime/src/auth.rs`, wiring the existing
orphaned callbacks:

```rust
pub struct AuthPlugin;
impl NativePlugin for AuthPlugin {
    fn namespace(&self) -> &str { "auth" }
    fn register(&self, r: &mut NativeRegistrar) {
        r.function("getUser", get_user_callback);
        r.function("requireUser", require_user_callback);
    }
}
```

⚠️ **Round-3 (BLOCKER) — registered at BOTH plugin-construction sites, with the WORKER as the
primary target.** Because the worker registers only `DbPlugin` (`crates/worker/src/cache.rs:40-46`),
`AuthPlugin` is pushed in **two** places, in the same patch: <!-- Added in round 3: addressing BLOCKER — pin AuthPlugin to create_plugins() (worker) AND the CLI vector; e2e item 5 runs against the worker path -->

1. **Worker** — `create_plugins()` in `crates/worker/src/cache.rs`: push `Arc::new(AuthPlugin)`
   unconditionally (it is stateless), so the vec becomes `[DbPlugin?, AuthPlugin]`. This is the line
   that makes `env.auth.getUser()` work for **every production end-user app**.
2. **CLI / `zeroship serve`** — the plugin vector in `crates/cli/src/main.rs` (alongside db/storage/kv):
   push `Arc::new(AuthPlugin)` so the dev path matches.

`AuthPlugin` takes no constructor args (the callbacks read `RuntimeState`), so neither site needs
auth config. After this, `env.auth.getUser()` resolves and the server SDK's `getUser()` returns the
real user instead of `null`. **Regression test runs against the WORKER path** (real worker
`create_plugins()` → real dispatcher → real `ZeroShip-User` header): assert `env.auth.getUser()`
returns the per-request user and `requireUser()` throws when anonymous. A second assertion confirms
the plugin is present in the worker's plugin vector specifically (not just under `zeroship serve`).
This is exercised by the **faithful e2e** (item 5, §8.6 — driven through the worker, not a serve
shim).

**`WorkerUser.scopes` is a permanent kernel-contract extension — flagged explicitly.** Adding
`scopes: Vec<String>` to the user shape (Subsystem 3) is a deliberate, *forever* addition to the
stable `WorkerUser` wire contract (`oidc_rp.rs:546` has no `scopes` today), per the "native
primitives are forever" invariant. Because it is a wire-format change, **every** producer and
consumer changes in the **same patch**, with no shim: <!-- Added in round 1: addressing MINOR — WorkerUser scopes is a permanent kernel contract; enumerate consumers -->
1. `oidc_rp::WorkerUser<'a>` (+ `scopes: Vec<&str>` / owned variant) and `encode_user_header`,
2. gateway `build_worker_user_from_{jwt_claims,wrapper,introspection}` (populate `scopes`),
3. gateway cookie-session path (read granted scopes from `auth.gateway_sessions.granted_scopes`),
4. worker `User` deserialization (`crates/worker/`),
5. runtime `auth.rs` `get_user_callback`/`require_user_callback` (expose `.scopes`),
6. RPC `ctx.user` (`rpc/ctx_holder.rs`) — `ctx.user.scopes` now present,
7. SDK `User` type (`sdks/auth/src/types.ts`).

> **Why `scopes` belongs in the kernel, not derived in JS.** The cookie-session path has **no**
> access token in the browser to decode — only the server knows the granted scopes
> (`auth.gateway_sessions.granted_scopes`). So `scopes` cannot be uniformly derived in JS from a
> token the SDK holds (the Bearer path has one; the cookie path does not). Putting `scopes` on
> `WorkerUser` is the only place both paths converge, which is why it is a kernel field and the
> worker-side `env.auth.getUser().scopes` is authoritative.

### 1.5 Route-sync wire-format extension (`RouteEntry` → gateway)

`RouteEntry` (`crates/core/src/types.rs:59`) and `CompiledRoute` (`crates/gateway/src/sync.rs`)
carry **no** OAuth identity today; the gateway holds a single hardcoded `client_id = "gateway"`.
Resolving a per-app `client_id`/sector from the request `Host` therefore requires a **deliberate
wire-format break** (Key Invariant: "Wire formats are explicit contracts"), landed in one patch
across every producer/consumer/fixture: <!-- Added in round 1: addressing BLOCKER — promote O1 to a designed wire-format change -->

- **`RouteEntry` gains** `oauth_client_id: Option<String>` and `sector_identifier: Option<String>`
  (the per-app apex origin used for pairwise + relay scoping). ⚠️ **Round-2 fix (MAJOR) — these are
  `Option`, never a defaulted empty `String`.** An empty-string `oauth_client_id` is a footgun: the
  Bearer arm binds on `client_id claim == route.oauth_client_id`, and a token whose `client_id`
  claim is empty (or a malformed token) could match `""`. So the field is `Option<String>` = `None`
  until the control plane provisions the app's client (`#[serde(default)]` ⇒ `None`, keeping the
  struct loadable for un-provisioned apps), and **all five browser-auth endpoints and the Bearer arm
  hard-fail when it is `None`** — `/authorize`, `/popup-callback`, `/token`, `/session`, `/signout`
  return `503 client_not_provisioned`; the Bearer arm returns `401` (never binding to a falsy
  value). The binding comparison is `Some(claim) == route.oauth_client_id`, which is unreachable
  while `None`. A round-trip fixture test asserts `None` round-trips and that the Bearer arm rejects
  every token (valid signature or not) when `oauth_client_id` is `None`. <!-- Added in round 2: addressing MAJOR — oauth_client_id is Option<String>, hard-fail on None, never bind to empty -->
  `sector_identifier` is likewise `Option`; the pairwise projection (§6.2) hard-fails closed (no
  `pws_` derivation) when it is `None`, so an un-provisioned app never emits a header.
- **Control populates them** from `control.app_oauth_clients` (§8) on the route-sync push (the
  existing control→gateway 5s HTTP pull) — the same path that already ships `RouteEntry`.
- **`CompiledRoute` surfaces them** so `lookup_by_name(host) -> (Uuid, Arc<CompiledRoute>)` yields
  the `oauth_client_id` without a second lookup.
- **Threaded through**: `build_browser_authorize_url(client_id, …)` (the `/authorize` 302), the
  `/__zs/auth/token` proxy (inject `client_id`), and the Bearer arm's `client_id`-claim binding
  (§1.3) all read `route.entry.oauth_client_id`.
- **Fixtures/tests** updated in the same patch: the gateway route-sync fixtures, `RouteEntry`
  round-trip tests, and the dispatch tests that assert `client_id`.

This is the concrete mechanism behind the former "O1" open question — it is **designed here**, not
deferred. Custom domains: each app host (apex + every custom domain) maps 1:1 to the same per-app
`oauth_client_id`; the `name_index` already keys by host, so all of an app's hosts resolve to the
same `app_id` → same `oauth_client_id`.

⚠️ **Round-3 — gateway-complexity acknowledgement against the "dumb gateway" invariant, and the
provisioning→route-sync ordering guarantee.** Two things the round-2 §1.5 left implicit: <!-- Added in round 3: addressing MAJOR — acknowledge gateway identity-logic growth; pin the provision→route-sync ordering so there is no cold-start 503; state oauth_client_id staleness/reassignment behavior -->

1. **The gateway grows real identity logic, and that is a deliberate, bounded exception to "the
   gateway is dumb."** The browser-auth endpoints make the gateway build authorize URLs, proxy/validate
   token exchanges, **fully validate id_tokens** (sig/iss/aud/exp), rotate the server refresh family
   under a lock, derive pairwise `pws_` via HMAC, look up relay aliases, and run a jti denylist. We
   justify keeping this in the gateway rather than a sidecar because: (a) every piece reuses
   primitives the gateway **already** owns for the shipped DPoP/BCL paths — JWKS verification
   (`state.oidc_rp.jwks`), `encode_user_header`, the `dpop_jti` PG cache, the cookie-session store —
   so it is the *same class* of work, not a new competence; (b) it is all **stateless OAuth plumbing
   + a header projection**, no app/business logic (the invariant's actual line); and (c) a separate
   auth-sidecar would add a network hop on the hot per-request Bearer path and a second JWKS cache to
   keep coherent. If this surface grows further (e.g. token introspection policies, multi-IdP), the
   documented escalation is to extract a thin **auth-sidecar** that owns id_token validation +
   pairwise derivation; until then it stays in the gateway, explicitly noted as the one place the
   "dumb" invariant is stretched.
2. **Provisioning ordering — control creates the Hydra client AND ships the populated `RouteEntry`
   before the app is routable, so there is NO cold-start 503 window.** The app/deploy handler runs
   `ensure_app_client(app)` (Hydra `POST /admin/clients` + write `control.app_oauth_clients`)
   **before** the route becomes resolvable: the route-sync push that makes the host routable carries
   the **already-populated** `oauth_client_id`/`sector_identifier` in the same `RouteEntry`. So a
   host is never live with `oauth_client_id == None`. The `None` arm and its `503
   client_not_provisioned` remain as a **defensive** state (e.g. a hand-rolled route, a partial sync,
   or a reconcile error) — not a routine first-deploy window — and the SDK treats `503
   client_not_provisioned` as retryable with backoff. A control-plane invariant test asserts
   "RouteEntry for a deployed app always carries `Some(oauth_client_id)`."
3. **`oauth_client_id` staleness / reassignment.** The per-app `client_id` is **stable for the life
   of the app** (`app_<base62-app-id>`, derived from `app_id`); it is never rekeyed or reassigned
   while the app exists, so the Bearer arm's `client_id`-claim binding cannot validate against a
   stale value. The only mutation is redirect-URI/scope edits (a `PUT`, not a new `client_id`), which
   do not affect the binding. The staleness bound on the cached `oauth_client_id` is therefore the
   route-sync interval (≤5s) only for the **provisioning** transition (`None → Some`), which (per
   point 2) is ordered to never be observed on a live host. On app **delete**, the client and the
   route are removed together; a token for a deleted app fails because the route (and thus the
   expected `client_id`) is gone → `401`.

## 4. Subsystem 2 — `@zeroship/auth` SDK

### 4.1 Package layout

`sdks/auth/` restructured to mirror `sdks/rpc` (subpath exports, tsup, ESM-only).

```
sdks/auth/
  package.json          exports: "." (server) · "./client" · "./react" · "./types"
  tsup.config.ts        entry: src/server.ts, src/client.ts, src/react.tsx, src/types.ts
  tsconfig.json         lib: ["ES2022","DOM","DOM.Iterable","WebWorker"]
  src/
    types.ts            Session, User, AuthError, AuthChangeEvent, AuthClientOptions, Scope
    server.ts           env.auth-backed getUser/requireUser/isLoggedIn/signOut  (the "." export)
    client.ts           createAuthClient(...) + AuthClient class
    react.tsx           AuthProvider, useAuth, SignInButton, SignIn
    internal/
      cache.ts          ICache, InMemoryCache, LocalStorageCache, CacheManager (keying+expiry)
      worker.ts         token.worker bootstrap (refresh-token isolation)
      locks.ts          navigator.locks wrapper + browser-tabs-lock fallback
      popup.ts          openPopup (sync), runPopup (poll closed, 60s timeout)  [no iframe.ts — §2/§4.4]
      relay.ts          postMessage + BroadcastChannel/localStorage fallback listener; origin/type/state validation
      pkce.ts           generateVerifier, s256Challenge (Web Crypto)
      transaction.ts    persist/recover {verifier,state,nonce,redirect_uri} in sessionStorage (§4.3) — Auth0 TransactionManager parity
      transport.ts      authorize URL build, /__zs/auth/token + /__zs/auth/session calls, app_ref capture
      breadcrumb.ts     zs.<app_ref>.is.authenticated cookie read/write/clear
```

`package.json` `exports`:

```json
{
  "exports": {
    ".":        { "types": "./dist/server.d.ts",  "import": "./dist/server.js" },
    "./client": { "types": "./dist/client.d.ts",  "import": "./dist/client.js" },
    "./react":  { "types": "./dist/react.d.ts",   "import": "./dist/react.js" },
    "./types":  { "types": "./dist/types.d.ts",   "import": "./dist/types.js" }
  },
  "peerDependencies": { "react": ">=18" },
  "peerDependenciesMeta": { "react": { "optional": true } }
}
```

`zeroship` + `@zeroship/*` stay external (server.ts imports `env` from `zeroship`). The client and
react entries have **no** runtime dependency on `zeroship` — they are pure browser code.

### 4.2 Public TypeScript surface

`src/types.ts`:

```ts
export interface User {
  /** Per-app pairwise subject — an opaque `pws_…` TEXT id (NOT a UUID). §6.3. */
  id: string;
  /** Per-app relay email alias (…@{relay_domain}); null if email scope not granted. */
  email: string | null;
  emailVerified: boolean;
  name: string | null;
  avatar: string | null;
  /** Scopes granted to this app for this user (from WorkerUser.scopes / the access token). */
  scopes: string[];
}

export interface Session {
  /** The gateway per-app WRAPPER access token: its `sub` is the `pws_`, NEVER the global UUID,
      so decoding it cannot correlate the user across apps (§1.2). Sent as `Authorization: Bearer`. */
  access_token: string;
  /** Present only when useRefreshTokens and not held in the Web Worker. */
  refresh_token?: string;
  /** Unix seconds. */
  expires_at: number;
  token_type: "Bearer";
  user: User;
  scopes: string[];
}

export type AuthChangeEvent =
  | "SIGNED_IN" | "SIGNED_OUT" | "TOKEN_REFRESHED" | "USER_UPDATED";

export type AuthErrorCode =
  | "login_required" | "consent_required" | "interaction_required"
  | "invalid_grant"  | "missing_refresh_token"
  | "popup_closed"   | "popup_blocked"   | "timeout"
  | "scope_required" | "invalid_state"   | "network_error"
  | "server_error"   | "config_error";

export class AuthError extends Error {
  readonly code: AuthErrorCode;
  readonly status?: number;
  readonly cause?: unknown;
  constructor(code: AuthErrorCode, message: string, opts?: { status?: number; cause?: unknown });
}

export type CacheLocation = "memory" | "localstorage";

export interface ICache {
  set<T>(key: string, value: T): Promise<void> | void;
  get<T>(key: string): Promise<T | undefined> | (T | undefined);
  remove(key: string): Promise<void> | void;
  allKeys?(): Promise<string[]> | string[];
}

export interface AuthClientOptions {
  /** Defaults to location.origin. The app's same-origin gateway host. */
  appOrigin?: string;
  /** Default "memory". */
  cacheLocation?: CacheLocation;
  /** Custom cache backend (overrides cacheLocation). */
  cache?: ICache;
  /** Default false. When true (+ memory), refresh tokens live in a Web Worker. */
  useRefreshTokens?: boolean;
  /** Default ["openid","profile","email"]. */
  scope?: string[];
  /** Seconds before expiry to proactively refresh. Default 60. */
  refreshSkewSeconds?: number;
}

export interface SignInOptions {
  provider?: "google" | "github" | "password";
  scopes?: string[];
  /** Default true. Popup vs full-page redirect. */
  popup?: boolean;
  /** Where to return after a redirect (popup ignores). */
  redirectTo?: string;
}

export interface SignOutOptions { scope?: "local" | "global"; }

export interface Scope { id: string; label: string; description?: string; }
```

`src/client.ts` (the `./client` export):

```ts
export interface AuthClient {
  /** Interactive sign-in. Resolves with the Session on success. */
  signInWithOAuth(opts?: SignInOptions): Promise<Session>;
  /** Phase-1: launches the popup to the hosted password page (no embedded form). */
  signInWithPassword(opts?: { scopes?: string[]; popup?: boolean }): Promise<Session>;
  /** Phase-1: launches the popup to the hosted OTP/magic-link page. */
  signInWithOtp(opts?: { email?: string; scopes?: string[] }): Promise<{ sent: true }>;

  /** Exchange an authorization code (popup relay or redirect callback) for a session. */
  exchangeCodeForSession(code: string, state?: string): Promise<Session>;

  /** Cheap, local. Returns the cached session or null. No network. */
  getSession(): Promise<Session | null>;
  /** Server-validated user via GET /__zs/auth/session (gateway-validated `user`; always probes,
      bypassing the breadcrumb — §4.3). The SDK never trusts a browser-decoded id_token. */
  getUser(): Promise<User | null>;
  /** Force a refresh: browser-family rotation (browser_refresh) or /session?mint=1 (server_anchor). */
  refreshSession(): Promise<Session>;
  /** Rehydrate after reload: breadcrumb-gated; first-party GET /__zs/auth/session?mint=1. No iframe. */
  checkSession(): Promise<Session | null>;

  /** True if a non-expired session is cached. Cache-derived, no network. */
  isAuthenticated(): boolean;
  /** Does the current session carry this scope? */
  hasScope(scope: string): boolean;
  /** Step-up: acquire a token with additional scopes via interactive popup (prompt=consent). */
  requestScopes(scopes: string[]): Promise<Session>;
  /** Returns a valid access token, refreshing if needed (deduped, lock-serialized). */
  getAccessToken(): Promise<string>;
  /** Step-up via popup specifically (Auth0 getAccessTokenWithPopup parity). */
  getAccessTokenWithPopup(opts?: { scopes?: string[] }): Promise<string>;

  onAuthStateChange(cb: (event: AuthChangeEvent, session: Session | null) => void):
    { unsubscribe(): void };

  signOut(opts?: SignOutOptions): Promise<void>;
}

export function createAuthClient(options?: AuthClientOptions): AuthClient;
```

`src/server.ts` (the `.` export — repaired, `env.auth`-backed):

```ts
import { env } from "zeroship";
// User type re-exported from ./types
export const auth = {
  getUser(): User | null,        // env.auth.getUser()
  requireUser(): User,           // env.auth.requireUser() (throws → 401 at dispatch)
  isLoggedIn(): boolean,
  /** Returns a 302 Response to POST-less signout; the gateway clears the anchor. */
  signOut(returnTo?: string): Response,   // → /__zs/auth/signout (now handled — bug fixed)
};
export default auth;
```

`src/react.tsx` (the `./react` export):

```ts
export interface AuthContextValue {
  isAuthenticated: boolean;
  isLoading: boolean;
  error: AuthError | null;
  user: User | null;
  session: Session | null;
  signInWithOAuth(opts?: SignInOptions): Promise<void>;
  signInWithPassword(opts?: { scopes?: string[] }): Promise<void>;
  signOut(opts?: SignOutOptions): Promise<void>;
  getAccessToken(): Promise<string>;
  getAccessTokenWithPopup(opts?: { scopes?: string[] }): Promise<string>;
  hasScope(scope: string): boolean;
}
export function AuthProvider(props: {
  children: React.ReactNode;
  options?: AuthClientOptions;
}): JSX.Element;
export function useAuth(): AuthContextValue;
export function SignInButton(props: {
  provider?: SignInOptions["provider"];
  scopes?: string[];
  children?: React.ReactNode;
}): JSX.Element;
/** Convenience wrapper that renders a default sign-in launcher. */
export function SignIn(props: { scopes?: string[] }): JSX.Element;
```

`AuthProvider` mount logic (auth0-react parity): build one client; on mount, if
`hasAuthParams(location)` (a `code`+`state` in the query — the full-page redirect path)
→ `exchangeCodeForSession` then `history.replaceState` to strip the query; else `checkSession()`.
`SignInButton` calls `signInWithOAuth({popup:true})` so `window.open` fires synchronously inside
the click handler.

### 4.3 Cache, worker, locks, breadcrumb (Auth0 internals)

- **The browser keys on a stable per-app `app_ref`, NOT the host.** The SDK doesn't know the Hydra
  `client_id` (the gateway injects it). Auth0 keys on `client_id`; we replace it with a stable
  **`app_ref`** the **gateway returns** on the first `/__zs/auth/token` and `/__zs/auth/session`
  response (a short opaque per-app id derived from `app_id`, e.g. `app_7Fk…` — the same value for
  every host the app serves). The SDK caches `app_ref` in `sessionStorage` and uses it for all
  keying. This makes apex + custom domains share **one** cache namespace, **one** breadcrumb, and
  **one** refresh lock — fixing the split-session bug where moving between `myapp.zeroship.ai` and a
  custom domain looked logged-out on one even though the IdP/anchor session was live. <!-- Added in round 1: addressing MINOR — apex+custom-domain shared keying via gateway-returned app_ref -->
- **CacheManager keying**: `@@zsauth@@::<app_ref>::<scope-sorted>` for token entries;
  `@@zsauth@@::<app_ref>::@@user@@` for the user-profile entry. `CacheEntry = {
  access_token, id_token?, refresh_token?, token_type, expires_in, user, scope, app_ref }`;
  `expiresAt = floor(now/1000)+expires_in`. On read, evict expired entries unless a refresh token is
  present. ⚠️ **The `user` profile comes from the gateway's server-validated `user` field**
  (§1.2 `/token`, §1.3 `/session`) — the SDK **does not** decode the id_token to build the profile
  (it can't verify its signature). The id_token, if cached, is opaque pass-through only; trusting it
  would let an XSS-influenced token response forge a profile. <!-- Added in round 2: addressing MAJOR — SDK profile is the gateway-validated `user`, not a decoded id_token -->
- **PKCE transaction persistence (round-3, MAJOR).** `pkce.ts` mints `nonce` alongside
  `state`+`verifier`+`redirect_uri` and `transaction.ts` persists the whole tuple in
  **`sessionStorage`** under a `@@zsauth@@::txn::<state>` key — a separate concern from
  `cacheLocation` (which only governs where *tokens* live). The tuple is also mirrored into the
  in-memory flow map as a fast path, but `sessionStorage` is the durable source so an **opener reload
  mid-popup** can still complete the exchange. On `relay`/`exchangeCodeForSession` the SDK first
  consults the in-memory map, then falls back to `sessionStorage` for the matching `state`. The entry
  is **cleared on completion, on a `state` mismatch, and on the 60s popup timeout** (so stale
  transactions never accumulate or get replayed). `sessionStorage` is per-tab and same-origin, so a
  cross-tab attacker cannot read it; the PKCE verifier is still never sent anywhere but the
  same-origin `/token` proxy. Recovery behavior on opener reload: `AuthProvider` mount (and
  `checkSession`) detect a pending `@@zsauth@@::txn::*` entry whose popup may already have delivered
  a `code` via a `localStorage`/`BroadcastChannel` relay (§4.4) and resume the exchange; if no code
  arrived, the next relay `postMessage` matches the persisted `state` and the exchange proceeds. <!-- Added in round 3: addressing MAJOR — persist verifier/state/nonce/redirect_uri in sessionStorage; specify reload-recovery and clearing -->
- **Nonce binding (browser side).** The persisted transaction holds `nonce` (with `state`+`verifier`).
  `relay.ts` matches the relay message's `state`, recovers `verifier`+`nonce` from the in-memory map
  or `sessionStorage`, and `transport.ts` echoes `nonce` in the `/token` body. ⚠️ **Round-3 honesty
  (MAJOR) — this nonce echo is defense-in-depth against accidental cross-flow id_token mixups, NOT a
  security guarantee against an XSS-controlled client** (see §1.2): the gateway is stateless across
  `/authorize`→`/token`, so the value it checks against is supplied by the same client the trust
  model distrusts. A relay message whose `state` matches no persisted transaction → `invalid_state`
  (CSRF/replay guard on the `code`). <!-- Added in round 3: addressing MAJOR — nonce echo is defense-in-depth, not an OIDC nonce guarantee against an untrusted client -->
- **InMemoryCache**: closure over a plain object (default). **LocalStorageCache**: opt-in,
  documented XSS tradeoff. **Custom `ICache`** honored when supplied.
- **Web Worker** (`internal/worker.ts`): only when `window.Worker && useRefreshTokens &&
  cacheLocation === 'memory'`. Holds the browser refresh family in worker-isolated memory; the
  main-thread cache entry has `refresh_token` stripped; refreshes are proxied to the worker.
  Strongest in-browser posture. (In the default `server_anchor` mode the browser holds no refresh
  token at all, so the worker holds nothing — the server is the holder.)
- **Locks** (`internal/locks.ts`): `navigator.locks.request('zs.refresh.<app_ref>', {signal:
  AbortController(5000)}, fn)` with a `browser-tabs-lock`-style fallback for browsers without the
  Web Locks API. Wraps every browser-family refresh. Because the key is `app_ref` (host-stable),
  refreshes serialize across the apex and custom-domain tabs too.
- **Breadcrumb** (`internal/breadcrumb.ts`): `zs.<app_ref>.is.authenticated=true`, non-HttpOnly,
  `SameSite=Lax`, `Secure`, `Path=/`, `Max-Age` matching the anchor (30 days). ⚠️ **Round-2
  hardening (MAJOR) — the breadcrumb is an OPTIMIZATION ONLY and never a security boundary; the
  HttpOnly anchor is authoritative.** It is non-HttpOnly so JS can read it and therefore trivially
  forgeable (any first-party JS or XSS can set/clear it), so it must never gate anything that grants
  authority — it only decides whether to *skip a network probe*. Two concrete consequences: <!-- Added in round 2: addressing MAJOR — breadcrumb is non-authoritative; gateway writes it; specify both desync directions; periodic force probe -->
  - **The gateway writes the breadcrumb server-side** on every `/__zs/auth/token` and
    `/__zs/auth/session` response (still non-HttpOnly so the SDK can read it, but written by the
    server so it tracks the anchor and cannot drift away from it under normal operation), and clears
    it on `/__zs/auth/signout`. The SDK also writes it as a fast-path but the server write is the
    source of truth.
  - **Both desync directions are specified.** (a) *Breadcrumb says authenticated but the anchor is
    gone* → `/session?mint=1` returns `401 login_required`; the SDK clears the breadcrumb and goes
    anonymous (correct). (b) *Breadcrumb absent but the anchor is live* (e.g. an XSS cleared it, or
    a stale tab) → a naive early-return would suppress recovery and the user would look logged out
    despite a live anchor. To prevent a **cleared breadcrumb from permanently suppressing
    recovery**, the SDK runs an **unconditional `/session` probe** (a) once on first
    `checkSession()` per page load **regardless** of the breadcrumb, and (b) when the app calls
    `getUser()`/`refreshSession()` explicitly (these always probe, `force:true`). The breadcrumb
    only suppresses the *repeat* background probes within a load, never the first one — so a forged
    or cleared breadcrumb can at most add/remove one cheap network call, never grant or deny a
    session. The breadcrumb is cleared on `signOut`.


### 4.4 Popup / relay mechanics (no iframe)

- `openPopup()` returns `window.open('', 'zs:auth', features)` called **synchronously** in the
  user gesture; `runPopup(popup, url)` sets `popup.location.href = url`, installs the message
  listener, and polls `popup.closed` every 1000ms → `AuthError('popup_closed')`; 60s timeout →
  `AuthError('timeout')`.
- **No `runIframe`.** The hidden-iframe `prompt=none` path is removed (§2): it would require
  reading Hydra's session as a third-party cookie. Silent renewal and reload-recovery go through
  `/__zs/auth/session?mint=1` (first-party). Step-up for new scopes (`requestScopes`) and recovery
  after the anchor expires both use the **interactive popup** (`prompt=consent` / `prompt=login`),
  never an iframe. <!-- Added in round 1: addressing BLOCKER — remove iframe mechanics from SDK -->
- `relay.ts` listens for `message`, validates `e.origin === appOrigin && e.data?.type ===
  'zs:authorization_response'`, matches `response.state` to the persisted transaction (in-memory map
  → `sessionStorage` fallback; else `AuthError('invalid_state')`), recovers the PKCE verifier,
  resolves/rejects. `response.error` maps to the typed `AuthError` (`login_required`,
  `consent_required`). When a message arrives from the wrong origin the SDK logs a distinct
  `config_error` rather than silently waiting for the 60s timeout. <!-- Added in round 1: addressing MINOR — origin-mismatch distinct error not just timeout -->
- ⚠️ **Round-3 fix (MINOR) — COOP across the three popup navigation legs + a `window.opener`-null
  fallback.** The popup navigates **app-origin** (`/authorize`) → **auth-origin** (`auth.zeroship.ai`
  login/consent) → **app-origin** (`/popup-callback`). The `/popup-callback` page sets
  `Cross-Origin-Opener-Policy: same-origin` (§1.2), and `auth.zeroship.ai` runs its own login UI with
  its own COOP. On these cross-origin transitions some browsers (notably Safari, and any UA applying
  COOP-driven browsing-context-group swaps) can **sever `window.opener`**, so the popup's
  `postMessage(msg, location.origin)` to `window.opener` may target `null`, and the opener's
  `popup.closed` poll can read `true` prematurely. The design no longer asserts "no COOP analysis
  needed." Instead: <!-- Added in round 3: addressing MINOR — analyze opener across app→auth→app legs; add BroadcastChannel/localStorage fallback when window.opener is null; don't rely on popup.closed under COOP -->
  - **Primary path** stays `window.opener.postMessage` (works when the opener handle survives — the
    common Chromium/Firefox case, where the final `/popup-callback` page is same-origin to the
    opener).
  - **Fallback path (opener severed):** when `window.opener` is `null` at relay time, the
    `/popup-callback` script ALSO posts the `{code,state}` envelope over a **same-origin
    `BroadcastChannel('zs:auth:<app_ref>')`** and writes a one-shot
    `localStorage['@@zsauth@@::relay::<state>']` item (then removes it). The opener's `relay.ts`
    listens on all three (postMessage, BroadcastChannel `message`, and `storage` events) and takes
    whichever arrives first, de-duplicated by `state`. Both fallbacks are same-origin and validated by
    `state`, so they carry no cross-origin exposure.
  - **Close/timeout signaling no longer depends on `popup.closed` alone:** the SDK resolves on the
    relay message (any channel) or rejects on an explicit 60s timeout; `popup.closed === true` is
    treated as a *hint* to emit `popup_closed` only if **no** relay has arrived, so a COOP-induced
    spurious `closed===true` cannot cancel a flow whose code already came back over BroadcastChannel.
  - **Test:** simulate `window.opener === null` at relay time and assert the BroadcastChannel/storage
    fallback completes the exchange; assert a spurious early `popup.closed` does not cancel a flow
    whose relay already delivered the code.

## 5. Subsystem 3 — declared permission scopes

### 5.1 Scope registry — two distinct grant namespaces

⚠️ **This is the structural fix for the consent authorization-inversion.** The existing consent
handler's `grantor_can_grant_requested_scopes` (`crates/auth/src/ui/consent.rs:492`) is a
**platform-delegation / admin** check: it parses each scope, builds `AuthzContext{ principal_id =
subject }`, and calls `authz::is_authorized_anywhere(db, platform_policies, ctx)`. That answers
*"is this principal authorized **by platform policy** to delegate this scope"* — the Phase-10
admin model (`apps:deploy`, `apps:read`, …). A normal end user of a creator app is **not** a
platform principal with policies, so for any app-declared scope this check returns `can_grant =
false` and the screen renders `CANNOT_GRANT`. Reusing that gate for end-user app scopes is an
**inverted** authorization model — it would make `read:billing` un-grantable by the very user who
owns the data. <!-- Added in round 1: addressing BLOCKER — consent authz inversion -->

The fix is to recognize **two grant namespaces** and route each to the correct check. ⚠️
**Round-2 correction (BLOCKER):** the namespace boundary is *exactly* the closed
`zeroship_authz::Scope::parse` vocabulary, not a hand-listed prefix set — see the guard below. <!-- Added in round 2: addressing BLOCKER — namespace (a) is the full Scope::parse vocabulary, not just platform:/apps:/org: -->

| Namespace | Membership test | Examples | Who may grant | Check |
|---|---|---|---|---|
| **(a) Platform / delegated scopes** | `zeroship_authz::Scope::parse(id)` returns `Ok` (the **entire** closed Phase-10 vocabulary: `apps:*`, `env:*`, `secrets:*`, `billing:*`, `team:*`, `account:*`, `deployments:*`) **or** the reserved prefixes `platform:` / `org:` | `apps:deploy`, `billing:read`, `secrets:write` | a **platform principal** with a matching policy | `grantor_can_grant_requested_scopes` (unchanged — `is_authorized_anywhere`) |
| **(b) App-declared end-user scopes** (resolved from `control.app_scope_defs` for the consent's client) + the **reserved identity scopes** (`openid`/`profile`/`email`/`offline_access`) | `read:billing`, `write:projects`, `email` | the **resource owner** (the end user themself) — self-grantable; **no** platform privilege required | **self-grantable** — bypasses `is_authorized_anywhere` entirely |

⚠️ **Why the membership test must be `Scope::parse`, not a prefix list.** The existing grantor gate
does `filter_map(|raw| Scope::parse(raw).ok())` (`consent.rs:499`) — it **silently drops** any token
outside the closed vocabulary. So today an app-declared `read:billing` parses to `Err`, is dropped,
and the platform gate is a no-op for it; the inversion only *bites* scopes that happen to collide
with the Phase-10 vocabulary. But that vocabulary includes `billing:read`, `team:read`,
`account:read`, `env:read`, `secrets:read`, `deployments:read`, etc. (`scope.rs:118-136`), **none**
of which the round-1 guard rejected (it only rejected `platform:`/`apps:`/`org:`). An app declaring
`billing:read` would therefore be classified namespace-(b) self-grantable **and** also parse as a
real platform `Scope` — exactly the shadowing the guard is supposed to prevent. The membership test
for namespace (a) is therefore: **`Scope::parse(id).is_ok()` OR the id starts with a reserved
prefix (`platform:` / `org:`)**. Everything else is namespace (b) or unknown.

A user consenting to an app's *own* declared scope is granting access to *their own* relationship
with that app; it requires **no platform privilege**, so it must **not** hit the platform-policy
authz path. The consent handler classifies each requested scope by namespace and only the (a) set
is gated by `is_authorized_anywhere`; the (b) set is always self-grantable by the authenticated
subject. (Reserved identity scopes are likewise self-grantable — a user can always consent to
share their own name/email with an app.)

**Two tiers of app-facing scope content:**
- **Reserved identity scopes** (namespace (b), platform-defined, always self-grantable): `openid`,
  `profile`, `email`, `offline_access`. Fixed labels.
- **App-declared custom scopes** (namespace (b)): each app declares scopes in its manifest; the
  control plane stores them in `control.app_scope_defs` and mirrors the allowlist into the app's
  Hydra client `scope`. These are self-grantable end-user scopes — never platform-delegated.

**Where declared** — manifest field (`crates/bundle/src/manifest.rs`):

```jsonc
// manifest.json (excerpt)
"auth": {
  "scopes": [
    { "id": "read:billing",  "label": "View billing",  "description": "See invoices and plan." },
    { "id": "write:projects","label": "Manage projects","description": "Create and edit projects." }
  ]
}
```

Scope ids follow the `verb:resource` convention (lowercase, `:`-segmented). The manifest schema
adds a `Scopes` type; the control plane validates ids (regex `^[a-z][a-z0-9_]*(:[a-z][a-z0-9_]*)*$`)
and **rejects any id that collides with a platform-delegated scope**, defined as: <!-- Added in round 2: addressing BLOCKER — reject the FULL closed Scope::parse vocabulary, not just three prefixes -->

```
reject_app_scope(id) ⇔
     zeroship_authz::Scope::parse(id).is_ok()      // ANY of apps:/env:/secrets:/billing:/team:/
                                                    // account:/deployments: — the full closed set
  || id starts_with "platform:" or "org:"          // reserved future prefixes
  || id ∈ {openid, profile, email, offline_access} // bare reserved identity scopes
```

This closes the round-1 hole where `billing:read`/`team:read`/`account:read`/`env:read`/
`secrets:read`/`deployments:read` (all in the closed `Scope` vocabulary, `scope.rs:118-136`) were
**not** rejected — an app could declare one and have it classified self-grantable while it *also*
parsed as a real platform scope. Binding the manifest guard to `Scope::parse` means the app-scope
namespace and the platform-scope vocabulary are provably disjoint. Definitions are stored in
`control.app_scope_defs` (§8). On deploy, `sync_app_client` updates the Hydra client `scope` to
`"openid offline_access profile email " + declared ids`.

> A regression test must declare a manifest with `billing:read` and assert the control-plane
> validator **rejects** it (pre-fix it was accepted), and assert a non-colliding `read:billing` is
> accepted.

### 5.2 Consent UI + the self-grant path (`crates/auth/src/ui/consent.rs`)

**How the consent handler classifies scopes (the load-bearing change).** On each consent
challenge the handler resolves the requesting client's `app_id` (from the consent `client_id`),
loads that app's declared scopes from `control.app_scope_defs` (cached; exposed to the auth UI via
an internal control endpoint, or carried in Hydra client `metadata.scope_defs`), then partitions
the requested scopes with this **classifier**:

```
classify(scope, app_scope_defs):
    if scope ∈ {openid, profile, email, offline_access}  → SelfGrant   (namespace b — identity)
    else if scope ∈ app_scope_defs[app_id]               → SelfGrant   (namespace b — declared)
    else if Scope::parse(scope).is_ok()
         or scope starts_with "platform:" / "org:"        → Delegated   (namespace a)
    else                                                  → Unknown
```

⚠️ **Round-2 fix (BLOCKER) — the classifier replaces the silent `filter_map` drop AND is applied
on BOTH the GET render path and the POST accept path.** Round 1 described the partition only for the
GET render. But the load-bearing enforcement is `post_consent_accept` (`consent.rs:172`), which
**also** calls `grantor_can_grant_requested_scopes` and re-renders `CANNOT_GRANT` on `Ok(false)`.
If only the GET render is patched, an end user still cannot complete consent for `read:billing`. So:
<!-- Added in round 2: addressing BLOCKER — apply the namespace-(b) bypass in BOTH get_consent render AND post_consent_accept; replace the silent filter_map drop with an explicit Unknown→invalid_scope reject -->

1. **`get_consent` (render, `consent.rs:127`)** and **`post_consent_accept` (`consent.rs:172`)**
   both replace the bare `grantor_can_grant_requested_scopes(&info)` call with: classify all
   requested scopes; feed **only** the `Delegated` subset to `grantor_can_grant_requested_scopes`
   (which keeps its existing `is_authorized_anywhere` body but receives a pre-filtered scope list,
   no longer relying on `filter_map`-drop); treat the `SelfGrant` subset as always grantable by the
   authenticated subject; and **reject the whole consent with `invalid_scope`** if any scope is
   `Unknown`. The accept path performs the *identical* classification before `accept_consent`, so a
   self-grantable `read:billing` reaches `accept_consent` on POST, not just on render.
2. **`grantor_can_grant_requested_scopes` no longer silently drops.** Its internal
   `filter_map(Scope::parse(raw).ok())` is replaced: it now receives only the `Delegated` subset
   (already known to be `Scope::parse`-able), and any token that fails `Scope::parse` *within that
   subset* is a hard error, not a drop. The `Unknown` bucket is handled by the classifier above
   (→ `invalid_scope` reject), not by silent discard. This implements the doc's "reject invalid
   scope" branch instead of assuming it.
3. ⚠️ **Round-3 fix (MINOR) — reconcile the Unknown→`invalid_scope` policy with the live
   `scope_views` renderer AND guarantee a deploy-race cannot brick login.** The existing
   `scope_views` (`consent.rs:600-622`) renders any scope that fails `Scope::parse` /
   `standard_scope_label` as `unrecognized: true` rather than rejecting, and the test
   `renders_scope_labels` (`consent.rs:730-744`) asserts `custom-scope` renders `unrecognized`.
   Hard-rejecting *every* unrecognized scope would break app-declared scopes (which `scope_views`
   doesn't know about) and that test. Round 3 resolves it: <!-- Added in round 3: addressing MINOR — reconcile Unknown-reject with scope_views/renders_scope_labels and define the deploy-ordering that prevents an app_scope_defs/Hydra-allowlist race from bricking login -->
   - **`scope_views` is extended to consult `app_scope_defs`** for the consent's `app_id`, so an
     app-declared `read:billing` renders with its declared label and is **recognized** (not
     `unrecognized`). After this, the *only* `unrecognized`/`Unknown` scopes are ones that are
     neither platform-vocab, nor reserved-identity, nor app-declared — genuinely undefined. The
     `renders_scope_labels` test is **updated in the same patch** (per the no-back-compat rule): a
     scope present in `app_scope_defs` renders recognized; a scope present nowhere renders
     `unrecognized` AND causes the gate to reject `invalid_scope`. (We do not keep a "render
     unrecognized but proceed" path — the classifier is the single authority.)
   - **Deploy-race guard (the brick-login risk).** A single not-yet-known platform/app scope (e.g.
     during a deploy where the Hydra client `scope` allowlist updated before `app_scope_defs`, or vice
     versa) must NOT `invalid_scope`-brick *all* logins for the app. The control plane updates
     **`app_scope_defs` and the Hydra client `scope` allowlist atomically** in the same deploy
     transaction (both derive from the same manifest `auth.scopes`), so an authorize request can never
     carry a scope that Hydra's allowlist accepts but `app_scope_defs` hasn't yet learned (Hydra
     rejects a non-allowlisted scope at `/authorize` before consent is ever reached, so the inputs to
     the classifier are always a subset of the allowlist, which equals `app_scope_defs ∪ identity ∪
     declared-platform`). The atomic update is the invariant that makes "Unknown at consent time"
     unreachable for a correctly-deployed app — leaving `invalid_scope` only for a genuinely bogus
     authorize request. A deploy-ordering test asserts the two updates land in one transaction and
     that an authorize with a just-declared scope is never rejected.

Only the namespace-(a) `Delegated` subset is fed to `is_authorized_anywhere`. The namespace-(b)
`SelfGrant` subset (app-declared + identity) is **always** self-grantable by the authenticated
subject — it never touches `load_platform_policies`. For the end-user-facing creator-app flow (the
only flow this spec adds), **all** requested scopes are namespace (b), so the consent screen renders
**and** the POST accept succeeds, and an ordinary end user **can** grant `read:billing`. The
Phase-10 console/admin consent (namespace (a)) keeps its existing delegation gate untouched.

The consent screen renders per-scope line items:
- **Identity group** (`openid`/`profile`/`email`): "Your name and email" (self-grantable).
- **App-defined group**: one row per requested custom scope, `label` + `description` from
  `control.app_scope_defs` (self-grantable).

⚠️ **Round-3 fix (MAJOR) — ONE grant ledger, and per-app end-user clients do NOT set `skip_consent`.**
Round 2 introduced a **parallel** `auth.app_grants` ledger keyed by `(app_id, global_user_id)` for
delta detection, while the live consent handler already has a remembered-grant fast path reading
`control.oauth_grants` keyed by `(subject, client_id)` (`get_consent`, `consent.rs:69-125`:
`load_oauth_grant`/`upsert_oauth_grant`/`scopes_are_subset` under `skip_consent`). Two ledgers,
written/read by two different actors (the auth consent handler vs the gateway), with no consistency
mechanism, can disagree — causing either an un-prompted scope escalation or an **infinite
`prompt=consent` loop** (consent is granted but one ledger still detects a delta). Round 3 collapses
this to a single source of truth: <!-- Added in round 3: addressing MAJOR — drop auth.app_grants; use control.oauth_grants as the single ledger; per-app clients do NOT set skip_consent; reconcile with the live get_consent subset fast path -->

- **`auth.app_grants` is DROPPED.** The remembered-grant ledger is the **existing
  `control.oauth_grants`** keyed by `(subject, client_id)`. Because the per-app `client_id` is
  `app_<base62-app-id>` (deterministic from `app_id`, §1.1), `(subject, client_id)` is 1:1 with
  `(global_user, app)` — no second table needed. (The `auth.app_user_identities` row, §6.3, remains
  the **pairwise/relay** mapping; it is not a grant ledger.)
- **Per-app end-user clients do NOT set `skip_consent`.** The `get_consent` silent-accept fast path
  (`consent.rs:69-125`) only fires for `client.skip_consent`; per-app clients leave it **false**, so
  every consent challenge runs the round-3 classifier (§5.2) — including the `Delegated`/`SelfGrant`
  partition and the delta detection — rather than the legacy subset auto-accept. This removes the
  two-actor disagreement entirely: the **auth consent handler** is the sole writer of
  `control.oauth_grants` (on `accept_consent`) and the sole reader for delta detection.
- **Delta detection runs in the auth consent handler, not the gateway.** When the SDK opens the
  popup for a new scope, the consent handler loads `control.oauth_grants` for `(subject, client_id)`,
  computes the **delta** vs the request, renders only the delta rows, and on accept upserts the union
  back into `control.oauth_grants` (the same `upsert_oauth_grant` the live code uses). Hydra's
  consent `remember` flag is set so a no-delta repeat login skips the screen; a delta forces the
  screen. The gateway does **not** read a grant ledger for delta detection — it stays out of the
  consent decision (preserving "dumb gateway" and removing the second reader).
- **Test (round-3):** grant `{a}`, then request `{a,b}` → **exactly one** consent prompt showing only
  `{b}`, accept upserts `{a,b}` to `control.oauth_grants`, and a subsequent `{a,b}` login shows **no**
  prompt (no loop). A per-app client is asserted to have `skip_consent = false`.

**Regression tests (must fail pre-fix) — driven through the POST accept path, not just render:** <!-- Added in round 2: addressing BLOCKER — assert the end-user-can-grant against post_consent_accept, plus the Unknown→invalid_scope branch -->
1. An *ordinary end user* (a plain `usr_…` with **no** platform policy) **POSTs** `/consent/accept`
   for an app that declares `read:billing` and the grant **succeeds**: `accept_consent` is called,
   `control.oauth_grants` records the scope (the single ledger, round-3), and the access token carries
   `scope: "… read:billing"`. (Pre-fix the POST handler re-rendered `CANNOT_GRANT` because the scope
   hit `is_authorized_anywhere` with an empty policy set — asserting against render alone would have
   missed it.)
2. The same end user POSTing consent for a namespace-(a) `apps:deploy` they cannot delegate still
   gets `CANNOT_GRANT` from the **POST** path.
3. A consent challenge requesting an `Unknown` scope (declared by no app, not in the platform
   vocabulary) is **rejected with `invalid_scope`** on both render and accept (pre-fix it was
   silently dropped by `filter_map` and the consent proceeded as if it weren't requested).

### 5.3 Enforcement

- **Gateway Bearer arm** (§1.3) checks the access token's `scope` claim against the rule/policy's
  `required_scopes`. Missing scope → **403** `{ "error": "scope_required", "scope": "read:billing" }`.
- **Manifest rule-level scopes**: `Rule` gains an optional `required_scopes: Vec<String>`
  (`crates/bundle/src/rule.rs`), compiled into `EffectivePolicy`. A route can demand
  `read:billing` independent of the app-wide `AuthLevel`. The gateway enforces it in the Bearer
  arm and (for cookie sessions) against the session's granted scopes.
- **Worker visibility**: the granted scopes ride in `ZeroShip-User` (extend `WorkerUser` with
  `scopes: Vec<String>`), so `env.auth.getUser().scopes` and RPC `ctx.user.scopes` are available
  for app-level checks.

### 5.4 SDK scope surface

`signInWithOAuth({ scopes })`, `getSession().scopes`, `hasScope(s)`, `requestScopes([…])`
(step-up), `getAccessTokenWithPopup({ scopes })`. The React `useAuth().hasScope` mirrors it.

## 6. Subsystem 4 — pairwise subject identifiers

### 6.1 Goal

Each app sees a stable-per-app pseudonymous `sub` (`pws_…`), never the global `usr_…`. Two apps
get different subs for the same human; the platform retains a private mapping for support, relay,
and revocation.

### 6.2 Mechanism — gateway-translated pairwise at the header boundary (locked default)

⚠️ **Round-2 relocation (BLOCKER).** Round 1 said the `pws_…` sub is "computed and set as the
accepted OIDC subject at `accept_consent`." **That is architecturally impossible against the code,
and §6 is rewritten to match the actual subject-binding site.** The grounded facts: <!-- Added in round 2: addressing BLOCKER — pairwise was mis-located at accept_consent; relocate to the gateway header boundary -->

- **Hydra's OIDC subject is bound at the LOGIN stage, not consent.** `login.rs:450` does
  `AcceptLoginRequest { subject: u.id.to_string(), .. }` (the global `usr_` UUID); the skip path
  `login.rs:100` reuses `info.subject`; `oauth_google.rs`, `oauth_github.rs`, `magic.rs:634/966`
  all `accept_login` with the global UUID. `AcceptConsentRequest` (`consent.rs:194`) has **no
  subject field** — by consent time Hydra has already bound the global UUID. So a `pws_` subject
  cannot be emitted at `accept_consent`; you would have to rewrite every `accept_login` call site.
- **The subject is hard-parsed as a UUID in at least four consumers a `pws_` text value would
  break:** `consent_subject_uuid()` (`consent.rs:337` `Uuid::parse_str(&info.subject)`),
  `grantor_can_grant_requested_scopes()` (`consent.rs:505` `Uuid::parse_str(&info.subject)`),
  `gateway/sessions.rs:59/158` (`revoke_all_for_user` / session create both `Uuid::parse_str` the
  `user_id`, and the `auth.gateway_sessions.user_id` **column is `uuid`**), and the back-channel
  logout pipeline (`backchannel_logout.rs:101` `wrapper_revocation::subject_uuid(sub)` with the
  explicit comment *"non-UUID sub cannot enter wrapper denylist"*). `build_id_token_claims` and the
  audit pipeline are also keyed on the UUID subject.

**So rewriting `accept_login` to emit `pws_` is both invasive and wrong-shaped** for SSO: the
per-app sector is **not known at login time** for an SSO'd session shared across apps (the login
challenge that Hydra skips via the `remember` flag is not per-app). Forcing a fresh login challenge
per app to learn the sector would defeat SSO and the `remember` flag.

**F4-B (locked default) — keep Hydra's subject = the global `usr_` UUID end-to-end, and translate
to the per-app `pws_…` exactly once, at the gateway's `ZeroShip-User` header boundary.** Hydra,
login, consent, id_token, BCL, `gateway_sessions`, and wrapper-revocation all keep operating on the
UUID and nothing in those pipelines changes type. The pairwise derivation is:

```
pws = "pws_" + base62( HMAC-SHA256(pairwise_salt, global_user_id || ":" || app_sector) )[:20]
```

computed **deterministically** by the gateway (and cached in `auth.app_user_identities` for the
relay/reverse-lookup, §6.3). The two gateway paths:

- **Cookie path:** the gateway already loads the `auth.gateway_sessions` row (UUID `user_id`) on
  every request and emits `ZeroShip-User`. It now derives `pws = derive_pairwise(user_id,
  route.sector_identifier)` and writes **that** into `ZeroShip-User.id` instead of the UUID. (The
  row is also upserted into `auth.app_user_identities` so the relay handler and support tooling can
  reverse `pws → (app, global_user)`.)
- **Bearer path (§1.3):** two cases. (i) The **browser-held wrapper** token already carries `sub =
  pws_` (the gateway minted it that way at `/token`/`?mint=1`, §1.2), so the Bearer arm reads `pws_`
  straight from the wrapper — no derivation, and **no global UUID ever reaches the browser**. (ii) A
  **non-browser raw Hydra access JWT** carries the **global** UUID `sub`; before `encode_user_header`
  the Bearer arm derives `pws = derive_pairwise(jwt.sub, route.sector_identifier)`. In neither case is
  any token rewritten — the wrapper is *minted* with `pws_`, the raw Hydra JWT is projected only into
  the worker header. <!-- Added in round 3: addressing MAJOR — browser-held wrapper already carries pws_; global UUID never in a browser token -->

This makes the pairwise sub a **pure gateway projection**: zero changes to Hydra config (stays
`subject_type: public`), zero changes to any UUID-parsing consumer, and no `sector_identifier_uri`
JSON dependency. The outward contract is identical to what round 1 promised — every app sees a
stable per-app `pws_…` in `ZeroShip-User.id` and in `User.id` — but the mechanism is correctly
located at the **header projection**, the only place that is per-app.

> **Why NOT Hydra-native pairwise (F4-A), and why NOT subject-at-consent.** (a) Hydra **requires** a
> `sector_identifier_uri` whenever a `pairwise` client has >1 `redirect_uri` (Ory docs + issue
> #3898); every app client here has ≥2 (popup-callback + callback, ×N hosts), and Hydra fetches +
> re-validates that JSON on every `redirect_uri` mutation — fighting per-deploy redirect_uri churn.
> (b) Subject-at-consent is impossible (the subject is bound at login) and a `pws_` subject would
> break four UUID-parsing consumers. The gateway-projection approach sidesteps both: Hydra keeps
> emitting the UUID, and only the (per-app, per-request) header carries the `pws_`.

- **Sector identifier**: the app's stable apex origin (`https://myapp.zeroship.ai`), stored in
  `control.app_oauth_clients.sector_identifier` and shipped to the gateway as
  `RouteEntry.sector_identifier` (§1.5). Custom domains do **not** change the sector — all of an
  app's hosts share the apex sector, so the derived `pws_…` is stable across them.
- **Determinism is intentional** (see §6.4 for the re-grant reconciliation): the same `(global_user,
  app_sector)` always yields the same `pws_…`, so the gateway can derive it on the Bearer path
  without a DB round-trip and re-login always yields the same sub.
- **Salt**: a platform-wide secret (config), combined with the sector. Rotating the salt rotates
  all subs (a deliberate break-glass), so it lives in the same secret store as the worker key.

### 6.3 Mapping table + the `ZeroShip-User.id` type change

`auth.app_user_identities` (DDL §8) keyed on `(app_id, global_user_id)`. The `id` column **is** the
derived `pws_…` value (no separate `pairwise_sub` column). The platform's `/me`, sessions, audit,
BCL, and Hydra all still key on the **global** UUID; only the gateway-projected
`ZeroShip-User`/`User.id` carries the pairwise sub.

⚠️ **`ZeroShip-User.id` becomes a `pws_…` text id — a deliberate wire-format break, called out.**
Today the gateway cookie path puts a UUID into `ZeroShip-User.id`. With pairwise, the gateway derives
`pws = derive_pairwise(global_uuid, route.sector_identifier)` (or reads `pws_` straight from the
wrapper) and writes that text id: the **cookie redirect path** from the `gateway_sessions.user_id`
UUID, the **raw-Hydra Bearer path** from the access JWT's `sub` UUID, and the **browser wrapper
Bearer path** straight from the wrapper's `pws_` `sub`. So `ZeroShip-User.id` is **always a `pws_…`
text id** on the end-user app path — never a UUID — and the worker's `User.id` consumer, the SDK
`User.id` type, and `ctx.user.id` all treat it as an **opaque string**, not a UUID. ⚠️ Note: both
the legacy cookie-flow row (`auth.gateway_sessions`) and the SDK anchor row
(`auth.app_session_anchors`, §8.1) keep `*user_id UUID` for the **global** user; the `pws_…` text
never enters those columns — it is derived on the way out into the header (or minted into the
wrapper), not stored next to the UUID. <!-- Added in round 2: addressing BLOCKER — pws_ is a gateway projection, never stored in the UUID user_id column; Updated in round 3: anchor row is app_session_anchors; wrapper path reads pws_ directly -->

**Regression test (would fail pre-fix):** a request that resolves a cookie session for global user
`U` on app `A`'s host emits `ZeroShip-User.id == derive_pairwise(U, A.sector)`, and the **same**
global user `U` on app `B`'s host emits a **different** `pws_…`; both differ from `U`. A second
test asserts the Bearer arm projects the JWT's UUID `sub` to the same `pws_…` the cookie arm
produces for the same `(U, A.sector)`. (The end-to-end accept_login subject value is asserted to
flow as the **global UUID** all the way to the gateway, where the projection happens.)

### 6.4 Re-grant reconciliation (deterministic sub vs. "fresh on re-grant")

⚠️ **Round 1 contradicted itself** (MINOR): §6.2 said the sub is *deterministic* while §7.5 said a
re-grant mints a *fresh* sub (Apple Hide-My-Email semantics) — and the DDL made `id` (the `pws_`)
the PRIMARY KEY, so a deterministic re-derivation after revoke would **collide** with the revoked
row's PK. Round 2 resolves it decisively in favor of **determinism**: <!-- Added in round 2: addressing MINOR — reconcile deterministic sub vs fresh-on-re-grant + the PK collision -->

- The pairwise **sub** is deterministic and **reused** across revoke→re-grant: re-granting the same
  `(user, app)` yields the **same** `pws_…`. The `auth.app_user_identities` row is **upserted** (its
  `revoked_at` cleared) rather than inserting a colliding second row — so `id` (the `pws_`) being
  the PK is fine, because there is exactly one row per `(app, global_user)` and re-grant reuses it.
- Only the **relay email alias** is minted fresh on re-grant (Apple Hide-My-Email applies to the
  *alias*, not the sub): the old alias is freed (partial-unique, §7.1/§7.5) and a new token issued.
  This is the correct reading of Apple's model — the *email relay* rotates; a deterministic per-app
  identifier does not.

§7.5 is updated to say "fresh **alias**" (not "fresh sub"); the DDL drops the second-row assumption.
The `pws_` is never non-deterministic, so the Bearer path can always derive it without a lookup.

## 7. Subsystem 5 — relay email service

> **Split into its own spec (`2026-05-29-relay-email-design.md`).** Subsystem 5 is large
> (a deliverability-sensitive inbound-mail subsystem), depends on Subsystem 4's
> `auth.app_user_identities` mapping (the `(app, global_user) → relay_email` lookup the gateway
> reads when projecting `ZeroShip-User.email`, §6.2/§7.1), and is the least SDK-coupled piece. It is
> **summarized** here for the whole-vision picture but **slated as a separate spec + separate review
> gate** so the browser SDK can ship without waiting on MX warm-up. ⚠️ **Round-2 dependency note:**
> the alias substitution rides the same gateway projection step as the `pws_` sub (both are
> `(app, global_user)` lookups), so Subsystem 5's email-claim swap is **unblocked** by the round-2
> pairwise relocation rather than blocked by the old accept_consent mis-location. Slice 5 in §9
> references that spec. <!-- Added in round 2: addressing MAJOR — relay alias substitution rides the resolved gateway projection, not the broken accept_consent hook -->

### 7.1 Alias generation (generate-and-retry on conflict)

On **first consent** per `(app, user)` that grants the `email` scope (in `accept_consent`, after the
grant row is written — round-3: this is the **single** creation site, **not** lazily on the hot-path
projection; see the round-3 fix below), generate a relay alias `{token}@{relay_domain}` where
`token` is a 16-char base62 random string, and store it in `auth.app_user_identities.relay_email`. The
`email` claim and `ZeroShip-User.email` are the **alias** — never the real address. (Apps that
weren't granted `email` scope get `null`.)

⚠️ **Round-2 fix (MAJOR) — the substitution point is pinned at every place the email is emitted,
consistent with the pairwise gateway-projection model (§6.2).** Round 1 asserted "the email claim is
the alias" without identifying where the real email is swapped, even though
`build_id_token_claims(db, &info.subject, …)` (`consent.rs:191/419/657`) inserts the **real**
`u.email` from `users::find_by_id`. The three emit sites and their substitution: <!-- Added in round 2: addressing MAJOR — identify the exact email-claim substitution points (id_token, userinfo, ZeroShip-User.email) and pin them to the gateway projection -->

| Emit site | Today | Round-2 substitution |
|---|---|---|
| **`ZeroShip-User.email`** (worker header — the value apps actually read) | gateway puts the real email | gateway looks up `app_user_identities.relay_email` for `(app, global_user)` via a **read-through cache** (round-3: created at consent time, never lazily on the hot path) and writes the **alias** — same projection boundary as the `pws_` sub (§6.2). Hit ⇒ no DB round-trip; miss ⇒ one cached `SELECT`, never a write. This is the authoritative app-facing email. |
| **id_token `email` claim** (`build_id_token_claims`, `consent.rs:657`) | inserts real `u.email` | the consent handler, when the requesting client is a **per-app end-user client** (not the console/admin client), substitutes the alias: it resolves `app_user_identities.relay_email` for `(client→app_id, subject)` (minting it if absent) and inserts **that** into the `ConsentSession.id_token` claims (`ConsentSession.id_token` IS settable, `consent.rs:200/426`). The SDK never trusts the id_token anyway (§1.2), but the claim is kept correct so any introspection/userinfo derived from it is also the alias. |
| **`/userinfo`** (Hydra-served, derived from the id_token session claims set above) | derives from id_token claims | because the `email` claim written into `ConsentSession.id_token` is already the alias, `/userinfo` returns the alias with no extra hook. |

Because the email is **always** projected to the alias at (a) the gateway header and (b) the
id_token session-claims write, no real address reaches an app via any path. The **dependency on the
resolved pairwise relocation is explicit**: both the `pws_` sub and the relay alias are
`(app, global_user)`-keyed lookups in `auth.app_user_identities`, so the gateway derives the sub and
looks up the alias in the **same** projection step.

⚠️ **Allocation is generate-and-retry-on-conflict.** The `relay_email UNIQUE` constraint is made
**partial** — `UNIQUE … WHERE revoked_at IS NULL` (see §8.1 DDL) — so a *revoked* alias no longer
occupies the slot and a recycled token can re-issue. On the (low-probability) collision of two
*active* aliases, the allocator catches the unique violation and **retries with a fresh token**
(bounded retries, then surface `server_error`), rather than failing the consent. This closes the
"recycled token hard-fails re-grant" hole. <!-- Added in round 1: addressing MAJOR — partial-unique + generate-and-retry for relay_email -->

⚠️ **Round-3 fix (MINOR) — alias creation happens at CONSENT time (off the hot path), and the
gateway projection uses a READ-THROUGH cache, so the per-request `ZeroShip-User.email` projection is
not a write on the hot path.** Round 2 said the alias could be created "lazily on first gateway
projection," which would make every first request that projects `ZeroShip-User.email` a **DB write**
(generate-and-retry-on-conflict) on the per-request path — contradicting §6.2's "no DB round-trip"
pairwise claim for the email half, with no caching and an unspecified race. Round 3 pins it: <!-- Added in round 3: addressing MINOR — create alias at consent time off the hot path; read-through cache on the gateway; define the upsert concurrency contract; state per-request cost -->

- **Alias is created when the `email` scope is granted, in the consent handler's `accept_consent`
  path** (off the per-request hot path), upserting the `relay_email` into the
  `(app_id, global_user_id)` row of `auth.app_user_identities`. This is the same boundary that writes
  the grant, so it is a one-time cost at consent, not per request.
- **The gateway projection is READ-THROUGH cached.** The gateway keeps an in-process LRU keyed by
  `(app_id, global_user_id) → relay_email` (alongside the route cache). A hit ⇒ **no DB round-trip**
  for the email half (matching the `pws_` derivation's no-round-trip property). A miss ⇒ one
  `SELECT` (cached thereafter); the alias already exists from consent, so projection **never writes**
  on the hot path.
- **Concurrency contract for the consent-time create:** the upsert is
  `INSERT … ON CONFLICT (app_id, global_user_id) DO UPDATE SET relay_email = COALESCE(existing,
  excluded) … RETURNING relay_email` under the same advisory lock the consent grant uses, so two
  concurrent first-consents for the same `(app, user)` mint **exactly one** alias (the second sees
  the first's value). On the rare *active-alias* unique collision the generate-and-retry above
  applies. A concurrency test fires two simultaneous first-consents and asserts one alias.
- **Per-request DB cost when email is granted:** zero writes; zero reads on a cache hit; one read on
  the first miss per `(app, user)` per gateway process. Stated as the perf budget.

### 7.2 Inbound forwarding — managed provider, thin compio handler (default)

⚠️ **Default: do NOT build a from-scratch receiving MX in Rust.** Deliverability of forwarded mail
is governed by IP warm-up, reputation, DKIM/SPF/DMARC alignment, and ARC sealing — exactly the
parts AGENTS.md classifies as an npm/managed concern (`@zeroship/email` "calls fetch"), not a
native kernel surface. Owning a bespoke inbound MX means owning all of that, which is out of
proportion to this spec. <!-- Added in round 1: addressing MAJOR — prefer managed inbound provider over bespoke MX -->

**The design uses a managed inbound-email provider** (SES inbound rule → SNS/HTTPS, **or** Postmark
inbound, **or** Mailgun routes) that POSTs parsed inbound messages to a **thin compio HTTPS
handler** (`crates/control` or a small `crates/relay` handler — no SMTP server, no IP warm-up):

- The handler looks up `alias → real inbox` via `auth.app_user_identities` (active only).
- If active, it forwards via the **existing outbound path** (`crates/auth/src/mailer` /
  the managed provider's send API), rewriting `From:` to `{app-name} via relay <alias>` and
  `Reply-To:` to the alias (replies re-enter the relay — two-way routing is v2). The provider
  handles DKIM/SPF/DMARC alignment and ARC for the `{relay_domain}` sending identity.
- **v1 scope = app → user forwarding only.** Inbound user replies are bounced with a "replies not
  yet supported" message (or silently dropped behind a flag) until v2.

> **Decision (managed inbound vs bespoke MX — O5, resolved).** A managed inbound provider + thin
> compio webhook handler is the **default**; a from-scratch zero-tokio receiving MX
> (`crates/relay` SMTP server) is deferred and only built if the managed route is rejected with
> concrete reasons (e.g. data-residency). This keeps the relay a "driver" (calls fetch / a provider
> API), consistent with the native-primitive invariant, and avoids owning IP warm-up.

### 7.3 Dev / test relay topology (so the e2e is actually faithful)

⚠️ **The dev/compose stack runs on `*.zeroship.localhost`; `relay.zeroship.ai` has no analog
there.** To make the §8.6 e2e ("a send to the alias forwards to the real inbox") **runnable**, the
dev topology is defined here: <!-- Added in round 1: addressing MAJOR — define dev/test relay topology -->

- **Relay domain in dev** = `relay.zeroship.localhost` (env-configurable `RELAY_DOMAIN`,
  prod `relay.zeroship.ai`).
- **A local MX sink container** (e.g. `mailpit` or `inbucket`) in `docker-compose` accepts mail for
  `*.zeroship.localhost` and exposes an HTTP API to assert delivery.
- The inbound provider is **simulated in dev** by a tiny webhook injector: the test posts a
  synthetic inbound message (the same JSON shape the managed provider would POST) to the relay
  handler, which forwards to the **mailpit sink** standing in for the "real inbox." The e2e asserts
  the message landed in the sink addressed to the mapped inbox with the rewritten `From:`/`Reply-To:`.
- This exercises the **real handler + real lookup + real forward**, with only the provider's
  receiving MX simulated — consistent with the faithful-e2e mandate (the externally-operated MX is
  the one piece we cannot stand up in compose).

### 7.4 Abuse / deliverability

- **Bounce handling**: forwarding bounces (provider webhook) feed `auth.email_suppressions`
  (existing table); a suppressed real inbox disables forwarding for that user across all aliases.
- **Loop protection**: drop messages already carrying our `X-ZS-Relay:` header; per-alias rate
  limit; max hop count.
- **Abuse**: per-app and per-alias volume caps; an alias forwarding to a suppressed/complained
  inbox is auto-revoked.
- **Deliverability**: the managed provider owns IP warm-up, SPF/DKIM/DMARC for `{relay_domain}`,
  and ARC. We publish the DNS records the provider requires; warm-up is the provider's concern.

### 7.5 Lifecycle / revocation

Revoking a grant (deleting the `control.oauth_grants` row for `(subject, client_id)` — the single
ledger, round-3) cascades: set `app_user_identities.revoked_at = now()`, which disables the relay
alias (subsequent inbound mail to the alias bounces) and frees its partial-unique slot. Re-granting
**reuses the deterministic `pws_` sub** (the row is upserted, `revoked_at` cleared — §6.4) but mints a
**fresh relay alias** (Apple Hide-My-Email applies to the *alias*, not the sub). App delete cascades
to revoke all the app's aliases and pairwise rows, and to delete the `control.oauth_grants` rows for
that `client_id`. <!-- Added in round 2: addressing MINOR — fresh ALIAS on re-grant, deterministic sub reused (reconciles §6.4 / DDL PK); Updated in round 3: revocation keys on control.oauth_grants, the single ledger -->

## 8. Cross-cutting — data model, error model, security, testing

### 8.1 Data model (SQL DDL sketches)

`control.app_oauth_clients` (control plane bookkeeping):

```sql
CREATE TABLE control.app_oauth_clients (
  app_id              uuid PRIMARY KEY REFERENCES control.apps(id) ON DELETE CASCADE,
  hydra_client_id     text NOT NULL UNIQUE,          -- app_<base62>
  sector_identifier   text NOT NULL,                 -- apex origin or sector_identifier_uri
  redirect_uris       text[] NOT NULL DEFAULT '{}',
  scope               text NOT NULL,                 -- mirrored allowlist
  created_at          timestamptz NOT NULL DEFAULT now(),
  updated_at          timestamptz NOT NULL DEFAULT now()
);
```

`control.app_scope_defs` (declared scope registry):

```sql
CREATE TABLE control.app_scope_defs (
  app_id       uuid    NOT NULL REFERENCES control.apps(id) ON DELETE CASCADE,
  scope_id     text    NOT NULL,                     -- read:billing
  label        text    NOT NULL,
  description  text,
  PRIMARY KEY (app_id, scope_id)
);
```

`auth.app_user_identities` (pairwise + relay mapping). ⚠️ **`app_id` is `TEXT`** to match the
existing `auth.gateway_sessions.app_id TEXT` (the auth schema keys app_id as text throughout — the
gateway store binds it as a string). The redundant `pairwise_sub` column is **dropped**: the
typed-id `id` (a `pws_…` value) **is** the pairwise sub — one value, one column, no consistency
hazard. `relay_email` uniqueness is **partial** (active aliases only) so revoked aliases free their
slot. <!-- Added in round 1: addressing MAJOR — app_id TEXT consistency, drop redundant pairwise_sub, partial relay_email UNIQUE -->

```sql
CREATE TABLE auth.app_user_identities (
  id              text        PRIMARY KEY,           -- pws_… == derive_pairwise(global_user, app.sector); DETERMINISTIC, one row per (app, global_user)
  app_id          text        NOT NULL,              -- TEXT to match auth.gateway_sessions.app_id
  global_user_id  uuid        NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,  -- UUID like auth.users.id
  relay_email     text,                              -- {token}@{relay_domain}; NULL until email scope granted
  created_at      timestamptz NOT NULL DEFAULT now(),
  revoked_at      timestamptz,
  UNIQUE (app_id, global_user_id)
);
-- pws_ is DETERMINISTIC (§6.2/§6.4): re-grant UPSERTs the same row (clears revoked_at) rather than
-- inserting a colliding second pws_, so `id` being the PK is correct — there is exactly one row per
-- (app, global_user). No "fresh sub on re-grant"; only the relay alias rotates.
-- relay alias unique only among ACTIVE aliases → a revoked alias frees its slot for the fresh alias
CREATE UNIQUE INDEX app_user_identities_relay_active
  ON auth.app_user_identities (relay_email) WHERE relay_email IS NOT NULL AND revoked_at IS NULL;
```

> Note: `global_user_id` is `uuid` here to match `auth.users.id` (UUID); `app_id` is `text` to
> match `auth.gateway_sessions.app_id`. These two are intentionally different types because they
> reference two differently-typed columns — both pinned to their referent, no guessing.

⚠️ **Round-3: there is NO `auth.app_grants` table.** The per-app remembered-grant ledger is the
**existing `control.oauth_grants`** keyed by `(subject, client_id)` (the live consent fast path's
store, §5.2); `client_id = app_<app_id>` is 1:1 with `(global_user, app)`, so no second grant table
is created. This removes the two-ledger consistency hazard round 2 introduced. <!-- Added in round 3: addressing MAJOR — drop the parallel auth.app_grants table; control.oauth_grants is the single grant ledger -->

`auth.app_session_anchors` (the **dedicated** SDK reload-recovery anchor — round-3 BLOCKER fix; a
SEPARATE credential from the 12h/30-min interactive `auth.gateway_sessions`, §2/§8.3):

```sql
CREATE TABLE auth.app_session_anchors (
  id                  uuid        PRIMARY KEY DEFAULT gen_random_uuid(),  -- the __Host-zs_app_session cookie value
  app_id              text        NOT NULL,            -- TEXT, consistent with gateway_sessions/app_user_identities
  global_user_id      uuid        NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,  -- the GLOBAL user (pws_ is a projection, never stored)
  refresh_token_enc   bytea       NOT NULL,            -- encrypted server-held rotating refresh family
  refresh_family_id   text        NOT NULL,            -- Hydra session/lineage id binding the anchor to its family (§1.2)
  cached_access_token text,                            -- last minted WRAPPER access token (shared by concurrent ?mint=1, §1.2)
  cached_access_exp   timestamptz,                     -- expiry of cached_access_token (= 10-min access TTL)
  granted_scopes      text[]      NOT NULL DEFAULT '{}',
  created_at          timestamptz NOT NULL DEFAULT now(),
  -- NO idle column: a reload-recovery anchor MUST survive long idle gaps (§2 BLOCKER fix).
  abs_expires_at      timestamptz NOT NULL,            -- = min(created_at + 30d, refresh-family ceiling); NO 30-min idle slide
  revoked_at          timestamptz
);
CREATE INDEX app_session_anchors_user ON auth.app_session_anchors (app_id, global_user_id) WHERE revoked_at IS NULL;
-- The anchor does NOT reuse auth.gateway_sessions (ABSOLUTE_HOURS=12 / IDLE_MINUTES=30, sessions.rs:46-50),
-- whose 30-min idle expiry would kill "return tomorrow" recovery. This store has no idle window.
-- /__zs/auth/session?mint=1 takes pg_advisory_xact_lock(hash(id)) ACROSS the Hydra refresh (§1.2 BLOCKER fix).
```

`auth.token_revocations` (cross-node access-token revocation — round-3 MAJOR fix; the
**family-marker** PRIMARY mechanism, backed by shared PG so it works under `--scale`/multi-gate,
§1.3/§8.5):

```sql
CREATE TABLE auth.token_revocations (
  client_id      text        NOT NULL,                 -- app_<base62>
  sub            text        NOT NULL,                 -- pws_ (wrapper) or global UUID (raw-Hydra) — whichever the token carries
  revoked_after  timestamptz NOT NULL,                 -- reject any token with iat < revoked_after for this (client_id, sub)
  PRIMARY KEY (client_id, sub)
);
-- The Bearer arm rejects a token when a row exists with revoked_after > token.iat.
-- Read-through cached locally (short TTL) like the auth.dpop_jti tier; one upsert on signout/family-revoke.
-- Per-jti admin revokes use a PG-backed TieredJtiCache (NOT the in-memory logout_jti_cache).
```

⚠️ **`auth.gateway_sessions` is UNCHANGED.** The interactive cookie redirect flow keeps it with its
`ABSOLUTE_HOURS=12` / `IDLE_MINUTES=30` semantics (`sessions.rs:46-50`) and its UUID `user_id`; the
SDK anchor lives in the separate `app_session_anchors` table above, so no `sessions.rs` constant or
`validate()`-slide test changes. The `pws_…` is **never stored** in either table — it is the
deterministic projection derived on the way out into `ZeroShip-User.id` (§6.2). All DDL authored as
Liquibase changesets under `db/changelog/` (per the auth.md data-model note — migrations are
Liquibase, no inline SQL). <!-- Added in round 3: addressing BLOCKER — anchor is a dedicated table with no idle slide; gateway_sessions untouched; +token_revocations cross-node table -->

The `auth.app_user_identities` row (§8.1) is the **only** place the `pws_↔(app, global_user)`
mapping is persisted (for the relay reverse-lookup and support tooling); the gateway upserts it on
the cookie/Bearer projection and reads it for the relay alias.

### 8.2 Token claim shapes

Two access-token shapes (round-3): the **gateway wrapper** the browser holds, and the **raw Hydra
access JWT** kept server-side (and used directly only by non-browser OAuth clients).

**Gateway WRAPPER access token** (what the browser holds — `wrapper_token::WrapperClaims`, ed25519,
10-min `exp`, §1.2). Its `sub` is the `pws_`, so app JS decoding it learns **no** global UUID:

```json
{ "iss":"<gateway issuer>", "sub":"pws_…",            // ← per-app pairwise sub; NO global UUID in the browser-held token
  "aud":"myapp.zeroship.ai",                 // the app host (wrapper aud)
  "client_id":"app_<base62>",                // ← the Bearer arm binds on THIS
  "email":"{token}@{relay_domain}",          // relay alias, not the real inbox
  "cnf":{ "jkt":"…" }?,                      // present only with useDpop (sender-constraint)
  "exp":…, "iat":…, "jti":"…",               // exp = iat + 10min; revocation via the (client_id,sub) family marker (§8.5)
  "scope":"openid profile email read:billing" }
```

**Raw Hydra access JWT** (RFC 9068, EdDSA, kept server-side under the anchor; used directly only by
non-browser clients like the CLI). ⚠️ The Bearer arm binds on the **`client_id` claim**, not `aud`
(`aud` is the resource server, §1.3):

```json
{ "iss":"https://auth.zeroship.ai/", "sub":"<global usr_ UUID>",   // ← Hydra issues the GLOBAL UUID (bound at accept_login)
  "aud":"http://api.zeroship.ai",            // resource-server audience (NOT the per-app client)
  "client_id":"app_<base62>",                // ← the Bearer arm binds on THIS
  "exp":…, "iat":…, "jti":"…",
  "scope":"openid profile email read:billing" }
```

⚠️ **The raw Hydra JWT `sub` is the global UUID** (Hydra bound it at `accept_login`); it **never
reaches the browser** (the browser gets the wrapper). On the raw-Hydra Bearer path (non-browser
clients) the gateway projects `sub → derive_pairwise(sub, route.sector_identifier)` only when
synthesizing the `ZeroShip-User` header; the token itself is never rewritten. <!-- Added in round 2: addressing BLOCKER — JWT sub is the global UUID; Updated in round 3: browser holds the wrapper (sub=pws_), raw Hydra token stays server-side -->

`ZeroShip-User` JSON body (signed per `encode_user_header`):

```json
{ "id":"pws_…", "email":"{token}@{relay_domain}", "name":"…",
  "avatar":null, "email_verified":true, "scopes":["openid","profile","read:billing"] }
```

(`WorkerUser` gains `scopes` — a permanent kernel-contract change; the full consumer list is in
§1.4. `id` is an opaque `pws_…` text id, never a UUID, §6.3.)

### 8.3 Cookies & headers

| Name | Set by | Attributes | Purpose |
|---|---|---|---|
| `__Host-zs_app_session` | gateway `/__zs/auth/token` | `HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=2592000` (30d) | First-party anchor; server-held refresh family for reload-recovery. **`SameSite=Strict`** (round-2): never needed cross-site, so a top-level navigation cannot ride it. Name via `app_session_cookie_name(insecure_dev)` → `zs_app_session` (no `__Host-`, no `Secure`) in dev. |
| `zs.<app_ref>.is.authenticated` | SDK (browser) | non-HttpOnly; `Secure; SameSite=Lax; Path=/; Max-Age=2592000` (30d, matches anchor) | Session-presence breadcrumb; gates `checkSession`. Keyed on the gateway-returned `app_ref` (§4.3), not the raw host. |
| `__Host-zs_oidc_stash` | gateway (legacy redirect only) | unchanged | Not used by the popup/PKCE-in-browser flow. |

⚠️ **Lifetime reconciliation (round-3 — the anchor is a SEPARATE store, NOT the 12h `gateway_sessions`).**
The anchor `Max-Age`, the breadcrumb `Max-Age`, and the **`auth.app_session_anchors.abs_expires_at`**
are **all 30 days** (NOT `auth.gateway_sessions`, which stays at its `ABSOLUTE_HOURS=12` /
`IDLE_MINUTES=30` for the interactive cookie redirect flow — §8.1). The anchor has **no idle window**
(a reload-recovery credential must survive long idle gaps), so it does not die after a 30-min gap.
The browser **access token** is the **10-min** wrapper (§8.5; refreshed via `/__zs/auth/session?mint=1`
under the anchor lock); the **server-held refresh family** is `720h` (Hydra), and the anchor's
absolute expiry is `min(created_at + 30d, refresh-family ceiling)` — each active `?mint=1` slides the
720h family, so an actively-used anchor practically caps at 30 days of total inactivity. When the
anchor expires (or Hydra returns `invalid_grant` at the 720h ceiling), recovery falls back to
interactive login. `Max-Age` values are documented, not derived per-handler. <!-- Added in round 1: addressing MINOR — lifetime reconciliation; Updated in round 3: anchor is app_session_anchors (no idle slide), gateway_sessions stays 12h/30min; access token is 10-min wrapper -->

⚠️ The `__Host-zs_app_session` cookie attribute table row (above) is the **anchor cookie pointing at
an `auth.app_session_anchors` row**, not a `gateway_sessions` row — corrected in round 3 so the
30-day `Max-Age` points at a store that actually lives 30 days.

⚠️ **`__Host-`/insecure-dev naming is centralized.** All five new handlers (`/token`, `/session`,
`/signout`, plus the cookie-clearing on revoke and the anchor validation) use the existing
`app_session_cookie_name(insecure_dev)` helper (the `APP_SESSION_COOKIE_{PROD,DEV}` constants in
`oidc_rp.rs`) rather than re-deriving the name; the `__Host-` prefix + `Secure` drop together in
dev exactly as the existing callback path does. <!-- Added in round 1: addressing MINOR — centralize cookie-name helper across handlers -->

Headers: `ZeroShip-User` (existing, +scopes); `Authorization: Bearer <jwt>` (new gateway Bearer
arm, issuer-discriminated, §1.3); `Authorization: DPoP <wrapper>` (existing hardening path). ⚠️
**Round-3: `/__zs/auth/token` and `/__zs/auth/session` are same-origin-only and emit NO CORS
allow-origin/allow-credentials headers** — the boundary is the custom-header (`X-ZS-Auth`) + exact
`Origin`-match + `Sec-Fetch-Site` conjunction (§1.2). A foreign, `null`, or (on POST) missing `Origin`
is rejected. No credentialed origin reflection.

### 8.4 Error model

Typed codes (Supabase/Auth0-shaped), surfaced as `{ "error": "<code>", "error_description"?,
"scope"? }` on the wire and `AuthError{code}` in the SDK:

| Code | Where | Meaning |
|---|---|---|
| `login_required` | `/session` (anchor missing/expired) | No recoverable session; interactive popup login needed. |
| `consent_required` | `/session`, step-up | New scopes need consent; open popup `prompt=consent`. |
| `invalid_grant` | `/token` | Code/refresh invalid, expired, or reuse-detected (family revoked). |
| `popup_closed` | SDK | User closed the popup before completion. |
| `popup_blocked` | SDK | `window.open` returned null. |
| `timeout` | SDK | Popup exceeded 60s. |
| `invalid_state` | SDK | Relay `state` didn't match an in-flight flow (CSRF guard). |
| `scope_required` | gateway Bearer arm / route policy | Token lacks a required scope (HTTP 403). |
| `network_error` / `server_error` / `config_error` | SDK | Transport / 5xx / misconfiguration (incl. cross-origin-iframe embedding, §1.2). |

### 8.5 Security

**Trust model — the creator app's own JS is in the token boundary (stated explicitly).** On
`{app}.zeroship.ai` the app is **arbitrary, AI-generated, first-party code**. With client-held
sessions (locked decision 1), that code is **fully trusted with its own user's tokens** — this is
inherent to any client-held-session model (Supabase/Auth0 included) and is **not** something the
platform can isolate away while keeping client tokens. An XSS bug in a creator app fully
compromises **that app's** session for the affected user. The platform's mitigations are: <!-- Added in round 1: addressing MAJOR — state the creator-app trust model explicitly -->

1. **Blast radius is one app, by design.** Per-app **pairwise sub** + per-app **relay email** mean a
   token stolen from app A reveals only app A's `pws_…` and alias — never the global user id, the
   real email, or any other app's identity. ⚠️ **Round-3 makes this true even of the raw access
   token**: the browser holds the gateway **wrapper** (sub=`pws_`, email=alias), not the raw Hydra
   JWT (global UUID), so even base64-decoding the stolen token reveals only the per-app pairwise id
   (§1.2). **This is a feature**: the privacy primitives double as a compromise-containment boundary.
   A stolen app-A token cannot even be replayed at app B (Bearer arm binds on per-app `client_id`,
   §1.3).
2. **Memory-default cache** (no token at rest) + **optional Web Worker** isolation of the browser
   refresh family (`browser_refresh` mode) — tokens are not in a JS-readable cookie (only the
   boolean breadcrumb is) and not in `localStorage` unless the creator opts in (documented
   tradeoff).
3. **The anchor is HttpOnly + family-bound.** XSS cannot read the `__Host-zs_app_session` anchor,
   and a stolen browser refresh token alone cannot resurrect the anchor (it is bound to the
   access-token family / Hydra session lineage, §1.2). In the default `server_anchor` mode the
   browser holds **no** refresh token at all, so the highest-value credential never reaches
   app-readable memory.
4. **Same-origin-only enforcement** on `/__zs/auth/token` and `/__zs/auth/session` (custom
   `X-ZS-Auth` header + `Origin` exact-match == app origin + `Sec-Fetch-Site: same-origin`, with NO
   credentialed CORS reflection — round-3) — a cross-site page cannot drive the token endpoint even
   if it guesses a code, and a reflection bug cannot turn it into a credentialed mint oracle.

This is **materially weaker** than an architecture where tokens never co-reside with arbitrary
first-party server code, and we say so plainly — it is the accepted cost of true Supabase parity
(locked decision 1), bounded to one app by the pairwise/relay design.

- **TTLs**: browser **wrapper** access token **10 min** (round-3 — sub=`pws_`; bounds the
  plain-Bearer replay window); server refresh family 720h sliding (Hydra); browser refresh family
  (opt-in) 720h; code 60s; **anchor 30d** (`auth.app_session_anchors`, no idle slide, §8.1);
  **breadcrumb 30d** (reconciled — §8.3); `token_revocations` family marker persists until swept
  (≥ max access TTL); per-jti denylist entry = 10 min.
- **PKCE S256** mandatory (Hydra `enforced_for_public_clients`); the verifier is persisted in
  **`sessionStorage`** only for the duration of the in-flight flow (round-3 — so an opener reload
  mid-popup does not strand it, §4.3) and is **cleared on completion, `state` mismatch, or the 60s
  timeout**. `sessionStorage` is per-tab/same-origin and the verifier is sent only to the same-origin
  `/token` proxy; it never crosses an origin and never outlives the flow.
- **Single-holder rotation** (§2): exactly one party rotates a given refresh family; Hydra
  reuse-detection (single-use, 30s grace, family-revoke on replay) never fires on a
  browser-vs-server race because the families are disjoint; `navigator.locks` serializes the
  browser family across tabs.
- **Same-origin-only, NO CORS reflection (round-3).** `/__zs/auth/token` and `/__zs/auth/session`
  emit **no** `Access-Control-Allow-Origin` / `allow-credentials` at all — they are same-origin
  endpoints, so a cross-origin request is simply blocked by the browser (no allow-origin) and a
  foreign / `null` / (POST) missing `Origin` is rejected server-side by exact-string compare.
  Credentialed origin-reflection (the round-2 posture) is **removed** because reflection +
  `allow-credentials` is a credentialed token-mint oracle if the match logic ever has a bug. The
  anchor cookie is `SameSite=Strict` and rides the legitimate same-origin fetch without any CORS
  credentials handshake. <!-- Added in round 3: addressing MAJOR — drop credentialed CORS reflection; same-origin-only -->
- **CSRF**: the token/session proxies do **not** rely on `SameSite` for CSRF. The defense is the
  conjunction of (1) a custom non-simple header `X-ZS-Auth` the SDK always sets (a cross-site page
  cannot set it without a preflight, and since these endpoints emit no allow-origin the preflight
  fails outright), (2) `Origin` exact-match == app origin (mandatory when present; `null`/foreign
  rejected), and (3) `Sec-Fetch-Site: same-origin` (enforced when present). `?mint=1` additionally
  **requires** `X-ZS-Auth` so a top-level navigation cannot mint (§1.2). The anchor cookie is
  **`SameSite=Strict`** (never needed cross-site). `state` is validated on the relay (replay/CSRF
  guard on the code).
- **Test (round-3):** a cross-origin `fetch` with a foreign `Origin` is rejected (no allow-origin,
  server-side 403); `Origin: null` cannot mint; a same-origin call with `X-ZS-Auth` succeeds.
- **`Sec-Fetch-Site` browser matrix**: `Sec-Fetch-*` is sent by Chromium ≥ 76, Firefox ≥ 90, and
  Safari ≥ 16.4. On older Safari/Firefox the header is **absent**; the custom-header + `Origin`
  checks carry the CSRF defense there, so its absence does **not** weaken the endpoint (it is
  enforced *when present*, advisory when absent). Documented so the fallback posture is explicit.
- **Access-token replay window (quantified, round-3 cross-node)**: plain browser Bearer arm uses a
  **10-minute** access-token TTL (on the per-app **wrapper** token, §1.2), so an XSS-exfiltrated
  access token is replayable for ≤10 min against the app's own host. Local verification (wrapper via
  `state.wrapper_verifier`, raw-Hydra via JWKS) means a revoked-but-unexpired token would otherwise
  still validate; the **cross-node revocation denylist** — a shared-PG `auth.token_revocations`
  `(client_id, sub, revoked_after)` **family marker** (the PRIMARY mechanism, since signout cannot
  enumerate per-node jtis), with a PG-backed per-jti `TieredJtiCache` for single-token admin revokes
  — closes this to **cross-node immediate for the family case** (one PG upsert, visible to every gate
  node on its next read-through, bounded by a seconds-scale cache TTL) and the residual window for a
  token not yet covered by a marker is the 10-min TTL. ⚠️ This is **not** backed by the in-memory
  `logout_jti_cache` (per-process only) — it is the PG-tiered store used by `dpop_jti`, so it is
  sound under `--scale` and multiple gate nodes (§1.3 step 2). DPoP binding (one flag, `useDpop`)
  upgrades to per-request sender-constraint and is the documented recommendation for sensitive-scope
  apps. <!-- Added in round 2: addressing MAJOR — Sec-Fetch-Site matrix, quantified replay window; Updated in round 3: cross-node family-marker denylist backed by shared PG, not the in-memory logout cache; MINOR — SameSite=Strict anchor -->
- **`__Host-` attributes** on the anchor: `Path=/`, no `Domain`, `Secure`, **`SameSite=Strict`**
  (dropped only in insecure_dev via `app_session_cookie_name`, §8.3).
- **Per-app `client_id` binding** on the Bearer arm prevents cross-app token replay; the
  issuer-discriminator keeps the reserved API-key Bearer path uncollided (§1.3); a `None`
  `oauth_client_id` hard-fails rather than binding to a falsy value (§1.5).
- **Plain-Bearer replay caveat**: no sender-constraint; mitigated by **10-min TTL** (on the wrapper),
  HTTPS, `client_id` binding, and the **cross-node `(client_id, sub)` revocation family marker**
  (§8.5); **DPoP-binding is the documented recommendation** for sensitive-scope apps (the wrapper
  path already exists — `useDpop` adds `cnf.jkt`).
- **Relay abuse**: alias rate limits, loop/hop detection, suppression-list integration,
  auto-revoke on complaint (managed-provider path, §7).
- **Pairwise salt** custody = same secret store as the worker key; rotation is a deliberate
  global break-glass.

### 8.6 Testing strategy (faithful e2e — no shims)

- **e2e (the gate)**: a real example app under `tests/` (or `apps/example-auth/`) driving the
  whole path against a **real Hydra + gateway + worker + auth UI**:
  1. popup login (headless browser drives `window.open` → relay → `exchangeCodeForSession`),
  2. consent screen rendering the app's **declared scopes**,
  3. the **`ZeroShip-User.id` is a per-app `pws_…`** (assert two apps get different ids for the same
     user; the **browser-held access token is the wrapper whose `sub` is the `pws_`** — assert it
     contains **no** global UUID, §1.2 round-3 — while the server-held raw Hydra `sub` is the global
     UUID),
  4. **end user can grant** a declared scope: an ordinary `usr_…` with no platform policy completes
     consent for an app declaring `read:billing` and the grant succeeds (would render `CANNOT_GRANT`
     pre-fix — the consent-inversion regression, §5.2); the grant is recorded in the **single**
     `control.oauth_grants` ledger and a re-login with the same scopes shows **no** consent prompt (no
     loop — round-3 §5.2),
  5. `env.auth.getUser()` in the worker returns the user with `scopes` — **driven through the WORKER
     runtime path** (`crates/worker` `create_plugins()` → real dispatcher → real `ZeroShip-User`
     header), NOT `zeroship serve`, since only the worker path is the production runtime (round-3
     §1.4),
  6. a route requiring `read:billing` returns 403 without the scope and 200 with it; an **expired
     wrapper Bearer on an `Anon` (public) route still serves the public page** (round-3 §1.3),
  7. **reload-recovery** survives a page reload via the first-party anchor (`/__zs/auth/session`),
     including **after a >30-min idle gap** (the dedicated `auth.app_session_anchors` store, NOT the
     30-min-idle `gateway_sessions` — round-3 §2/§8.1), with **no** hidden iframe and **no**
     third-party cookie; **N concurrent `?mint=1` make exactly one Hydra refresh** and do not revoke
     the family (round-3 §1.2),
  8. **relay alias** (in the relay sub-spec's e2e): the email claim is `…@{relay_domain}`, and a
     simulated inbound to the alias forwards to the mapped inbox in the **local MX sink**
     (mailpit/inbucket; §7.3) — the provider's receiving MX is the only simulated piece.
  This mirrors the project mandate that e2e exercises the real runtime + dispatcher + example RPCs,
  not shims (the auto-tx faithful-e2e lesson).
- **Per-behavior regression tests** (each would fail pre-fix):
  - gateway: Bearer arm accepts a valid **wrapper** (sub=`pws_`) / accepts a valid raw-Hydra token
    (non-browser) / rejects wrong-`client_id` / rejects expired / rejects bad sig / rejects
    non-recognized-`iss` as the reserved API-key path / 403 on missing scope / **on an `Anon` route an
    expired/invalid Bearer falls through to anonymous and serves the public page; on a `User` route it
    401s** (round-3 §1.3) / **rejects every token when `route.oauth_client_id` is `None`** (never binds
    to empty) / **rejects a token whose `(client_id, sub)` family was revoked after its `iat`**
    (cross-node family marker) (round-3, §1.3/§1.5/§8.5).
  - gateway: the **access token returned to the browser contains NO global `usr_` UUID** (it is the
    wrapper, `sub` = `pws_`) on both `/token` and `/session?mint=1` (round-3 §1.2/G4).
  - gateway: `/__zs/auth/token` **validates the id_token** (sig/iss/aud/exp — the load-bearing
    binding; nonce is defense-in-depth) and returns a server-validated `user`; a forged/altered
    id_token in the Hydra response ⇒ `400 invalid_token` (round-2/3 §1.2). `?mint=1` **without
    `X-ZS-Auth`** ⇒ `400` (cannot mint via top-level nav). A cross-origin `fetch` with a foreign or
    `null` `Origin` is **rejected** (no credentialed CORS reflection — round-3 §1.2/§8.5).
  - gateway: **N parallel `?mint=1`** against one anchor ⇒ exactly **one** Hydra `/oauth2/token` call,
    all N get a valid token, family not revoked (round-3 §1.2); a mint after the cached token expires
    triggers exactly one new rotation.
  - gateway: `/__zs/auth/signout` clears anchor + breadcrumb (fixes the live bug — a test that
    404s today) **and writes the `auth.token_revocations` family marker**. `global` revokes only this
    app's `(app_id, user)` `app_session_anchors` rows + marker, not other apps; a token minted before
    the marker is rejected on a **second** gate node (cross-node — round-3 §8.5).
  - gateway: per-app BCL revokes only the matching app's sessions (disambiguated by `logout_token`
    `aud` = per-app `client_id`).
  - gateway: **`/popup-callback` reflects no query param into the body** (CSP-nonce no-reflection,
    round-2 §1.2) and emits the same-origin **BroadcastChannel/localStorage relay** alongside
    `postMessage` (round-3 §4.4).
  - core/gateway: `RouteEntry` round-trips `oauth_client_id: Option`+`sector_identifier: Option`,
    **`None` round-trips and is rejected by the Bearer arm** (round-2 wire-format fixture); a deployed
    app's `RouteEntry` always carries `Some(oauth_client_id)` (provision-ordering invariant, round-3
    §1.5).
  - consent: ordinary end user CAN grant `read:billing` (namespace b) **via the POST accept path**;
    non-privileged user CANNOT grant `apps:deploy` (namespace a) via POST; an `Unknown` scope is
    **rejected `invalid_scope`** on both render and accept (round-2 §5.2); **grant `{a}` then request
    `{a,b}` ⇒ exactly one prompt for `{b}` and no loop** (single `control.oauth_grants` ledger,
    per-app client `skip_consent = false`, round-3 §5.2); `scope_views` renders an app-declared scope
    **recognized** (updated `renders_scope_labels`, round-3 §5.2).
  - control: a manifest declaring `billing:read` (closed-vocabulary collision) is **rejected** by
    the scope-def validator; `read:billing` is accepted (round-2 §5.1); `app_scope_defs` + Hydra
    client `scope` allowlist update **in one transaction** so a deploy with a just-declared scope is
    never `invalid_scope`-bricked (round-3 §5.2).
  - runtime: `env.auth.getUser()` resolves **with the plugin registered in the WORKER's
    `create_plugins()`** (round-3 §1.4); `undefined` without it; `.scopes` present; an explicit test
    asserts `AuthPlugin` is in the worker plugin vector (not just the CLI vector).
  - control: `ensure_app_client` is idempotent; `sync_app_redirect_uris` adds/removes URIs and the
    full set is **diff-then-PUT** (no-op deploy ⇒ no PUT) within the per-app cap (round-3 §1.1).
  - pairwise (**round-3 projection**): cookie arm projects `gateway_sessions.user_id` (global UUID)
    → `pws_`; raw-Hydra Bearer arm projects the JWT `sub` (global UUID) → the **same** `pws_`; the
    **wrapper** Bearer arm reads `pws_` straight from the wrapper `sub`; all three agree for the same
    `(user, app.sector)`; two apps → two distinct `pws_`; stable across re-login; the Hydra/cookie/raw
    JWT `sub` stays the **global UUID** (no UUID-parsing consumer breaks).
  - relay: alias substitution in `ZeroShip-User.email` **and** the id_token `email` claim (never the
    real address); partial-unique allows re-issue after revoke; generate-and-retry on active
    collision; **fresh alias but reused deterministic `pws_` on re-grant** (round-2 §6.4); **alias
    created at consent time (off the hot path); two concurrent first-consents mint exactly one
    alias** (round-3 §7.1); revoked on grant revoke; bounce → suppression.
- **SDK unit coverage** (`node --import tsx --test`): CacheManager keying on `app_ref` + expiry +
  refresh-aware eviction; **profile sourced from the gateway `user` field, never a decoded id_token**;
  **the PKCE transaction (verifier/state/nonce/redirect_uri) is persisted in `sessionStorage` and
  recovered after an opener reload mid-flow** so the exchange still succeeds (round-3 §4.3/§4.4);
  lock serialization (two concurrent `getAccessToken` → one network call); rotation `updateEntry`;
  **breadcrumb is non-authoritative — a cleared breadcrumb still recovers via the unconditional first
  probe; a forged breadcrumb adds at most one probe**; popup timeout + `popup_closed`; **the
  BroadcastChannel/localStorage relay completes the exchange when `window.opener` is `null` at relay
  time, and a spurious early `popup.closed` does not cancel a flow whose code already arrived**
  (round-3 COOP §4.4); relay origin/type/state validation + distinct origin-mismatch error;
  cross-origin-iframe `config_error`; PKCE S256 vectors; `AuthError` mapping.
- `lint:pkg = publint && attw --pack .` green for all four subpaths; `pnpm build` before
  `cargo build` (bootstrap/runtime ordering invariant).

### 8.7 Operations — rate-limiting, metrics, alerting on the new proxy endpoints

⚠️ **Round-2 addition (MINOR).** The new gateway endpoints proxy to Hydra and mint tokens, so they
need production observability and abuse limits — absent from round 1. They reuse the gateway's
**existing** rate-limit infra (the CHWBL/per-tenant limiter, AGENTS.md), not a new mechanism. <!-- Added in round 2: addressing MINOR — operational rate-limit/metrics/alerting for the token/session proxy -->

- **Rate limits** (per-IP **and** per-app, via the existing limiter):
  - `POST /__zs/auth/token` — code exchange + refresh; cap bounds Hydra `/oauth2/token` load and
    blocks mint loops. On exceed: `429` with `Retry-After`.
  - `GET /__zs/auth/session?mint=1` — minting; per-IP + per-app cap (tighter than `/token` since a
    valid anchor can mint repeatedly).
  - `POST /__zs/auth/signout`, `GET /__zs/auth/authorize` — looser caps (cheap), still bounded.
- **Hydra back-pressure**: when `/oauth2/token` is slow or 5xx-ing, the proxy applies a bounded
  timeout + circuit-breaker and returns `503 upstream_unavailable` (not a hang), so a Hydra brownout
  cannot exhaust gateway connections.
- **Metrics** (emit on the existing gateway metrics path):
  - `auth_token_mint_total{result=success|invalid_grant|upstream_error}`,
  - `auth_session_mint_total{result}`,
  - `auth_reuse_detection_revocations_total` (a spike ⇒ attack or a rotation bug),
  - `auth_anchor_validation_failures_total`,
  - `auth_bearer_reject_total{reason=wrong_client_id|expired|bad_sig|denylisted|not_user_session}`,
  - `auth_relay_bounce_total`, `auth_client_reconcile_errors_total` (per-app client provisioning).
- **Alerts**: page on `auth_reuse_detection_revocations_total` rate spike (rotation regression),
  `auth_client_reconcile_errors_total > 0` (apps unable to provision clients ⇒ broken login),
  sustained `upstream_error` ratio on `/token` (Hydra health), and an anchor-validation-failure
  spike (anchor-store or signing-key drift).

## 9. Phased implementation plan (reviewable slices)

Each slice is independently testable and committable (commit-only, never push). Build order
1 → 2 → 3 → 4 → 5; the consent *mechanism* in Slice 1.3 lets 3/4/5 swap content in without
reshaping the SDK.

**Slice 1 — foundation (Subsystem 1).** Reviewable sub-slices:
- 1a. `env.auth` `AuthPlugin` registration **in BOTH `crates/worker/src/cache.rs` `create_plugins()`
  (the production path) AND the CLI `zeroship serve` vector** (round-3 §1.4) + a runtime regression
  test driven through the **worker** path (smallest, unblocks server SDK).
- 1b. **Wire-format first (§1.5):** extend `RouteEntry` with `oauth_client_id`+`sector_identifier`,
  populate from control's route-sync, surface on `CompiledRoute`; update every producer/consumer/
  fixture in the same patch (a deliberate wire-format break). Then gateway `browser_auth.rs`:
  `/authorize`, `/popup-callback` (with BroadcastChannel/localStorage relay fallback), `/token`
  (mint per-app **wrapper** access token sub=`pws_`; server-held refresh family in
  `auth.app_session_anchors`; **same-origin-only, no credentialed CORS**), `/session` (`?mint=1`
  under an **anchor-id advisory lock across the Hydra refresh**, cached access token), `/signout`
  (fixes the live bug; per-app `global`; writes `auth.token_revocations` marker) + anchor cookie via
  `app_session_cookie_name`. **New Liquibase changesets: `auth.app_session_anchors`,
  `auth.token_revocations`** (§8.1). Handler-level tests (incl. N-parallel-mint, foreign-Origin
  reject, >30-min-idle recovery) + a loopback-Hydra integration test.
- 1c. Gateway Bearer arm in `router/auth.rs` (**wrapper path via `state.wrapper_verifier` + raw-Hydra
  path via JWKS**, issuer discriminator, **per-app `client_id`-claim binding**, cross-node
  `token_revocations` family-marker check, expired-Bearer-falls-through-on-Anon, `encode_user_header`)
  + regression tests. **Live-Hydra spike**: confirm a per-app public client's raw access JWT carries
  `client_id` (and what `aud` holds) before finalizing the binding claim (§1.3).
- 1d. Control plane per-app client lifecycle (`app_oauth_client.rs`: create on app-create with
  `backchannel_logout_uri`, sync redirect URIs on deploy, delete on app-delete) +
  `control.app_oauth_clients` Liquibase changeset + idempotency tests. Update
  `backchannel_logout.rs` to disambiguate per-app `aud` and revoke per-app sessions.

**Slice 2 — SDK (Subsystem 2).** Restructure `sdks/auth` to subpath exports; implement
`types.ts`, repaired `server.ts`, `client.ts` (cache/worker/locks/popup/relay/**transaction
(sessionStorage PKCE persistence)**/breadcrumb — **no iframe**), `react.tsx`. Cache keyed on the
gateway-returned `app_ref`. Unit suite for cache/locks/rotation/relay + **opener-reload-mid-flow
recovery** + **opener-null BroadcastChannel/localStorage relay fallback** (round-3 §4.3/§4.4).
`lint:pkg` green. Wire into an example app.

**Slice 3 — declared scopes (Subsystem 3).** Manifest `auth.scopes`; `control.app_scope_defs` +
Hydra client allowlist sync **in one transaction** (deploy-race guard, §5.2); **two-namespace consent
classification (the self-grant fix, §5.1/§5.2)** + remembered grants via the **single
`control.oauth_grants` ledger** (no `auth.app_grants`; per-app clients `skip_consent = false`) + delta
step-up (popup `prompt=consent`); `scope_views` consults `app_scope_defs` (update
`renders_scope_labels`); gateway route-level `required_scopes` + 403 enforcement; `WorkerUser.scopes`
(kernel-contract change, all consumers §1.4); SDK `hasScope`/`requestScopes`. Regression tests:
end-user-can-grant, 403/200, step-up shows only the delta with **no consent loop**.

**Slice 4 — pairwise subjects (Subsystem 4).** **F4-B (gateway-projected) is the default** — Hydra
keeps `subject_type: public` and emits the global UUID; the gateway projects `pws_…` at the
`ZeroShip-User` header boundary (cookie arm from `gateway_sessions.user_id`, raw-Hydra Bearer arm from
the JWT `sub`) **and mints the browser-held WRAPPER access token with sub=`pws_`** so no global UUID
ever reaches app JS (round-3 §1.2/G4). **No `accept_login`/`accept_consent` change, no UUID-parsing
consumer touched.** `auth.app_user_identities` (app_id TEXT; `id` IS the deterministic `pws_`, one row
per `(app, global_user)`, upserted on re-grant §6.4); gateway emits `pws_…` in `ZeroShip-User.id`
(opaque text, wire-format break §6.3); e2e asserts cross-app sub divergence + stability across
re-login, that all three arms (cookie/raw-Hydra/wrapper) project the same `pws_`, and that the
browser token carries no global UUID. F4-A (Hydra-native) stays a future optimization gated on the
S2 spike.

**Slice 5 — relay email (Subsystem 5) — SEPARATE SPEC.** Tracked in
`2026-05-29-relay-email-design.md` (split per §7). Alias generation on first consent (partial-unique
+ generate-and-retry); **managed inbound provider + thin compio webhook handler** (not a bespoke
MX); suppression + loop/abuse guards; grant-revoke cascade; dev MX-sink topology (§7.3). e2e against
the local mailpit/inbucket sink.

A `writing-plans` doc converts each slice into ordered tasks before implementation; each slice is
implemented by a background subagent (opus), reviewed (diff + tests + decisions) before the next.

## 10. Open questions for the review gate

> Round-1 update: the round-0 blockers/majors that masqueraded as "open questions" are now
> **resolved decisions** in the body. Only the genuinely-open items and the two live-Hydra spikes
> remain. <!-- Added in round 1: resolve O1/O3/O4/O5/O6, keep only genuine unknowns + spikes -->
>
> Round-3 closed three blockers (separate `auth.app_session_anchors` store; `AuthPlugin` on the
> worker path; atomic anchor-lock `?mint=1`) and five majors (browser holds a `pws_` wrapper not the
> global-UUID Hydra token; honest nonce scope; cross-node `token_revocations` family marker; single
> `control.oauth_grants` ledger; same-origin-only no-credentialed-CORS; expired-Bearer-on-Anon
> fall-through) plus minors (sessionStorage PKCE transaction; COOP relay fallback; redirect_uri
> bound; consent-time alias creation + read-through cache; `scope_views`/deploy-race reconciliation).
> None of these reopened a decision; they are body fixes. <!-- Added in round 3: index the round-3 closures -->

**Round-3 grounded-fact corrections (verified against the live tree):**
- `crates/gateway/src/sessions.rs:46/50` — `IDLE_MINUTES=30`, `ABSOLUTE_HOURS=12`; `validate()`
  (`:103`) rejects on either expiry. ⇒ the anchor needs its own store (`auth.app_session_anchors`).
- `crates/worker/src/cache.rs:40-46` — `create_plugins()` registers **only `DbPlugin`**; Kv/Storage
  live only in the CLI path (`crates/cli/src/main.rs:108-165`). ⇒ pin `AuthPlugin` to BOTH.
- `crates/auth/src/ui/consent.rs:69-125` — `skip_consent` fast path reads/writes
  `control.oauth_grants` via `load_oauth_grant`/`upsert_oauth_grant`/`scopes_are_subset`. ⇒ single
  ledger; per-app clients `skip_consent=false`.
- `crates/core/src/dpop.rs:980` `TieredJtiCache::with_pg` is PG-tiered (cross-node);
  `crates/core/src/logout_token.rs:99` `LogoutJtiCache` is in-memory. ⇒ back the denylist with PG.
- `crates/gateway/src/wrapper_token.rs` `Issuer::issue`/`WrapperClaims{sub,email,client_id,…}` +
  `state.wrapper_issuer`/`wrapper_verifier` (`lib.rs:136/147`). ⇒ mint the `pws_` wrapper for the browser.
- `ops/hydra-dev.yaml:49-51` — `rotation_grace_period: 30s`, `rotation_grace_reuse_count: 3`,
  refresh `720h`. ⇒ grace softens but ≥3 concurrent minters trip revocation ⇒ anchor-lock mint.

**Resolved (now decided in the body, not open):**
- **O1 — client_id resolution → RESOLVED (§1.5).** A designed wire-format extension to `RouteEntry`
  (`oauth_client_id`+`sector_identifier`), populated by control's route-sync. Not "inject from Host
  magically" — a deliberate, in-patch wire-format change. All app hosts (apex + custom) map 1:1 to
  one per-app client via the existing `name_index`.
- **O3 — server-held refresh → RESOLVED (§1.2/§2, round-3 hardened).** Single-holder: the rotating
  refresh family lives server-side in the **dedicated `auth.app_session_anchors`** store; the browser
  holds none by default (`server_anchor`), or its own *separate* family from a *separate* code
  (`browser_refresh`). Round 3 additionally serializes the **same-family** tab-vs-tab `?mint=1` race
  with an **anchor-id advisory lock across the Hydra refresh** + a cached minted access token. No
  copies, no parallel-grant, no dual-rotation. The silent-iframe alternative is **deleted** (3p
  cookie).
- **O4 — pairwise mechanism → RESOLVED (§6.2; RELOCATED round 2; round-3 wrapper).** Hydra keeps
  `subject_type: public` and emits the **global UUID** (bound at `accept_login`); the gateway projects
  the `pws_…` at the `ZeroShip-User` header boundary on the cookie + raw-Hydra Bearer arms, **and
  mints a per-app wrapper access token (sub=`pws_`) for the browser** so the global UUID never reaches
  app JS (round-3 §1.2/G4). Round 1's "set `pws_` at `accept_consent`" was impossible (subject bound
  at login; `pws_` text breaks four UUID-parsing consumers) — corrected. F4-A (Hydra-native) remains a
  future optimization gated on a spike (sector_identifier_uri vs redirect_uri churn).
- **O5 — relay → RESOLVED (§7): managed inbound provider + thin compio webhook**, split into its
  own spec. A from-scratch zero-tokio MX is deferred unless the managed route is rejected.
- **O6 — signout `global` → RESOLVED (§1.2): "this app, every device."** Revoke all
  `(app_id, user)` anchors; do NOT trigger Hydra RP-logout (which the existing BCL fans out across
  all apps). Per-app BCL disambiguates by `logout_token` `aud` = per-app `client_id`. Platform-wide
  logout is a separate explicit action.

**Two live-Hydra spikes (must run before the dependent slice finalizes):**
- **S1 (Slice 1c) — Bearer binding claim.** Confirm a per-app *public* client's Hydra access JWT
  carries the per-app `client_id` in the `client_id` claim (and what `aud` holds). The Bearer arm
  binds on `client_id`; if Hydra omits it, set each client's `audience = [its own client_id]` and
  bind on `aud`. Decided by the spike, not assumed.
- **S2 (Slice 4) — F4-A viability (optional).** Only if we ever want Hydra-native pairwise: verify
  `pairwise.salt` + `sector_identifier_uri` fetch/validation under redirect_uri churn. Until then,
  F4-B ships.

**Still genuinely open (cosmetic / product decisions):**
- **O2 — Local verify vs introspection on the Bearer arm → RESOLVED (round-3 cross-node).** Local
  verification (wrapper via `state.wrapper_verifier`, raw-Hydra via JWKS — `strategies.access_token=jwt`)
  for speed, **plus** a **cross-node `auth.token_revocations` `(client_id, sub)` family marker**
  (§8.5, backed by shared PG like `dpop_jti`, NOT the in-memory `logout_jti_cache`) for revocation
  that actually works under `--scale`/multi-gate, and a **10-min** wrapper-access TTL bounding the
  residual window. Round 2's "reuse the (in-memory) jti denylist" was unsound multi-node; round 3
  makes the family marker the primary mechanism (signout cannot enumerate per-node jtis). No
  per-request Hydra introspection. *Decided: local verify + cross-node family marker + short TTL.*
- **O7 — Relay reply routing.** v1 = app→user forwarding only; two-way reply re-injection is v2.
  Confirm v1 bounces vs silently drops user replies. (Belongs to the relay sub-spec.)
- **O8 — Manifest `auth.scopes` location.** Top-level manifest `auth.scopes` vs a deploy-time
  control-plane field. *Lean: manifest (creators declare scopes alongside routes), mirrored to
  control on deploy.*
- **O9 — re-grant rotation → RESOLVED (§6.4): deterministic sub reused, alias fresh.** The `pws_`
  is deterministic and reused (the `app_user_identities` row is upserted, `revoked_at` cleared) so
  it never collides with its own PK; only the **relay alias** rotates on re-grant (Apple
  Hide-My-Email applies to the alias, not the sub). Round 1's "fresh sub on re-grant" contradicted
  the deterministic derivation and the `id`-as-PK DDL — corrected.

---

## Loop execution plan (unchanged)
1. **This spec → user review gate.**
2. `writing-plans` → implementation plan with reviewable slices (Subsystem 1 → 2 → 3 → 4 → 5).
3. Implement slice-by-slice via background subagents (opus); review each (diff + tests + decisions)
   before next; commit-only.
4. Faithful e2e: real runtime + dispatcher + example app exercising popup + scopes + pairwise +
   relay. Regression test per fix.
