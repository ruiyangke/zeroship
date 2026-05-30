# @zeroship/auth — in-app popup login SDK (whole-vision design)

> Status: **DRAFT / spec-in-progress.** This file is the durable source of truth for the
> `feat/auth-sdk-popup` loop. Locked decisions + grounded facts below; full per-subsystem
> design is being authored (SDK-internals section pends the Auth0 research workflow).
> Date: 2026-05-29. Branch: `feat/auth-sdk-popup` (worktree `.worktrees/auth-sdk-popup`).
> Discipline: **commit-only, never push**; orchestrate via subagents (pilot mode); every
> fix carries a regression test; tests must run the real path (no shims).

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
| 1 | **Per-app OAuth + gateway browser-auth foundation** | per-app public PKCE client (control-plane registers on deploy); gateway same-origin `/__zs/auth/{token,popup-callback,session}`; gateway **Bearer arm** → `ZeroShip-User`; fix orphaned **`env.auth`** wiring | — |
| 2 | **`@zeroship/auth` SDK** | client (popup, silent-auth iframe, `getSession`/`getUser`/`onAuthStateChange`/`refreshSession`/`signOut`, `cacheLocation`, rotation + cross-tab lock) + React adapter + server entry | 1 |
| 3 | **Declared permission scopes** | scope registry, manifest `scopes:`, scope-aware consent UI, per-scope gateway enforcement, SDK scope surface | 1 |
| 4 | **Pairwise subject identifiers** | Hydra pairwise subjects + per-app sector salt; `ZeroShip-User.id` becomes per-app `sub` | 1 |
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
  window** → popup AND hidden-iframe silent-auth both just redirect back to our own relay page
  and `postMessage` the opener. **No cross-origin postMessage, no COOP/`web_message` needed.**
- For per-app consent we register **one public PKCE client per app** (not the shared gateway
  client). Control-plane must create it on app create/deploy + register its redirect URIs.

### Fact 2 — gateway Bearer path (`crates/gateway/src/router/auth.rs`)
- Today gateway accepts **cookie sessions + DPoP only**; plain `Authorization: Bearer` is
  explicitly rejected (test `auth.rs:561-569`).
- DPoP path (`resolve_dpop_user_header`, `auth.rs:184-359`) already implements the full
  machinery: extract token → verify (local wrapper JWT via `state.wrapper_verifier`, or Hydra
  `/oauth2/introspect`) → `build_worker_user_from_{wrapper,introspection}` →
  `oidc_rp::encode_user_header()` (`oidc_rp.rs:545-576`).
- Gateway **already holds Hydra JWKS** (`OidcRp.jwks: Arc<JwksCache>`, `oidc_rp.rs:46-79`) for
  local JWT verification.
- ⇒ Adding a **Bearer arm** in `resolve_auth()` (verify Hydra access-token JWT via JWKS, or
  introspect; synthesize `ZeroShip-User`) is ~50 lines reusing existing code. Worker handoff
  unchanged. Decide policy gating (gate same as cookie: require `User`). Risk: plain Bearer has
  no sender-constraint (replay) — short TTL + HTTPS; DPoP-binding available as later hardening.
- OPEN: set Hydra `strategies.access_token = jwt` so gateway verifies locally (fast) instead of
  introspecting every request. Verify current setting.

### Fact 3 — Hydra public client + token exchange (`ops/hydra-dev.yaml:44-51`, `ops/auth-clients-dev.toml:30-38`, `crates/gateway/src/dpop_exchange.rs`)
- Hydra **already** configured for public PKCE clients w/ rotating refresh tokens: PKCE enforced
  (`enforced_for_public_clients: true`), refresh TTL 720h, rotation grace 30s / reuse_count 3.
- `zeroship-cli` is a live public client (`token_endpoint_auth_method="none"`, `offline_access`,
  `refresh_token` grant) — proof the shape works.
- Hydra has **no CORS** → browser cannot call `/oauth2/token` directly.
- ⇒ Recommended: **gateway same-origin proxy `POST /__zs/auth/token`** forwarding to Hydra
  `/oauth2/token`, mirroring the `dpop-exchange` precedent (~150 LOC, `Cache-Control: no-store`,
  no Hydra config change). Caddy already proxies `/oauth2/*` → hydra:4444.
- Gateway CORS infra exists (`crates/gateway/src/router/cors.rs`).

### env.auth (`crates/runtime/src/auth.rs`, `core/plugin.rs`, `core/runtime.rs:1471`)
- Per-request user storage IS wired (`set_request_user` in dispatch). But
  `get_user_callback`/`require_user_callback` are **orphaned** — no `env.auth` plugin registered
  in `build_env_object` (only db/kv/storage). ⇒ `env.auth.getUser()` is `undefined` today; even
  the current SDK's server `getUser()` returns null. Must register an AuthPlugin.
- RPC handlers read `ctx.user` (`rpc/ctx_holder.rs`), lazy-`JSON.parse`d.

### Existing SDK + conventions
- `sdks/auth/` exists but minimal + broken: server-only `getUser/requireUser/isLoggedIn/signOut`;
  `signOut` 302s to `/__zs/auth/signout` which **has no gateway handler** (live bug); `dist/` is
  stale (carries removed `window.__zs_user`). No `signIn*`/`getSession`/`onAuthStateChange`.
- SDK conventions: ESM-only, tsup, ES2022, strict TS, `exports` map, `.attw.json` profile
  `esm-only`, `lint:pkg = publint && attw --pack .`, tests `node --import tsx --test tests/*`.
  `zeroship` + `@zeroship/*` always external. Server/client split via **subpath exports** (copy
  `sdks/rpc`: `.`/`./server`/`./client`; rpc tsconfig has `lib:['ES2022','DOM']`). React adapter
  ⇒ `./react` subpath + `peerDependencies.react` (see `sdks/server`).

## Supabase / Auth0 patterns to replicate
- Supabase client API: `signUp`, `signInWithPassword`, `signInWithOtp`, `signInWithOAuth({skipBrowserRedirect})`,
  `exchangeCodeForSession`, `getSession` (local), `getUser` (server-validated), `refreshSession`,
  `onAuthStateChange`, `signOut({scope})`. Session = `{access_token, refresh_token, expires_at, user}`.
- Popup OAuth: `window.open` → same-origin callback relays `code` via `postMessage`/`BroadcastChannel`
  → `exchangeCodeForSession` (PKCE).
- **Auth0 (per user request) — pending the `auth0-spa-impl-research` workflow:** `cacheLocation:'memory'`
  default; silent renewal via hidden iframe `prompt=none`; refresh-token rotation + reuse detection;
  cross-tab `browser-tabs-lock`; `is.authenticated` session-presence cookie; `loginWithPopup` vs silent
  iframe distinction. **Will fill the SDK-internals section once that result lands.** Our same-origin
  callback removes the need for Auth0's `web_message` response mode — we reuse the redirect+relay page
  for both popup and silent iframe.

## Loop execution plan
1. (pending Auth0) Author the full per-subsystem design into this file via subagent; spec self-review; **user review gate**.
2. `writing-plans` → implementation plan with reviewable slices (subsystem 1 → 2 → 3 → 4 → 5).
3. Implement slice-by-slice via background subagents (opus); review each (diff + tests + decisions) before next; commit-only.
4. Faithful e2e: real runtime + dispatcher + example app exercising popup + scopes + pairwise. Regression test per fix.

## Open design questions to resolve while authoring
- Token shape: Hydra JWT access tokens (local JWKS verify) vs opaque (introspect). Set `strategies.access_token=jwt`?
- Per-app client lifecycle: created on app-create or first-deploy? secret-less public; how `client_id` maps to `app_id`; cleanup on app delete.
- Pairwise sector identifier: per-app `client_id` as sector vs explicit `sector_identifier_uri`. Salt storage.
- Relay email: inbound forwarding infra (new crate vs extend mailer); alias format; per-grant revocation; bounce/abuse.
- Scope registry: where declared (manifest field), reserved identity scopes vs app-defined; enforcement point (gateway route policy vs worker); token `scope` claim size.
- Silent-auth on memory-cache reload: refresh-token grant (works w/ Hydra) primary; hidden-iframe `prompt=none` → same-origin relay fallback for `login_required`/`consent_required`.
- Logout: fix `/__zs/auth/signout`; `scope: local|global`; relation to Hydra back-channel logout (currently revokes all sessions all apps).
