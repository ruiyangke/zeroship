# Immersive in-page auth login + dev-proxy session-mint fix

**Status:** pilot-decided (offline, 2026-06-01). Branch `feat/auth-immersive-popup`
(worktree `.worktrees/auth-immersive-popup`, off main `8d191aaa`). **Commit-only,
NEVER push** — the never-push gate is the human-review safety net before any of the
security-sensitive Part 2 code reaches production.

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

## Out of scope / risks

- Google/social in-page is impossible (Google blocks framing) — popup retained.
- 3rd-party creator apps keep the popup/redirect (security); only first-party gets the
  in-page form.
- The auth-service credential-verify reuse must be confirmed against the real
  `crates/auth` login handler during the gateway phase; if it's HTML-only, a thin
  headless JSON variant is added there.
- Pre-launch, no back-compat: the retired builder RP routes/comments are deleted, not shimmed.
