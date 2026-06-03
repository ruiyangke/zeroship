# Auth dev-tier primitive

The platform gives every primitive a **dev-tier implementation** behind one
contract, so `pnpm dev` runs an app with zero platform infra:

| Primitive   | Prod tier                         | Dev tier (self-contained)            |
| ----------- | --------------------------------- | ------------------------------------ |
| `env.db`    | PostgreSQL                        | SQLite (embedded)                    |
| `env.kv`    | Redis                             | redb (embedded)                      |
| `env.storage` | S3 / R2                         | LocalFs                              |
| **auth**    | **gateway BFF + Hydra**           | **in-process dev-auth provider**     |

Auth was the missing one. This note describes the dev tier: the contract it
mirrors, the dev implementation, and the dev-only-by-construction guarantee.

Like SQLite is a *real* implementation of the DB contract (not a fake Postgres),
the dev-auth provider is a *real* implementation of the auth contract — not a
stub. The same `@zeroship/auth` client code and the same server-side
`env.auth.getUser()` / `currentUser()` calls run byte-identical in dev and prod;
only the backend that answers them differs.

## The contract (prod tier — unchanged)

The platform uses the **BFF model**: the browser holds an HttpOnly signed
session cookie + an identity projection, never a token. Two surfaces:

1. **Browser** — the `@zeroship/auth` client drives same-origin endpoints:
   - `GET  /__zeroship/auth/authorize`     → the login hop. The first-party
     password UI renders an **immersive iframe** on the same-site console
     (`auth.zeroship.ai`'s real `/login` framed in-page — the Stripe-Elements
     model), and a **popup** everywhere else; federated providers
     (`google`/`github`) are always a popup. (There is no in-page credential
     POST: the password is typed into the cross-origin auth iframe, which the
     SOP forbids console JS from reading.)
   - `GET  /__zeroship/auth/popup-callback`→ same-origin HTML relay; postMessages
     `{ type: "zs:authorization_response", response: { code, state } }` to the
     launcher window — `window.opener` (popup leg) if present-and-distinct, else
     `window.parent` (iframe leg), pinned to `location.origin` (never `'*'`).
   - `POST /__zeroship/auth/session`       → code→session exchange; sets the session
     cookie; returns `{ user, expires_at }` (NO token in the body).
   - `GET  /__zeroship/auth/session[?mint=1]` → read / re-mint; `{ user, expires_at }`.
   - `POST /__zeroship/auth/signout`       → revoke + clear; `204`.

   A failed sign-in surfaces the re-rendered login form with an error banner: in
   prod the framed `auth.zeroship.ai/login` re-renders `invalid email or
   password` (401) in-frame; the dev tier renders the **same** banner from its
   own in-frame form, so the failure path is real in dev too (clear/alter the
   prefilled fields to exercise it).

   The `user` projection is `{ id, email, emailVerified, name, avatar, scopes }`
   (client camelCase); the wire body uses snake_case `email_verified`. `id` is
   an opaque per-app pairwise subject (`pws_…`), never the global `usr_…`.

2. **Server** — app code reads the authenticated identity via
   `env.auth.getUser()` / `env.auth.requireUser()` (the `@zeroship/auth` server
   helper) and `currentUser()` (the `zeroship` module / `@zeroship/server`). In
   prod these are fed by the gateway's request-bound `ZeroShip-User` header
   (`base64(JSON).<request_id>.<iat>.<hex-hmac>`, signed with the worker key —
   `crates/core/src/auth.rs`, `crates/gateway/src/oidc_rp.rs`). The worker
   verifies + decodes it into `user_json` and threads it through
   `Runtime::call_fetch_handler_with_user`, which populates BOTH the
   `env.auth` per-request state (`crate::auth::set_request_user`) and the RPC
   ctx (`mint_rpc_ctx(user_json)` → `currentUser()`).

The prod tier (gateway + Hydra) is the faithful validation surface and is NOT
touched by the dev tier. Its integration tests + live E2E stay authoritative.

## The dev tier

Two pieces, mirroring the two contract surfaces:

### 1. Browser endpoints — `@zeroship/bootstrap` `dev-auth.ts`

`createDevAuthProvider(getEnv)` serves the same-origin `/__zeroship/auth/*` routes
from inside the dev runtime — no gateway, no Hydra:

- `authorize` (GET) → render a **real dev login form** in-frame (the same
  same-origin iframe the SDK drives in prod). It is **prefilled** with the
  selected dev user's credentials so sign-in is one click, but it is **NOT
  auto-submitted** — the developer clicks "Sign in", exactly mirroring prod's
  framed `auth.zeroship.ai/login`. The form carries a CSRF token (same
  double-submit *contract* as prod's `__Host-zsidp_csrf`, but server-reflected:
  the GET renders the token into both the cookie and the hidden field, so the
  cookie stays `HttpOnly` — a stricter dev variant) and hidden `state` /
  `redirect_uri` (pinned to the exact same-origin callback path). Multi-user
  configs render an email `<select>` of the configured users (the default
  pre-selected); a tiny inline script re-prefills the password on change. There
  is **no** frictionless 302 and **no** separate picker step.
- `authorize` (POST) → the form submit. Validates CSRF + email + password
  against the configured dev users; on success mints a dev code and 302s to
  `/__zeroship/auth/popup-callback?code&state`; on failure re-renders the form
  with the `invalid email or password` banner (401) — same shape as prod.
- `popup-callback` → the same relay **contract** the gateway serves — the
  dual-target postMessage (`window.opener` for the popup leg, else
  `window.parent` for the immersive iframe leg, pinned to `location.origin`). It
  is a CSP-free localhost variant (prod serves the relay under a nonce'd CSP),
  so the inline script is behaviourally identical but not byte-identical.
- `session` (POST) → spends the dev code, mints the `__zeroship_dev_session` cookie,
  returns `{ user, expires_at }`.
- `session` (GET / `?mint=1`) → read / re-mint, or `401 { error: "login_required" }`.
- `signout` → clears the cookie; `204` (idempotent).

Every wire shape is identical to prod, so the `@zeroship/auth` client is
unchanged dev↔prod. The provider is wired into the dev fetch handler in
`dev-entry.ts` (`@zeroship/bootstrap/dev`), ahead of the user module's fetch.

### Framed same-origin dev-login parity

There is **no separate `auth.zeroship.ai` origin in dev** — the dev-auth
provider IS the auth service, served **same-origin** through the
`@zeroship/vite-plugin` dev-server proxy (which forwards the whole
`/__zeroship/auth/*` prefix to the spawned `zeroship serve` child runtime). The
immersive iframe's `src` is the same `/__zeroship/auth/authorize?…`, which in dev
is same-origin (localhost), so the iframe loads with no header relax needed:

- The login-form/authorize/callback responses set only `content-type` +
  `cache-control` (no `X-Frame-Options` / `frame-ancestors`), so they are already
  frameable same-origin. The prod-only framing relax (auth-service
  `frame-ancestors` allowlisting the console) is **N/A** in dev; the
  `frame_ancestor_origins` config is a no-op (or set to the dev console origin
  for symmetry).
- Isolation is **N/A** in dev (one localhost origin; nothing to isolate), but the
  **UX/flow is identical**: the SDK opens the in-page iframe, the dev authorize
  renders the prefilled login form in-frame, the developer clicks "Sign in", the
  POST 302s to the in-frame callback, the callback postMessages `{code,state}`
  to `window.parent`, and the SDK drives `POST /session`.

### 2. Server-side identity — `__zeroship_dev_session` cookie → `user_json`

The `__zeroship_dev_session` cookie value is
`base64url(user_json) "." hex-HMAC-SHA256(secret, base64url-payload)`, signed
with a per-dev-server secret (`ZEROSHIP_DEV_AUTH_SECRET`). The dev runtime's
serve path (`crates/runtime/src/core/serve.rs::handle_request`) calls
`dev_auth::resolve_dev_user_json(headers)` BEFORE dispatch: it reads the cookie,
verifies the dev HMAC, and threads the decoded `user_json` through the SAME
`call_fetch_handler_with_user` path the worker uses for the gateway header. So
`env.auth.getUser()` and `currentUser()` resolve the dev user server-side with
the identical native plumbing — only the *producer* of the cookie differs (the
dev JS provider vs the gateway).

The JS `signDevSession` (`dev-auth.ts`) and the Rust `dev_auth::sign_dev_session`
(`dev_auth.rs`) emit the identical token envelope, so a cookie the browser-side
provider mints verifies in the runtime.

This is NOT the old console dev-bypass (`isDevAutoAuth`) — that synthetic-user
footgun lived in *shipped client code* and was deleted in the cutover. The dev
tier lives exclusively in the dev runtime; no dev/prod flag exists in app code.

### Configuration — the `devAuth` Vite-plugin option

```ts
zeroship({
  // default in dev: the built-in dev user (pws_dev… / dev@localhost /
  //   scopes openid profile email / password "dev")
  devAuth: true,

  // one configured user (password defaults to "dev" when omitted)
  devAuth: { user: { email: "alice@localhost", scopes: ["openid", "admin"], password: "s3cret" } },

  // multiple users (the login form renders an email dropdown at /authorize)
  devAuth: { users: [{ id: "pws_a", name: "A", email: "a@x" },
                     { id: "pws_b", name: "B", email: "b@x", password: "bee" }],
             defaultUserId: "pws_a" },

  // off — /__zeroship/auth/* falls through; env.auth.getUser() is anonymous
  devAuth: false,
})
```

The login form **prefills + validates** each user's `password` (default `"dev"`,
overridable per user) — sign-in is one click, but it is not auto-login and the
`invalid_credentials` failure arm is real. The `password` is *not* a secret: it
is prefilled in the page and never enters the `{user}` identity projection /
session cookie.

The plugin serializes the resolved config into `ZEROSHIP_DEV_AUTH` and mints a
fresh `ZEROSHIP_DEV_AUTH_SECRET`, both passed to the spawned `zeroship serve`
child (`sdks/vite-plugin/src/{dev-auth-config,dev-server,constants}.ts`).

## Dev-only by construction

The dev-auth provider must never reach a production `.zship`:

- It lives in `@zeroship/bootstrap`'s `dev-auth.ts`, imported ONLY by
  `dev-entry.ts` (`@zeroship/bootstrap/dev`). The production
  `runtime-entry.ts` never imports it.
- It is **deliberately NOT re-exported from the `@zeroship/bootstrap` barrel**
  (`index.js`) — the prod synthetic SSR entry does `import "@zeroship/bootstrap"`
  for side effects, so a barrel re-export would risk pulling the provider into
  the shipped worker. Consumers reach it via the `./dev-auth` subpath or
  transitively via `./dev`, neither of which the prod entry imports.
- The runtime hook `dev_auth::resolve_dev_user_json` is a no-op unless
  `ZEROSHIP_DEV=1` (set only on the dev `serve` child).

This is **grep-provable** and guarded by a test
(`sdks/bootstrap/tests/dev-auth.test.ts`): the prod artifacts
(`dist/runtime-entry.js`, `dist/index.js`, `dist/dispatcher.js`) carry no
`createDevAuthProvider` / `__zeroship_dev_session` / `signDevSession` /
`/__zeroship/auth/authorize` symbols.

## Tests (the faithful path)

- `crates/runtime/tests/dev_auth.rs` — drives the real serve-path seam
  (`resolve_dev_user_json` → `call_fetch_handler_with_user`) and asserts BOTH
  `env.auth.getUser()` and `currentUser()` resolve the dev user from the cookie,
  plus forgery rejection (wrong-secret cookie → anonymous). No gateway/Hydra.
- `sdks/bootstrap/tests/dev-auth.test.ts` — exercises the real provider through
  the full `/__zeroship/auth/*` flow + the WebCrypto HMAC cookie roundtrip, and the
  production-build absence guard.
- `sdks/auth/tests/dev-tier.test.ts` — the real `@zeroship/auth` client driving
  the real dev provider end-to-end (`signInWithOAuth` popup flow, `getUser`,
  `signOut`) — the contract-parity proof.

## Files

```
crates/runtime/src/core/dev_auth.rs        cookie verify → user_json (dev-gated)
crates/runtime/src/core/serve.rs           handle_request calls resolve_dev_user_json
sdks/bootstrap/src/dev-auth.ts             the /__zeroship/auth/* provider + cookie signing
sdks/bootstrap/src/dev-entry.ts            wires the provider into the dev fetch handler
sdks/vite-plugin/src/dev-auth-config.ts    devAuth option → env pair + secret
sdks/vite-plugin/src/dev-server.ts         passes the env pair to the serve child
```
