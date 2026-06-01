# Immersive in-page auth login + dev-proxy session-mint fix

**Status:** IMPLEMENTED offline (2026-06-01); **live e2e + human review pending.** Branch
`feat/auth-immersive-popup` (worktree `.worktrees/auth-immersive-popup`, off main `8d191aaa`).
**Commit-only, NEVER pushed** — the never-push gate is the human-review safety net before any of
the security-sensitive Part 2 code reaches production.

Commits: `59771a58` (Part 1 session-mint fix) · `0edd9702` (Part 2A dev-complete in-page login +
fetch-binding fix) · `218e14d2` (2B Phase 0 refactors) · `<phase1>` (2B Phase 1 crates/auth
credential endpoint) · `<phase2>` (2B Phase 2 gateway endpoint). All security reviews APPROVED;
crate chain builds; offline + unit tests green.

**REMAINING (human-run — not offline-doable):**
1. **Live e2e (Part 2B Phase 3):** run `crates/auth/tests/e2e_password_grant.rs` against a live
   Hydra+Postgres (`AUTH_DB_URL`+`HYDRA_ADMIN_URL`), and a full-stack browser→gateway→auth→Hydra
   pass (docker-compose: Caddy + auth + hydra + gateway) confirming in-page password login mints
   the BFF cookie with no window. Add a gateway↔auth live e2e mirroring `oidc_rp_e2e.rs`.
2. **Review the credential surface** (`crates/auth/src/ui/password.rs`, `oauth/headless.rs`,
   `crates/gateway/src/browser_auth.rs::password`) — esp. the shared-secret gate + first-party
   fail-closed gate — then merge + push.
3. **Config:** set `[secrets].auth_internal_key` / `AUTH_INTERNAL_KEY` for both the auth service
   and the gateway (require_unless_dev makes an empty key fatal in prod); register the per-app
   popup-callback `redirect_uri` for trusted clients; add the console's `oac_` client to
   `[auth].trusted_oauth_clients`.

Two asks on the auth login flow:
1. Fix **"sign-in: session mint request failed"** on the builder `/login`.
2. Replace the **new-browser-window** OAuth popup with an **in-page "immersive"
   login** — no separate OS window.

## Findings (from the understand exploration, wf wndkzd0fg)

- The browser login (`@zeroship/auth` `./client` + `./react`) opens a **new window**
  via `window.open("", "zs:auth", …)` (`sdks/auth/src/internal/popup.ts:26`),
  navigates it to the **same-origin** `${appOrigin}/__zeroship/auth/authorize`
  (gateway → Hydra), and completes via a same-origin postMessage/BroadcastChannel/
  localStorage relay from `/__zeroship/auth/popup-callback`. "Session mint" =
  `POST/GET ${appOrigin}/__zeroship/auth/session[?mint=1]` (BFF: sets HttpOnly
  `__Host-zeroship_app_session`, returns `{user, expires_at}` — no token to the browser).
- `google` vs `password` differ only by the `idp_hint` query param; the hosted
  credential form + Google consent are rendered by the auth service behind the gateway.
- **The hosted login UI is a separate origin** (`auth.zeroship.ai`) with
  `X-Frame-Options: DENY` + `frame-ancestors 'none'` + COOP (`crates/auth/src/headers.rs:60,79,88`).
  It **cannot be iframed**, and Google federation inherently needs a window/redirect.
- The console consumes the SDK purely client-side (same-origin BFF). Its old builder-side
  RP modules (`server/oauth.ts`, `oauth-store.ts`, `session.ts`, `client/api/auth.ts`)
  are **dead/untracked** leftovers; the BFF lives in the gateway.

## Part 1 — dev-proxy session-mint fix (DONE)

**Root cause (dev-harness wiring bug, not an SDK/flow bug):** the `@zeroship/vite-plugin`
dev-server reverse-proxy allowlist (`sdks/vite-plugin/src/dev-server.ts`) forwarded
`/__zeroship/v1/`, `/_rpc`, `/rpc`, `/api/`, and the **retired** `/auth/login|callback|
logout` — but **not** `/__zeroship/auth/*`. So the SDK's `GET /__zeroship/auth/session
?mint=1` fell through to Vite's SPA `index.html`; the fetch failed → the SDK's
`"session mint request failed"` fallback (`transport.ts:242`). The dev-auth provider
(`sdks/bootstrap/src/dev-auth.ts`) already implements the full `/__zeroship/auth/*`
contract — it was simply never reached.

**Fix:** add `/__zeroship/auth/` to the proxy allowlist and drop the dead
`/auth/login|callback|logout` entries (no-back-compat). Now the whole login flow
(authorize → popup-callback → session exchange → mint) reaches the child runtime's
dev-auth provider, exactly as the gateway answers it in prod.

**Regression test:** a dev-server proxy test asserting `/__zeroship/auth/session?mint=1`
is forwarded to the child runtime (returns the dev-auth JSON, not `text/html`
index.html). Would fail pre-fix.

## Part 2 — immersive in-page login

**The only architecture that satisfies "immersive + no new window"** for the password
path is an **in-page embedded credential form** that POSTs same-origin to a new gateway
endpoint (the SDK design's deferred "Phase 2") — iframing the hosted UI is hard-blocked,
and a full-page redirect isn't "immersive". Google/social IdPs **inherently** require a
window/redirect (Google blocks framing) — the modal spawns a popup for those, documented.

### Architecture

```
in-page <AuthModal> (app origin, crystal)         gateway (same-origin BFF)        auth service
  email + password ──POST /__zeroship/auth/password──►  orchestrates server-side:   verifies creds
  (X-ZS-Auth:1, credentials:include, no window)         PKCE authorize → submit      (REUSE the
                                              ◄──{user, expires_at}──  creds to auth login →     existing hosted
  "Continue with Google" ─────► popup window (unavoidable)  code → token exchange →   login handler;
                                                          set __Host BFF cookie       no new verify path)
```

- **SDK (`@zeroship/auth`):** new headless `signInWithCredentials({email,password})` /
  `signUpWithCredentials` on the client (same BFF/identity-only result as the popup
  path), and crystal React components `<AuthModal>` / `<SignInForm>` in `./react` that
  collect credentials and call them — **no `window.open`**. The existing popup path is
  kept for federated providers; `<AuthModal>` renders "Continue with Google" as a popup
  launcher.
- **Gateway (`crates/gateway`):** new same-origin `POST /__zeroship/auth/password`
  (+ `/register`) that, **server-side**, runs the PKCE authorize + submits the
  credentials to the auth service's **existing** login handler + completes the token
  exchange + sets the `__Host-zeroship_app_session` cookie + returns `{user, expires_at}`.
  Reuses the auth service's current credential verification — **no new verify logic**.
- **Auth service (`crates/auth`):** ideally **no new endpoint** — the gateway drives the
  existing hosted-login credential handler headlessly. (Confirm during impl; add a thin
  headless variant only if the current handler is HTML-form-only.)
- **Dev-tier (`sdks/bootstrap/dev-auth.ts`):** implement `POST /__zeroship/auth/password`
  for the dev provider (accept a dev credential, mint the dev session) so `vite dev` has
  full parity.
- **Builder:** `Login.tsx` / `Signup.tsx` use the in-page `<AuthModal>`/`<SignInForm>`
  for the password path; keep a "Continue with Google" popup button.

### Security model (this is credential-handling — treat with care)

- **First-party gated.** The same-origin credential endpoint is enabled **only for
  first-party/trusted app origins** (the console). Arbitrary creator apps keep the
  redirect/popup-to-auth-origin flow so they never handle platform credentials (the
  anti-phishing reason the redirect model exists). Gating via an app-record trust flag /
  gateway config (decide + wire during impl).
- **BFF preserved:** credentials travel only over the same-origin POST; no token ever
  reaches browser JS; the HttpOnly cookie is the only artifact. CSRF: same-origin +
  `X-ZS-Auth` custom header (preflight-gated) + `Origin` exact-match (as the existing
  endpoints already do). Rate-limit the credential endpoint.
- **Never-push** until the user reviews this surface.

### Verification

- Dev e2e (worktree dev server, `zeroship` on PATH + free ports): in-page password login
  completes with **no new window**; Google still opens a popup; `tsc`/`vite`/`vitest`
  green; auth SDK unit tests; an adversarial **security-review** pass on the new gateway
  endpoint (CSRF, origin gating, rate-limit, no-token-leak).

### Concrete backend design (post-exploration, wf wmkmdth11)

There is **no headless credential path today**: the hosted form POSTs
`/login?login_challenge=…` (form-encoded, Argon2id verify in
`crates/auth/src/identity/password.rs`, then Hydra `accept_login` → redirect, not a
code). The gateway is **credential-blind** and `OidcRp` has only
`authorization_code`+`refresh_token` (no ROPC). The session-mint **tail** is fully
reusable: `exchange_code_public` → `verify_id_token` → `anchors::create` +
`sessions::create` → `sign_session_cookie`/`set_app_session_cookie` →
`user_projection` (all `pub(crate)`, `crates/gateway/src/auth_token.rs`).

**Decomposition (build A first; B is the security-critical core):**

- **A — dev-complete in-page login (low-risk, demonstrable in `vite dev`):**
  - `sdks/bootstrap/src/dev-auth.ts`: add a `POST /__zeroship/auth/password` arm
    (`passwordLogin()`) mirroring `exchange()` — JSON `{email,password}`, resolve the
    seeded user by email (password accepted-and-ignored, frictionless dev ethos;
    typed `invalid_credentials` on unknown email), `signDevSession` + `sessionBody`.
    Byte-identical wire to prod. + `bootstrap/tests/dev-auth.test.ts` arm.
  - `sdks/auth`: replace the popup `signInWithPassword` (client.ts:203-207) with a
    **headless** `signInWithCredentials({email,password})` that POSTs same-origin to
    `/__zeroship/auth/password` (`X-ZS-Auth`, credentials:include) and stores the
    returned identity-only session — **no window**. Add crystal `<AuthModal>` /
    `<SignInForm>` to `./react`. `signInWithOAuth` (popup) stays for Google.
    + `react.test.tsx` regression test.
  - Builder `Login.tsx`/`Signup.tsx`: in-page `<SignInForm>` for the password path;
    keep a "Continue with Google" popup launcher.
- **B — prod backend (security-critical; adversarial review; never-push gates it):**
  - New headless credential endpoint in `crates/auth` (JSON `{email,password,…}`)
    reusing `password::verify` + dummy-hash enumeration defense + ratelimit +
    eligibility + audit from `ui/login.rs`, that drives Hydra to mint a code
    (server-side authorize → accept_login with `acr=urn:zeroship:pwd`/`amr=[pwd]`,
    consent auto-skipped for the trusted first-party client).
  - Gateway `POST /__zeroship/auth/password`: `same_origin_guard(.., true, true)` +
    **first-party gate** (`route.client_id ∈ auth.trusted_oauth_clients`; move the
    `resolve/contains` helpers from `crates/control` to `crates/core` so the gateway
    can use them), server-holds the PKCE verifier, calls the auth-service endpoint,
    then reuses the session-mint tail. Returns `{user, expires_at}` + BFF cookies.
  - **Open questions to resolve in B (documented for review):** whether Hydra permits
    a server-driven code mint without an interactive browser challenge (else a
    server-side redirect-follower); reconciling `service.rs::login`'s own JWT with the
    BFF model (no-back-compat → likely collapse).

### Part 2B — concrete design (post Hydra-feasibility, wf wjxxyty1j)

**Mechanism (no security landmine):** Hydra has **no ROPC**, but a code can be minted
**server-side by replaying the existing, audited authorization_code+PKCE dance with a
cookie jar** — `crates/auth/tests/e2e_password.rs` already proves this headless flow
(GET `/oauth2/auth` → `login_challenge` → `accept_login` → consent → `?code=`). Identity
scopes (openid/profile/email/offline_access) **auto-accept silently** (no UI) even with
per-app `skip_consent=false`. No new grant, no admin code-minter — reuse audited primitives.

**Flow:** gateway `POST /__zeroship/auth/password` → `resolve_route` → `same_origin_guard(..,true,true)`
→ **first-party gate** (`route.client_id ∈ trusted_oauth_clients`, fail-CLOSED) → gateway
generates PKCE verifier+challenge → calls `crates/auth` `POST /password` (JSON
{email,password,client_id,redirect_uri,scope,nonce,code_challenge}) → gets `{code}` →
**reuses the extracted `mint_session_from_code`** (exchange_code_public → verify_id_token →
anchors+sessions::create → sign_session_cookie → 3 cookies → {user,expires_at}). `amr=[pwd]`/
`acr=urn:zeroship:pwd` flow through `accept_login` into the id_token automatically.

**SECURITY (mandatory — this is a credential→code oracle):**
1. The `crates/auth` `/password` endpoint MUST require a **gateway↔auth shared secret**
   (HMAC/bearer header, mirroring the worker/control-key pattern) — `same_origin_guard`
   lives only on the gateway, so the auth endpoint is otherwise dial-able by anyone. Without
   this it's a full auth-bypass oracle. **Hard requirement.**
2. The gateway first-party gate **fails closed** on unknown/empty `client_id` → 403.
3. `verify_password_credentials` (extracted from `ui/login.rs`, sharing the constant-time
   argon2/dummy-hash/ratelimit/eligibility/audit path) returns on EVERY failure arm **before**
   the headless dance — no arm reaches code-mint without a successful verify.
4. Headless dance only completes for first-party `skip_consent` clients.
→ Adversarial security-review pass over 1–4.

**Phased build (loop drives these):**
- **Phase 0 (safe refactors, behavior-preserving, test-verified):** move
  `resolve_trusted_oauth_clients`/`is_trusted_client_id` `crates/control`→`crates/core`
  (re-export from control); extract gateway mint tail (`auth_token.rs:354-586`) →
  `pub(crate) mint_session_from_code` and rewire `session_post`; add
  `trusted_oauth_clients: HashSet<String>` to `GateState` (populate in `main.rs` from `file.auth`).
- **Phase 0b (control+core — the console must be usable for in-page login):**
  - `ensure_app_client` (`crates/control/src/app_oauth_client.rs`) gains a `first_party: bool`
    param. Creator apps pass `false` ⇒ `skip_consent=false` (spec §5.2 — per-app clients NEVER
    skip consent; this targets THIRD-PARTY creator apps). The console passes `true` ⇒
    `skip_consent=true` (`bootstrap_console.rs`) — an **owner-approved, narrow exception**: a
    consent prompt for the platform's OWN console is meaningless. The flag is mirrored to
    `control.oauth_clients.skip_consent` in the same txn so the DB and the live Hydra client never
    disagree. The exception applies ONLY to the console, never to creator apps.
  - `default_trusted_oauth_clients()` (`crates/core/src/auth/trusted_clients.rs`) now returns the
    **EMPTY set (fail-closed)** — the retired `BUILDER_CLIENT_ID="zeroship-builder"` default is
    removed. No client is trusted unless `[auth].trusted_oauth_clients` explicitly names it. The
    constant `BUILDER_CLIENT_ID` (if still needed) moves to its sole consumer
    `crates/control/src/bootstrap_builder.rs`.
  - **Deployment requirement (load-bearing).** Core has no console host, so it CANNOT derive the
    console's `oac_` client id. The deployment MUST set `[auth].trusted_oauth_clients` to the
    console's client id = `client_id_for_app(console_app_id(console_host))`, which
    `zeroship-control --bootstrap-console` prints at boot. The gateway reads this overlay into
    `GateState.trusted_oauth_clients`; until it names the console client, the first-party password
    gate fails closed and in-page console login is locked out (the secure default).
- **Phase 1 (`crates/auth`):** extract `verify_password_credentials` from `ui/login.rs`;
  promote the cookie-jar dance (`tests/common`) → `oauth/headless.rs::mint_code_for_subject`;
  add `ui/password.rs` + route + the **shared-secret gate**; `e2e_password_grant` test.
- **Phase 2 (gateway):** `browser_auth::password` + route reg + first-party-gate & guard
  regression tests (the gate-fails-closed test is the security-critical one).
- **Phase 3:** faithful live gateway↔auth e2e (real Hydra) — real `mint_session_from_code`, no shim.

## Out of scope / risks

- Google/social in-page is impossible (Google blocks framing) — popup retained.
- 3rd-party creator apps keep the popup/redirect (security); only first-party gets the
  in-page form.
- The auth-service credential-verify reuse must be confirmed against the real
  `crates/auth` login handler during the gateway phase; if it's HTML-only, a thin
  headless JSON variant is added there.
- Pre-launch, no back-compat: the retired builder RP routes/comments are deleted, not shimmed.
