# Immersive auth login — cross-origin same-site iframe (Stripe-Elements model)

**Status:** DESIGN (re-spec, 2026-06-01). Supersedes the same-origin `/__zeroship/auth/password`
credential-oracle design. Branch `feat/auth-immersive-popup` (worktree `.worktrees/auth-immersive-popup`).
**Commit-only, NEVER push** — the never-push gate is the human-review safety net before any
framing/header change reaches production.

## 1. Summary

The console's immersive password login pivots **from** a same-origin in-page credential POST
(`POST /__zeroship/auth/password` → gateway first-party gate → auth `/password` headless oracle,
gated by the `auth_internal_key` shared secret) **to** a **cross-origin, same-site iframe** that
embeds `auth.zeroship.ai`'s real login form inside an in-page modal on `console.zeroship.ai` — the
Stripe Elements model.

Why this is the right shape:

- **Stripe-style isolation by the Same-Origin Policy.** The password is typed *into* the
  `auth.zeroship.ai` iframe — a **different origin** than the console's JS — so the SOP forbids
  console JS from reading the credential at all. The old `/password` form had console JS handle the
  credential, which is exactly why it needed the `auth_internal_key` shared-secret oracle plus a
  first-party gate to contain it. The iframe removes the premise: there is nothing to contain.
- **Same-site → first-party cookies, for free.** `console.zeroship.ai` and `auth.zeroship.ai` are
  both subdomains of `zeroship.ai` → **same site** (storage partitioning keys on eTLD+1, not origin).
  The auth iframe's session/CSRF cookies are therefore **first-party** → none of the
  third-party-storage-partitioning / ITP breakage applies. This is precisely the trap that forced
  Firebase off its cross-site auth iframe; the console has the escape for free.
- **Drop-in for the popup WINDOW, not the BFF.** The iframe is a replacement for `window.open` only.
  It runs the EXACT same `authorize → auth.zeroship.ai/login → popup-callback → POST /session` dance,
  just framed instead of windowed, and reuses all the BFF code-exchange/session machinery and the
  same `__Host-` cookies.

What gets **deleted** (no shim, pre-launch): the gateway `POST /__zeroship/auth/password` handler and
its first-party gate; the auth-service `/password` headless credential endpoint
(`ui/password.rs` + `oauth/headless.rs` + `oauth/mod.rs`); the `auth_internal_key` shared secret in
both services + core config; `OidcRp::password_login`; the SDK `signInWithCredentials` /
`Transport.passwordLogin` / `CredentialsInput` / React `SignInForm` credential plumbing; the dev-auth
`passwordLogin` arm.

What gets **modified**: the auth-service security headers — the global `SecurityHeaders` middleware
becomes **route-aware** so the framed login documents emit `frame-ancestors` allowlisting the console
and DROP `X-Frame-Options` (everything else keeps `XFO: DENY` + `frame-ancestors 'none'`); the
gateway/dev popup-callback relay (also target `window.parent`, not only `window.opener`); the SDK
`signInWithOAuth` launcher + `SignInOptions.provider` union (re-add `password`) + `AuthClientOptions`
(new `authOrigin`/`immersive` field) + `AuthModal` (host the iframe instead of the credential form);
the builder `Login.tsx`/`Signup.tsx` (host the iframe launcher) + the three builder e2e specs; the
`crates/auth/tests/threat_model.rs` clickjacking-headers test (assert the NEW framed contract).

What is **retained** despite looking password-only: `mint_session_from_code` + the `auth_token.rs`
helpers (the surviving `/session` path); `verify_password_credentials` + `identity/password.rs` (the
interactive `/login` the iframe frames); and the `trusted_oauth_clients` / `skip_consent` / `first_party`
machinery in `crates/core` + `crates/control` + `ops/*.toml` (its **gateway** consumer dies; its
**control** consumer — the console's `skip_consent=true`, so the framed login auto-accepts identity
consent — stays).

---

## 2. Background & topology

### 2.1 The two systems and the origins

```
console.zeroship.ai      Creator Platform console (the app surface that gets the iframe)
{app}.zeroship.ai        Creator apps (keep the popup; never get the iframe)
auth.zeroship.ai         The auth service: /login /signup /consent /oauth/google + Hydra /oauth2/*
                         — a SEPARATE origin, same SITE (eTLD+1 = zeroship.ai)
```

Under the **app/console** origin, the gateway serves the same-origin BFF "integration" endpoints
(`crates/gateway/src/browser_auth.rs` + `crates/gateway/src/auth_token.rs`):

```
GET  /__zeroship/auth/authorize        the ONE cross-site hop: build the per-app PUBLIC PKCE client's
                                       Hydra /oauth2/auth URL, 302 the browser CROSS-SITE to auth.zeroship.ai
GET  /__zeroship/auth/popup-callback   same-origin (app) HTML relay; the OAuth code lands here and is
                                       postMessage'd to the opener (popup) / parent (iframe)
POST /__zeroship/auth/session          code(+PKCE verifier) → session; sets HttpOnly signed
                                       __Host-zeroship_app_session (Lax, ~15m) + __Host-zeroship_app_anchor
                                       (Strict, 30d). No token in the body.
GET  /__zeroship/auth/session          {user, expires_at} (?mint=1 re-signs from the anchor)
POST /__zeroship/auth/signout
POST /__zeroship/auth/password         *** DELETED *** (the same-origin credential POST)
```

### 2.2 Why the same-origin `/password` model didn't isolate the credential

In the deleted model, console JS (`signInWithCredentials`) collected `{email, password}` in its **own**
realm and POSTed them to the gateway. The credential transited app-origin script. To contain that we
needed two devices:

1. The gateway **first-party gate** (`is_trusted_client_id(&state.trusted_oauth_clients, …)`), so only
   the console could reach the endpoint.
2. The gateway↔auth **shared secret** (`auth_internal_key`), so the auth `/password` oracle — by
   construction a "give me `{email,password}` and I'll mint an OAuth code" endpoint — was not
   world-dialable.

Both devices exist only because the credential touches app-origin script. They are containment for a
weakness the redirect/popup model never had.

### 2.3 The Stripe / Firebase lessons, and the same-site insight

- **Stripe Elements** embeds `js.stripe.com` / `hooks.stripe.com` frames inside arbitrary merchant
  sites. The card number is typed into Stripe's cross-origin frame; the merchant's JS cannot read it
  (SOP). We adopt the same isolation but are **tighter**: we allow exactly one embedder (the console).
- **Firebase** ran its hosted auth iframe on `*.firebaseapp.com` while apps lived on the developer's
  own domain → **cross-site** → the iframe's session/`__session` cookies became third-party and were
  partitioned/blocked by Safari ITP and Chrome's third-party-cookie phase-out, breaking silent session
  continuation. Firebase's #1 fix was literally "host auth on a **subdomain** of your app so the iframe
  is same-site."
- **The same-site insight:** `console.` and `auth.` are siblings under `zeroship.ai`, so the auth
  iframe is **same-site, cross-origin**. Browser storage isolation keys on **site** (eTLD+1), so the
  iframe is a **first-party** context — its cookies are not partitioned, not blocked. We get Firebase's
  remediation for free for the console. **This property is console-only** and does NOT generalize to
  custom-domain consoles (§6.5).

---

## 3. Architecture

The iframe is a drop-in for the popup WINDOW. The console opens an in-page modal containing
`<iframe src="${appOrigin}/__zeroship/auth/authorize?…">`. The iframe navigates cross-site to
`auth.zeroship.ai`, the user authenticates inside the auth origin, Hydra mints a code, the browser
302s back **same-origin** to `${appOrigin}/__zeroship/auth/popup-callback` (still inside the iframe),
and the callback `postMessage`s `{code, state}` to `window.parent`. The console SDK's relay listener
(registered on the console top window) receives it and drives the surviving
`POST /__zeroship/auth/session` exchange. **Federated providers (`google`, `github`, …) stay popup
WINDOWS** — they set `X-Frame-Options: DENY` on their own login and refuse to be framed; only OUR
first-party `auth.zeroship.ai` password UI (whose headers we control) gains the iframe.

### 3.1 The full in-iframe navigation chain (this is the load-bearing invariant — see §6.3)

The whole flow below runs **inside the one `<iframe>`** as a chain of top-level-document navigations
within that frame. Every response that returns an HTML body (not just a 302) is a *framed document*
and is therefore subject to `X-Frame-Options` / CSP `frame-ancestors`. Verified against
`browser_auth.rs` (authorize), Hydra (`ops/hydra.yaml`), `login.rs`, `consent.rs`, and the gateway
callback:

```
iframe.src = ${appOrigin}/__zeroship/auth/authorize?…   [app origin; gateway BFF]
   │ 302 cross-site  (the ONE hop)
   ▼
auth.zeroship.ai/oauth2/auth?client_id&code_challenge&redirect_uri&state  [Hydra]
   │ 302 (no body) → issues login_challenge
   ▼
auth.zeroship.ai/login?login_challenge=…                 [auth GET — HTML BODY, framed doc]
   │  user types password HERE (SOP: console JS cannot read it)
   │  POST /login (verify_password_credentials, Argon2)
   │     ├─ on FAILURE → render_login_error(…)            [auth POST — HTML BODY, framed doc]
   │     └─ on success → accept_login amr=[pwd]
   ▼
auth.zeroship.ai/oauth2/auth?login_verifier=…            [Hydra]
   │ 302 (no body)
   ▼
auth.zeroship.ai/consent?consent_challenge=…             [auth — see below]
   │     ├─ console client has skip_consent=true → SILENT accept (302, no body)
   │     └─ (non-skip path) interactive consent render    [auth — HTML BODY, framed doc]
   │ 302 (no body) → accept_consent → Hydra → 302 ?code=…
   ▼
${appOrigin}/__zeroship/auth/popup-callback?code&state   [app origin — HTML relay, framed doc]
   │  inline relay script (nonce-CSP) postMessages {code,state} to window.parent
   ▼
console relay listener → POST /session → __Host- cookies set → SIGNED_IN
```

**Framed HTML documents** (need the relaxed `frame-ancestors`): the auth `/login` GET, the auth
`/login` POST failure re-render (`render_login_error`), the auth `/signup` GET + its POST error
re-render, the interactive `/consent` render (only reached on the non-skip path), and the app-origin
`popup-callback` (already framed by `'self'`). **Hydra's `/oauth2/auth` responses are 302s with no
body** in the happy path and are NOT separately framed documents — but **if Hydra ever renders an HTML
error/interstitial page on `/oauth2/auth`, that page is ALSO a framed document** and must carry the
relaxed `frame-ancestors` or the flow dead-ends inside the frame. This is a hard invariant, pinned in
§6.3.

### 3.2 Data-flow diagram (password path — iframe)

```
console.zeroship.ai (top window)                 auth.zeroship.ai (iframe doc)        Hydra
─────────────────────────────────                ──────────────────────────────      ─────
[user clicks "Sign in"]
  AuthClient.signInWithOAuth({provider:'password'})
    beginFlow(): PKCE verifier+challenge, state, nonce, stash txn
    listenForRelay(appOrigin, state)  ── window.addEventListener('message') on TOP window
    open in-page <iframe modal> created WITH src preset (§4.1 — never assign .href)
      src = ${appOrigin}/__zeroship/auth/authorize?code_challenge&state&nonce&scope&redirect_uri
                       │
        (gateway BFF)  ▼  302 cross-site (the ONE hop)
                  ── auth.zeroship.ai/oauth2/auth (Hydra) ──► /login  ◄── user types password HERE
                                                              (SOP: console JS CANNOT read it)
                                                                │ verify_password_credentials (Argon2)
                                                                │ accept_login amr=[pwd] → /consent
                                                                │ skip_consent (console) → silent accept
                                                                ▼  302 ?code=… to
                  ${appOrigin}/__zeroship/auth/popup-callback  (SAME-ORIGIN, inside the iframe)
                       │  inline relay script (nonce-CSP):
                       │    tgt = window.opener (popped) || window.parent (framed)
                       └── postMessage({type:'zs:authorization_response',response:{code,state}},
                                       targetOrigin = location.origin /* app origin, never '*' */)
  relay.onMessage: ev.origin === appOrigin ✓, envelope ✓, state ✓
    SDK tears down the iframe modal
    completeFlow() → exchangeCodeForSession()
      POST ${appOrigin}/__zeroship/auth/session  {grant_type:'authorization_code',code,code_verifier,redirect_uri}
        (gateway mint_session_from_code) → sets __Host-zeroship_app_session + __Host-zeroship_app_anchor
      ◄── {user, expires_at}   (NO token in the body)
    emit SIGNED_IN
```

The federated path is identical except: the provider is `google`/`github`, the SDK uses `window.open`
(not an iframe), the callback posts to `window.opener`, and there is no `frame-ancestors` relaxation
(the federated IdP's page is never framed).

---

## 4. Components & responsibilities (file by file)

All paths are under `/home/ruiyang/Projects/appbase/.worktrees/auth-immersive-popup`.

### 4.1 SDK — `@zeroship/auth` (`sdks/auth/`)

This is the central UX contract of the pivot; it is specified here in full (not deferred to Open
Questions). The deleted `signInWithCredentials` is replaced by routing the iframe through the existing
`signInWithOAuth` with `provider: 'password'`.

**Type changes (REQUIRED — the iframe entry point does not typecheck without them):**

- `sdks/auth/src/types.ts` — **re-add `'password'`** to the `SignInOptions.provider` union (146); it is
  currently `"google" | "github"` only and its doc comment (141-145) explicitly says password "is no
  longer a value here." Rewrite that comment: `'password'` selects our first-party login UI (iframe on
  the same-site console, popup elsewhere); `'google'`/`'github'` are federated popup providers. **Delete**
  the `CredentialsInput` interface (166-174) and the in-page-password prose on the `invalid_credentials`
  doc (78-82). **KEEP** the `invalid_credentials` union VALUE — `auth.zeroship.ai`'s `/login` still emits
  it and the relay surfaces it.
- `sdks/auth/src/types.ts` — add to `AuthClientOptions` (126-138) an explicit launcher input so the SDK
  can decide iframe-vs-popup **without** trying to infer same-site (which it cannot do from `appOrigin`
  alone — `appOrigin` is the console's OWN origin, and the cross-site hop to `auth.zeroship.ai` happens
  server-side in the gateway 302). Add **`authOrigin?: string`** (the auth-service origin, e.g.
  `https://auth.zeroship.ai`): the SDK selects the iframe ONLY when `eTLD+1(appOrigin) === eTLD+1(authOrigin)`
  AND the caller opted in. To keep it explicit and fail-safe, also add **`immersive?: boolean`** (default
  **`false`** → popup). The console build sets `immersive: true` + `authOrigin: 'https://auth.zeroship.ai'`;
  creator-app / custom-domain builds leave both unset (popup). A misconfigured or unknown surface thus
  fails to the working popup, exactly as §6.5 wants. (Decision in §10.3 — `immersive` boolean is the
  primary gate; `authOrigin` provides the same-site sanity check.)

**Reused verbatim (back BOTH the surviving federated popup AND the new iframe):**

- `sdks/auth/src/internal/transport.ts` — `authorizeUrl()` (123-146), `redirectUri()` (149-151) build
  the iframe `src` and the app-origin callback URI unchanged; `exchangeCode()` (179-205) is the
  `POST /__zeroship/auth/session` mint the iframe still drives; `session`/`sessionMint`/`signout` stay.
- `sdks/auth/src/internal/relay.ts` — the relay listener (84-183) and the message contract
  (`{type:'zs:authorization_response', response:{code,state}|{error,error_description,state}}`, 6-13,
  39-48). **No change needed:** the callback is served from the **app/console** origin, so the
  `ev.origin === expectedOrigin` check at 139 (where `expectedOrigin` is the app origin) already passes
  for the iframe leg. The `state` filter (113-118) and the BroadcastChannel/localStorage fallback
  channels (154-180) stay — they are same-origin by construction and serve both popup and iframe.
- `sdks/auth/src/client.ts` — `beginFlow` (233-260), `completeFlow` (263-293), `exchangeCodeForSession`
  (297-323) stay.

**Modified — the launcher branch in `signInWithOAuth` (`client.ts:160-207`):**

Today `signInWithOAuth` branches only on `opts.popup !== false`, and at line 182 it calls
`openPopup(this.env)` **synchronously and unconditionally** before the async URL build (to dodge popup
blockers). The new shape adds an iframe branch that must be chosen **before** that synchronous
`openPopup` call:

- Compute `useImmersive = (opts.provider === 'password') && this.immersive && sameSite(this.appOrigin, this.authOrigin)`.
- `useImmersive` → drive the iframe (`runIframe`, below). The iframe has NO popup-blocker constraint, so
  it does NOT need the synchronous-gesture dance: build the URL first, then create the iframe with the
  URL preset as `src`.
- `provider ∈ {google, github}` OR `!useImmersive` (popup mode) → the existing synchronous
  `openPopup` → `beginFlow` → `runPopup` path, verbatim.
- `opts.popup === false` → the existing full-page redirect path, verbatim.

**New (ADD — the iframe driver replacing `window.open` for our UI):**

- `sdks/auth/src/internal/iframe.ts` (new) — `createIframe(env)` / `runIframe(env, frame, url, relay)`,
  mirroring `internal/popup.ts` (24-30, 42-97). **Navigation contract:** an `<iframe>` to a cross-origin
  document CANNOT be navigated by assigning `iframe.contentWindow.location.href` (that throws
  `SecurityError` cross-origin — the bug `popup.location.href` would hit if copied). The driver instead
  **creates the iframe element with `src` PRE-SET to the authorize URL** (or assigns `iframe.src = url`
  on the element, which is allowed — it is the element attribute, not the contentWindow location). It then
  `Promise.race`s the relay against a 60s timeout, and on settle **removes the iframe element** (the iframe
  analogue of `popup.close()` at popup.ts:91). It has no `closed` poll (an iframe has no `.closed`); the
  modal's close button is the cancel signal (§8).
- `sdks/auth/src/internal/env.ts` — add an injectable iframe-creation handle to `ResolvedEnv` (88-104) so
  the iframe driver is unit-testable against fakes the same way `openPopup` is. `WindowLike.open` (16)
  stays for the federated popup. Add a same-site helper (eTLD+1 comparison) used by the launcher gate.
- `sdks/auth/src/react.tsx` — `AuthModal` is **restructured**: it hosts the cross-origin
  `auth.zeroship.ai` login iframe (driven by `signInWithOAuth({provider:'password'})` → `runIframe`)
  instead of an in-page `<SignInForm>`. It keeps its federated popup button(s). The modal must provide the
  close affordance (user-closes → reject the relay race; §8). `SignInButtonProps.provider` (react.tsx:360-361)
  already documents `google|github|password` and forwards it — that becomes correct once `'password'` is
  re-added to the `SignInOptions.provider` union (above).

**Deleted (the same-origin `/password` plumbing):**

- `transport.ts` — `Transport.passwordLogin` (207-237) and the in-page-credential-POST comment (75).
- `client.ts` — `AuthClient.signInWithCredentials` interface entry (56-62) + `AuthClientImpl.signInWithCredentials`
  (209-220); `CredentialsInput` import (32) + re-export (498).
- `react.tsx` — the `SignInForm` component + `SignInFormProps` (479-624); `signInWithCredentials` from
  `AuthContextValue` (66), the `useAuth` provider `useCallback` (266-269) and context-value entries
  (318, 326); the `CredentialsInput` import (38). `useAuth`/`AuthProvider`/`SignInButton` + the federated
  popup path SURVIVE.
- The `.css` `zs-auth-form` / `zs-auth-field` form selectors are dropped or repurposed as iframe-modal
  chrome.

### 4.2 Gateway popup-callback — dual-target postMessage (`crates/gateway/src/browser_auth.rs`)

**Reused, with a one-line target change.** `popup_callback()` (194-209) and `popup_callback_html(nonce)`
(218-244) stay. The single postMessage line (230) currently targets only `window.opener`:

```js
try { if (window.opener) window.opener.postMessage(msg, location.origin); } catch (e) {}
```

It must detect framed-vs-popped and target the right window — **`targetOrigin` stays `location.origin`,
never `'*'`**. It targets **exactly one** window: `window.opener` first (the popup leg, where opener is
the console top and `parent === self`), else `window.parent` (the iframe leg, where opener is null and
parent is the console top):

```js
var tgt = (window.opener && window.opener !== window) ? window.opener
        : (window.parent  && window.parent  !== window) ? window.parent : null;
try { if (tgt) tgt.postMessage(msg, location.origin); } catch (e) {}
```

- The opener-first ordering is intentional: the SDK uses exactly ONE launcher per flow (popup XOR iframe),
  so opener-and-parent are never both distinct in practice. If a future nesting made both distinct, only
  the opener is posted to — acceptable because `targetOrigin` is pinned to `location.origin`, so a
  wrong-context window of a foreign origin silently drops the message anyway.
- `location.origin` is the **app/console** origin (the callback is app-origin), so the parent relay's
  `ev.origin === appOrigin` check (relay.ts:139) passes; a wrong-context window of a different origin
  silently drops the message (the browser enforces `targetOrigin`).
- **The full-page redirect flow has neither a distinct opener nor a distinct parent** (`window.parent ===
  window`, `window.opener` null) → `tgt` is null → the postMessage is skipped, and the
  **BroadcastChannel + one-shot-localStorage fallbacks (233-239) carry the code.** These fallbacks need
  **no change** (origin-shared on the app origin; cover the popup, the iframe, AND the COOP-severed /
  redirect cases) and **must be retained verbatim**.
- `window.close()` (240) is a harmless no-op inside the iframe; keep it for the popup leg. The SDK tears
  down the iframe element when the relay settles.
- `popup_csp(nonce)` (56-58) `frame-ancestors 'self'` stays correct — the callback is on the app origin
  and is framed by the same console origin → `'self'` satisfies it.
- COOP `same-origin` on the callback (206) stays — it governs the callback leaf's own opener relationship,
  not the console top document (constraint 6 is about the console, §6.6).
- The existing regression test pinning `postMessage` to `location.origin` (never `'*'`,
  `browser_auth_test.rs` ~415) must be updated for the both-targets form (assert it posts to the resolved
  `window.opener || window.parent`, still pinned to `location.origin`, still never `'*'`).

**Deleted (the `POST /__zeroship/auth/password` section, ~487-716):** the `password` handler, the
`PasswordRequest` struct, `parse_password_request`, `parse_password_code`, `forward_auth_error`, the
`PASSWORD_LOGIN_SCOPE` const (49), the `PasswordLoginParams` import (43), and the first-party gate call
`is_trusted_client_id(&state.trusted_oauth_clients, …)` (547). **KEEP** `mint_session_from_code` +
`resolve_route` + `same_origin_guard` + `error_response` + `CACHE_NO_STORE` (shared with `/session` +
`/signout`); the `authorize`/`popup-callback`/`signout` handlers + their tests survive.

### 4.3 Auth-service security headers — route-aware `frame-ancestors` (`crates/auth/src/headers.rs`)

This is the pivot point (constraints 2 + 5). **Correcting the earlier draft:** the framed login GET
handlers currently emit **NO** CSP of their own — `login.rs` GET (131-134) and `signup.rs` GET (70-73)
return `HttpResponse::Ok()` with only content-type + a Set-Cookie; their `frame-ancestors 'none'` comes
**entirely from the global `SecurityHeaders` middleware default** (`DEFAULT_CONTENT_SECURITY_POLICY`,
headers.rs:24) via `static_set_if_absent` (95). Only the token interstitial and the interactive consent
render actually call `content_security_policy_with_script_nonce` (`ui/mod.rs:219-221`, `consent.rs:644`).
So "have the handler override" is **not** a one-line edit — it would mean adding a full CSP header to
handlers that emit none. The clean design is to make the middleware **route-aware** instead.

**Why route-aware (not handler-set/absent):** the middleware `apply()` runs **after** the handler
(`SecurityHeadersService::call` stamps `res.headers_mut()` at headers.rs:262-263) and sets
`x-frame-options` (79) and `cross-origin-opener-policy` (88) with **`static_set` (unconditional insert)**
— only CSP uses `static_set_if_absent` (95). So a handler CANNOT "suppress" XFO by leaving it absent: the
middleware unconditionally re-inserts `X-Frame-Options: DENY`, clobbering the handler. The
"handler-set/absent wins" trick is impossible for XFO (a header that must be DROPPED, not set). The
middleware itself must branch.

**The change (concrete):**

1. **Thread the request path + framed-route set into the middleware.** `apply()` currently takes only
   `&mut HeaderMap` and has no request handle. Change it to `apply(headers: &mut HeaderMap, req_path: &str,
   cfg: &FrameAncestorsCfg)` (or stash the path + cfg via request extensions, like `RequestContext` does at
   headers.rs:218). `SecurityHeadersService::call` (262-263) already has the `WebRequest`; thread the path
   in. **This is a non-trivial signature change, not a one-line override** — call it out in the impl PR.
2. **For the framed routes** — `/login`, `/signup`, the interactive `/consent` render (NOT the
   skip-consent silent-accept path, which emits no body) — and **on the POST error re-render paths**
   (`login.rs::render_login_error` at 358, the `signup.rs` POST error render): emit
   `Content-Security-Policy: …; frame-ancestors 'self' https://console.zeroship.ai; …` (built from a
   helper that takes the allowed ancestors) and **SKIP the `static_set(x-frame-options, DENY)` line
   entirely** for those paths. `'self'` keeps the auth origin's own pages framing each other; the explicit
   console origin is the single cross-site embedder allowed.
3. **For everything else** (all other routes, AND the `/oauth/google` federated bounce, AND Hydra
   `/oauth2/*` if they ever pass through this middleware): keep `X-Frame-Options: DENY` +
   `frame-ancestors 'none'` — the fail-closed default.

**Framed = navigable documents only.** `frame-ancestors` governs the framing of a top-level navigable
document; it has **no effect on subresources** like `/static/style.css` (served by `server.rs:212`),
which are pulled in via `<link>`/`<script>` and are never themselves framed documents (they are governed
by the framed document's `style-src`/`script-src`). **Do NOT relax `frame-ancestors` or XFO on static
CSS/JS** — they stay on the strict default. The framed-route set is exactly: auth `/login` (GET + POST
error re-render), auth `/signup` (GET + POST error re-render), the interactive `/consent` render, and (in
the gateway, already correct) the app-origin `popup-callback`.

**Hydra leg (§3.1 invariant).** Hydra's `/oauth2/auth` happy-path responses are body-less 302s and are
not separately framed documents. **But the design REQUIRES that no Hydra `/oauth2/auth` response — and no
fronting proxy — ever emits `X-Frame-Options` or a `frame-ancestors` that excludes the console**, or the
in-iframe navigation is killed mid-flow (60s timeout, no diagnosable error). Pin this with two assertions
(§9): (a) `ops/hydra.yaml` emits no framing headers on `/oauth2/*` (verified: it sets none today); (b)
Caddy / any proxy in front of `auth.zeroship.ai` does not inject a global `X-Frame-Options` (verified:
`ops/Caddyfile` adds none today). If Hydra is ever configured to render an HTML error/interstitial on
`/oauth2/auth`, that page is a framed document and must also carry the relaxed `frame-ancestors`.

**The console origin must be config-injected** (constraint 2). Core/auth has no console host, so add an
`AuthConfig` field — `frame_ancestor_origins: Vec<String>` — populated from deployment config (mirrors the
`trusted_oauth_clients` deployment-injection pattern; §10.1). Dev overrides it (or makes the relax a no-op;
§7).

**COOP / CORP unchanged:** `cross-origin-opener-policy: same-origin` (88) stays — the auth iframe is a
leaf; COOP governs popups it opens, not its own embedding. `cross-origin-resource-policy: same-origin`
(89) stays — CORP gates `no-cors` subresource embedding, NOT a top-level iframe navigation (governed by
`frame-ancestors`/XFO). Do NOT relax CORP.

### 4.4 Dev-tier — `sdks/bootstrap/src/dev-auth.ts` + the vite-plugin proxy

See §7 for the full dev-parity treatment. In brief:

- **Deleted:** the dev `passwordLogin` arm (503-545+), its route dispatch (456), the module-doc bullet
  (28-31). `authorize`/`popup-callback`/`session`/`signout` survive and are exactly what the dev iframe
  drives.
- **Modified:** `popupCallbackHtml()` (362-385, postMessage line 373) gets the SAME both-targets change
  as the gateway, so the dev iframe relay reaches `window.parent`.
- **`sdks/vite-plugin/src/dev-server.ts`** — KEEP the whole `/__zeroship/auth/` prefix proxy forward
  (~560); there is no per-path `/password` allowlist entry to delete. Only scrub any comment enumerating
  `/__zeroship/auth/password` among forwarded routes (~454, 542).

### 4.5 Config — what's removed vs retained

**Removed (the `auth_internal_key` shared secret + the gateway `trusted_oauth_clients` consumer):**

- `crates/auth/src/config.rs` — `AuthConfig.auth_internal_key` field + its
  `#[arg(long="internal-key", env="AUTH_INTERNAL_KEY")]` clap attr (~109-130) + the Debug-impl redaction
  line (~501). **Add** the new `frame_ancestor_origins: Vec<String>` field here (§4.3, §10.1).
- `crates/auth/src/main.rs` — the `require_unless_dev("AUTH_INTERNAL_KEY / --internal-key", …)` startup
  gate (~82-97) and the `obtain_secret(…file_secrets.auth_internal_key…)` resolution (~251-256). The
  `stash_signing_key` obtain/require directly above SURVIVES.
- `crates/gateway/src/lib.rs` — `GateConfig.auth_internal_key` field (~66-73) AND
  `GateState.trusted_oauth_clients` field (~211-217). The frame-ancestors CSP allowlist is the
  browser-enforced replacement for the deleted gateway gate.
- `crates/gateway/src/main.rs` — the `--auth-internal-key`/`AUTH_INTERNAL_KEY` clap arg (~136-145),
  `obtain_secret` (~346-352), `require_unless_dev` (~396-405), `auth_internal_key_configured`
  check-config report (~545-546), `GateConfig{…auth_internal_key}` init (~727); the
  `resolve_trusted_oauth_clients(&file.auth)` (~283) + the `trusted_oauth_clients` GateState init (~752);
  the `/__zeroship/auth/password` route mount (~810-820). The other secrets (control_key, oidc_secret,
  stash, pairwise) + the `{authorize,popup-callback,signout,session}` mounts + backchannel-logout SURVIVE.
- `crates/core/src/config/file.rs` — `SecretSection.auth_internal_key` (~89-92). `SecretSection` has
  `#[serde(deny_unknown_fields)]`, so any deployment TOML still carrying `[secrets].auth_internal_key`
  now fails to parse — **intended** (no-back-compat). The other `SecretSection` fields survive.
- `docker-compose.yml` — `AUTH_INTERNAL_KEY` env + comment on BOTH the gateway (~206-214) and auth
  (~335-343) services. `PAIRWISE_SALT`/`STASH_SIGNING_KEY`/`GATEWAY_OIDC_SECRET` next to it survive.

**Retained (the `trusted_oauth_clients` / `skip_consent` CONTROL consumer — load-bearing for the iframe):**

- `zeroship_core::auth::trusted_clients::{resolve_trusted_oauth_clients, is_trusted_client_id,
  default_trusted_oauth_clients}` — the CORE module SURVIVES (`crates/control` consumes it for the
  console `skip_consent` decision). Only the gateway-side consumer is removed.
- `crates/control/src/app_oauth_client.rs::ensure_app_client(first_party)` +
  `crates/control/src/bootstrap_console.rs::bootstrap_console(first_party=true)` +
  the `control.oauth_clients.skip_consent` mirror — SURVIVE: drive the console's `skip_consent=true` so
  the framed `/oauth2/auth → /login → /consent` dance auto-accepts identity consent (no consent screen
  for the platform's own console).
- `ops/zeroship.toml` + `ops/zeroship.example.toml` — `[auth].trusted_oauth_clients = ["oac_…"]` KEEPS
  the key+value (it still drives the console's silent consent via control). Only scrub the comments
  (~16-30 / ~28-40) that say the value is "admitted by the gateway's POST /__zeroship/auth/password
  first-party gate" / "trusted for the gateway's in-page password login" — that gate is deleted. Reframe
  the key as: **skip Hydra consent for the first-party console only.** Deleting the value would make the
  framed login render a consent screen for the console (and the interactive consent render is then a
  framed document that ALSO needs the relaxed `frame-ancestors`).

**Added (the new console-origin config):**

- `crates/auth/src/config.rs` `frame_ancestor_origins: Vec<String>` + a `--frame-ancestor-origin`
  (repeatable) / `FRAME_ANCESTOR_ORIGINS` clap input; `ops/zeroship.toml` + `ops/zeroship.example.toml`
  `[auth].frame_ancestor_origins = ["https://console.zeroship.ai"]`; `docker-compose.yml` auth service
  `FRAME_ANCESTOR_ORIGINS=https://console.zeroship.localhost` (or the dev no-op, §7). Full field/arg/env/
  resolution/compose/toml enumeration in §10.1.

**Why retain the trust set if the gateway gate is gone?** The same identifier serves two unrelated
purposes that were conflated in the old design. (1) Gateway: gate the credential oracle — **deleted**,
replaced by the browser-enforced `frame-ancestors` allowlist. (2) Control: tell Hydra the console client
has `skip_consent=true` so the framed login does not show a consent screen — **still needed**. Only (1)
goes.

### 4.6 Builder console (`apps/zeroship-builder/`)

- `apps/zeroship-builder/src/client/pages/Login.tsx` — replace the `<SignInForm>` embed (import 26,
  usage ~98) with the iframe-based `<AuthModal>` launcher; keep `SignInButton` (federated popup); scrub the
  header comments (1-8, 92-93) describing the same-origin `/__zeroship/auth/password` POST.
- `apps/zeroship-builder/src/client/pages/Signup.tsx` — same (import 22, usage ~93,
  `passwordTestId='signup-password'`); scrub header comments (1-7, 86-93).
- Clean up the now-dead `zs-auth-*` form selectors in `Login.css`/`Signup.css`.
- **E2E (three files, all break when `SignInForm` + its testids are removed — full inventory in §5):**
  `apps/zeroship-builder/e2e/auth.spec.ts`, `forms.spec.ts`, `a11y.spec.ts` all reference
  `login-password` / `signup-password` testids. The cross-origin iframe means Playwright **cannot fill
  those inputs directly** (in prod they live in the auth-origin frame; cross-origin frame contents are not
  Playwright-fillable on the parent page). The rewritten specs must either (a) drive the **dev-tier
  same-origin** framed login (where the form is same-origin and the picker/login is fillable in-frame), or
  (b) assert modal/iframe presence + the relay completion, rather than filling the old testids. This is a
  behavior change the e2e suite must reflect.

---

## 5. Removal list (exact files/symbols)

**Delete whole file (3):**

- `crates/auth/src/ui/password.rs` — the `POST /password` credential→code oracle handler
  (`ui::password::post`), `PasswordLoginRequest`, `check_internal_secret`, `credential_error_json`/
  `error_json`, all `secret_gate_*` tests. It imports `verify_password_credentials` (SHARED — do NOT
  delete that fn) and `mint_code_for_subject`/`MintCodeParams`/`HeadlessError` (deleted with
  `oauth/headless.rs`).
- `crates/auth/src/oauth/headless.rs` — `mint_code_for_subject` + `MintCodeParams` + `HeadlessError` +
  the private `CookieJar`/`point_at_hydra`/`extract_query_param`/`build_id_token_claims` helpers. Its
  `IDENTITY_SCOPES` is a private copy; the surviving `ui/consent.rs` has its own. Sole consumer was
  `ui/password.rs`.
- `crates/auth/src/oauth/mod.rs` — the module exists only to host `pub mod headless;`. With `headless.rs`
  gone it is empty; delete it and remove `pub mod oauth;` from `crates/auth/src/lib.rs` (15).

**Delete whole test file (1):**

- `crates/auth/tests/e2e_password_grant.rs` — the live e2e for the `/password` oracle. Do NOT confuse with
  `crates/auth/tests/e2e_password.rs` (`e2e_password_flow`), which tests the INTERACTIVE `/login → Hydra`
  dance with `skip_consent` and **SURVIVES** (it backs the iframe's login UI; it never references
  `/password`, `mint_code_for_subject`, `auth_internal_key`, or `verify_password_credentials`).

**Modify — auth crate module decls / routes / comments:**

- `crates/auth/src/lib.rs` — remove `pub mod oauth;` (15).
- `crates/auth/src/ui/mod.rs` — remove `pub mod password;` (23). (Other `password` mentions at 103/243/283
  are unrelated `has_password` doc/struct contexts — do NOT touch.)
- `crates/auth/src/server.rs` — remove the `web::resource("/password").route(web::post().to(ui::password::post))`
  registration (~49-51) + the preceding comment block (~45-49). `/login`, `/signup`, `/consent`,
  `/oauth/google`, `/device`, `/logout`, `/link` SURVIVE (they back the iframe login UI).
- `crates/auth/src/identity/credentials.rs` — KEEP `verify_password_credentials` / `CredentialError` /
  `VerifiedUser` (SHARED with `ui/login.rs`). Doc-only: scrub the rustdoc referencing the deleted
  `ui::password` headless endpoint (the "so the headless in-page login endpoint reuses…" / "or a hydra
  login acceptance (password.rs)" comments).
- `crates/auth/src/ui/login.rs` — comment-only: scrub the ~243 comment claiming the verify path "lives in
  identity::credentials so the headless /password endpoint can reuse it." The `verify_password_credentials`
  import/call STAY.

**Modify — auth security headers (NOT a deletion — the pivot, §4.3):**

- `crates/auth/src/headers.rs` — make `apply()` + `SecurityHeadersService::call` route-aware: framed routes
  emit `frame-ancestors 'self' <console origins>` and SKIP `X-Frame-Options`; everything else keeps
  `XFO: DENY` + `frame-ancestors 'none'`. Thread the request path + a new `frame_ancestor_origins` config
  into the middleware. New `AuthConfig` field for the console origin(s).

**Modify — auth threat-model regression test (the behavior change MUST carry its regression test):**

- `crates/auth/tests/threat_model.rs` — `login_clickjacking_headers_present` (140) currently hard-asserts
  `X-Frame-Options == DENY` AND CSP `frame-ancestors 'none'` on `GET /login`. After the pivot that
  assertion is FALSE and the test fails. **Rewrite it** to assert the NEW contract: `GET /login` emits
  `frame-ancestors 'self' https://console.zeroship.ai` (read from config) and **NO** `X-Frame-Options`
  header; and assert a non-framed auth route still emits `XFO: DENY` + `frame-ancestors 'none'`. This is
  the home of the "replaces the deleted gateway fail-closed gate" regression test. Audit the rest of the
  file for other framing assertions while there (`login_referrer_policy_set` at 183 etc. are unrelated —
  `referrer-policy` is unchanged — but confirm none of them re-assert XFO on `/login`).

**Remove symbols — gateway:**

- `crates/gateway/src/browser_auth.rs` — the `/password` section (~487-716): `password`, `PasswordRequest`,
  `parse_password_request`, `parse_password_code`, `forward_auth_error`, `PASSWORD_LOGIN_SCOPE` (49),
  `PasswordLoginParams` import (43), the `is_trusted_client_id` gate (547). KEEP `mint_session_from_code`
  import + the shared helpers + the `authorize`/`popup-callback`/`signout` handlers and tests. **Modify**
  `popup_callback_html` (218-244) for the both-targets postMessage (§4.2).
- `crates/gateway/src/oidc_rp.rs` — `OidcRp::password_login` (~537-608), `PasswordLoginParams` (~1000-1022),
  `PasswordLoginOutcome` (~1024-1032). KEEP `hydra_client::call` + breaker + `OidcRpError::from_hydra` +
  `exchange_code_public`/`refresh_token_public`/`finish_callback`/`introspect_token`/`revoke_token_public`.
- `crates/gateway/src/lib.rs` — `GateConfig.auth_internal_key` + `GateState.trusted_oauth_clients` (§4.5).
- `crates/gateway/src/main.rs` — the `auth_internal_key` CLI/obtain/require/report/struct-init,
  `trusted_oauth_clients` resolve+wiring, and the `/__zeroship/auth/password` route mount (§4.5). KEEP
  `obtain_secret`/`require_unless_dev` (used by other secrets).

**Modify — gateway test GateState constructors (drop the removed fields; mechanical, tied to lib.rs):**

- `crates/gateway/src/router/auth.rs` — `auth_internal_key: String::new()` (1649, 2626) +
  `trusted_oauth_clients: default_trusted_oauth_clients()` (1680, 2651).
- `crates/gateway/src/router/dispatch.rs` — `auth_internal_key` (1701) + `trusted_oauth_clients` (1731).
- `crates/gateway/src/router/static_serve.rs` — `auth_internal_key` (886) + `trusted_oauth_clients` (916).
- `crates/gateway/tests/auth_token_anchors_test.rs` — `auth_internal_key` (344).
- `crates/gateway/tests/backchannel_logout_test.rs` — `auth_internal_key` (344).
- `crates/gateway/tests/dpop_bound_e2e.rs` — `auth_internal_key` (240). (Leave the unrelated
  `skip_consent` Hydra-seeding reference.)
- `crates/gateway/tests/dpop_exchange_test.rs` — `auth_internal_key` (122).

**Modify — gateway security-regression test file (SHARED — surviving tests must keep compiling):**

- `crates/gateway/tests/browser_auth_test.rs` — delete the `// POST /__zeroship/auth/password` section
  (~651+): `password_first_party_gate_fails_closed_for_untrusted_client`, the same-origin-guard `/password`
  cases (~731-775), and any other `password_*` tests. Remove `StateOpts.trusted` + the
  `trusted_oauth_clients` insert/build (~100-154, 197), the `auth_internal_key:"test-internal-key"` init
  (~170), and the `/__zeroship/auth/password` route in the test app (~236-237). The `authorize_*`,
  `popup_callback_*`, `signout_*` tests (266-650) SURVIVE — edit `build_state`/`StateOpts`/`build_route_map`
  to drop the removed fields WITHOUT breaking them. **Update** the postMessage-`location.origin` assertion
  (~415, which forbids `'*'` at ~418) for the both-targets form. **Confirm** the existing authorize
  foreign-`redirect_uri` → 400 test survives the `StateOpts` field removals (it pins the §6.4 open-redirect
  guard). The deleted fail-closed gate test is replaced by the rewritten `threat_model.rs` framing test
  (above) — the browser-enforced `frame-ancestors` is the conceptual successor to the server gate.

**Remove config:**

- `crates/auth/src/config.rs` + `crates/auth/src/main.rs` — `auth_internal_key` (§4.5); ADD
  `frame_ancestor_origins`.
- `crates/core/src/config/file.rs` — `SecretSection.auth_internal_key` (§4.5).
- `docker-compose.yml` — `AUTH_INTERNAL_KEY` on both services; ADD `FRAME_ANCESTOR_ORIGINS` on auth (§4.5).

**Modify config comments (keep the values):**

- `ops/zeroship.toml` + `ops/zeroship.example.toml` — keep `trusted_oauth_clients`; reframe the comment as
  skip-consent-for-the-console only (§4.5). ADD `frame_ancestor_origins` (§10.1).

**Remove symbols / tests — SDK + dev-tier:**

- `sdks/auth/src/internal/transport.ts` — `Transport.passwordLogin` (207-237) + the credential-POST comment.
- `sdks/auth/src/client.ts` — `signInWithCredentials` (interface 56-62 + impl 209-220) + `CredentialsInput`
  import (32)/re-export (498). MODIFY `signInWithOAuth` (160-207) to add the iframe branch (§4.1).
- `sdks/auth/src/react.tsx` — `SignInForm`/`SignInFormProps` (479-624), `signInWithCredentials` context
  wiring (66, 266-269, 318, 326), `CredentialsInput` import (38); restructure `AuthModal` to host the iframe.
- `sdks/auth/src/types.ts` — delete `CredentialsInput` (166-174) + the in-page-password prose (78-82); MODIFY
  `SignInOptions.provider` to re-add `'password'` (143-146); ADD `AuthClientOptions.immersive` +
  `authOrigin` (126-138). KEEP the `invalid_credentials` union value.
- `sdks/auth/tests/client.test.ts` — the two `signInWithCredentials` cases (~216-260) + the
  `/__zeroship/auth/password` fetch-stub matcher. ADD the iframe-driver cases (§9).
- `sdks/auth/tests/react.test.tsx` — the `signInWithCredentials — in-page password sign-in` describe block
  (~474-560+), the `signInWithCredentials` entry on the fake client (68, 97, 133-134) + its rejection knob
  (85).
- `sdks/bootstrap/src/dev-auth.ts` — `passwordLogin` (503-545+) + route dispatch (456) + module-doc bullet
  (28-31). MODIFY `popupCallbackHtml` (362-385, line 373) for both-targets.
- `sdks/bootstrap/tests/dev-auth.test.ts` — the password-login test (~133-167). ADD the both-targets
  parity assertion (§9).

**Modify — builder + dev-server (§4.4, §4.6):**

- `apps/zeroship-builder/src/client/pages/Login.tsx`, `Signup.tsx` — swap `<SignInForm>` for the iframe
  launcher; scrub comments; clean dead CSS.
- `apps/zeroship-builder/e2e/auth.spec.ts`, `forms.spec.ts`, `a11y.spec.ts` — rewrite to drive the dev-tier
  framed login / assert iframe-modal presence instead of filling `login-password`/`signup-password` (§4.6).
- `sdks/vite-plugin/src/dev-server.ts` — comment-only scrub; KEEP the `/__zeroship/auth/` prefix forward.

**Modify — docs:**

- `docs/reference/auth-dev-tier.md` — remove the `POST /__zeroship/auth/password` dev-contract entry and
  document the framed same-origin dev-login parity story from §7 instead.
- This spec — canonical record of the pivot.

**Shared symbols that MUST be KEPT (do NOT delete — they look password-only but are not):**

- `crates/gateway/src/auth_token.rs::mint_session_from_code` + `{resolve_route, same_origin_guard,
  error_response, db_error, CACHE_NO_STORE}` — the surviving `/session` + `/signout` + `/authorize` +
  `/popup-callback` use them.
- `crates/auth/src/identity/credentials.rs::verify_password_credentials` (+ `CredentialError`,
  `VerifiedUser`) and `crates/auth/src/identity/password.rs::{verify, dummy_hash}` — back the interactive
  `/login` the iframe frames.
- `crates/auth/src/ui/consent.rs::get_consent` + `IDENTITY_SCOPES` + the `skip_consent` silent-accept fast
  path — LOAD-BEARING: the framed dance must NOT show a consent screen for the console, which requires the
  console client's `skip_consent=true`.
- `zeroship_core::auth::trusted_clients::*` + `crates/control` `ensure_app_client(first_party)` /
  `bootstrap_console` / the `skip_consent` mirror + the `ops/*.toml` `trusted_oauth_clients` value — the
  CONTROL/skip-consent consumer survives; only the gateway gate goes.
- `crates/auth/tests/e2e_password.rs::e2e_password_flow` (interactive) survives; only
  `e2e_password_grant.rs` (oracle) is deleted.
- `sdks/auth`: `signInWithOAuth`/`exchangeCodeForSession`/`Transport.{exchangeCode,session,sessionMint,
  signout,authorizeUrl,redirectUri}`/`openPopup`/`listenForRelay`/`runPopup` — back BOTH the surviving
  federated popup AND the new iframe.
- `sdks/vite-plugin/src/dev-server.ts` `/__zeroship/auth/` prefix proxy forward — load-bearing for the dev
  iframe.

---

## 6. Security model

### 6.1 Credential isolation (SOP) and same-site first-party cookies

**Isolation.** The password is typed into `<iframe src="https://auth.zeroship.ai/login?…">`. `auth.` and
`console.` are different ORIGINS → the SOP forbids console JS from reading the framed document:
`iframe.contentDocument`, `iframe.contentWindow.document`, reading the framed `<input>.value`, attaching
listeners inside the frame, or reading its `location.href` (beyond the opaque `WindowProxy`) all throw
`SecurityError`. Keystrokes are delivered by the browser to the auth-origin document; they never enter the
console's JS realm. This is **strictly stronger** than the deleted `/password` model, which had console JS
hold `{email,password}` and needed the gate + `auth_internal_key` to contain it. The console only ever sees
the OAuth `code` relayed by the app-origin callback — never the credential.

**Same-site → first-party cookies (the Firebase escape).** The framed login depends on cookies set by
`auth.zeroship.ai`: the Hydra login/consent session, the auth CSRF cookie, the login-challenge session.
Because `console.` and `auth.` share eTLD+1 `zeroship.ai`, the iframe is **same-site**; browser storage
isolation (Chrome CHIPS / partitioning, Safari ITP, Firefox TCP) keys the "third-party?" decision on
**site vs the top-level site** — here both are `zeroship.ai`, so the iframe is a **first-party** context:
cookies are not partitioned, not blocked. This is exactly the trap that forced Firebase off its cross-site
auth iframe; the console has the escape for free.

**Cookie-attribute audit (no SameSite value needs changing):**

| Cookie | Where set | Attrs | Verdict |
| --- | --- | --- | --- |
| `__Host-zsidp_session` | `crates/auth/src/sessions/login.rs:34` | HttpOnly; SameSite=Lax | fine in same-site iframe |
| `__Host-zsidp_csrf` | `crates/auth/src/csrf.rs:53` | **SameSite=Strict** | see the dedicated invariant below |
| `__Host-zsidp_pkce` / `__Host-zsidp_nonce` | `crates/auth/src/handlers.rs:57,64` | HttpOnly; SameSite=Lax | fine |
| oauth-stash | `crates/auth/src/ui/oauth_stash.rs:167` | HttpOnly; SameSite=Lax | fine |
| magic-link | `crates/auth/src/ui/magic.rs:92` | HttpOnly; SameSite=Lax | fine |
| `__Host-zeroship_app_session` | `crates/gateway/src/oidc_rp.rs:1136` | HttpOnly; SameSite=Lax | app-origin first-party; set by the parent's `POST /session`; unchanged |
| `__Host-zeroship_app_anchor` | `crates/gateway/src/anchors.rs:97` | HttpOnly; **SameSite=Strict** | read only by same-origin `?mint=1` fetch from console JS; Strict satisfied; unchanged |
| breadcrumb | `crates/gateway/src/anchors.rs:142` | SameSite=Lax | fine |

**The single most fragile invariant in the design — make it explicit (not a footnote).** The
`__Host-zsidp_csrf` cookie is SET on the auth `/login` GET response and READ on the auth `/login` POST.
Both the GET that sets it and the POST that reads it occur inside a cross-origin iframe whose **top-level
site is `console.zeroship.ai`**. `SameSite=Strict` means "omit on cross-**SITE** requests"; the framed
GET→POST is cross-**origin but same-site** (top-level site == cookie site == `zeroship.ai`), so the
double-submit cookie rides and the CSRF check passes. **This holds ONLY because console and auth are
same-site.** The moment that breaks — a custom-domain console, or fronting `auth` on a non-same-site host —
the Strict CSRF cookie is silently dropped and the framed login fails closed with an opaque CSRF error
(`render_login_error` "invalid request", 400), NOT a clear "use the popup" signal. This is precisely why
the iframe path is gated to the same-site console (§6.5): a non-same-site surface must never enter the
framed path. Pin both directions in the live e2e (§9): (a) the Strict CSRF cookie rides the framed GET→POST
on the console; (b) a cross-site embedder attempting the framed POST gets a CSRF failure — proving the gate
matters. Same logic applies to `__Host-zeroship_app_anchor` `Strict` (read only same-origin).

### 6.2 Clickjacking — `frame-ancestors` is the gate

The anti-phishing gate **moves from the gateway** (`is_trusted_client_id` / `trusted_oauth_clients`) **to
the auth-service CSP** (constraints 2 + 5). The auth login routes emit
`frame-ancestors 'self' https://console.zeroship.ai`, making the **browser** the enforcer: only the console
may frame `/login`. A creator app at `https://shop.zeroship.ai` or an external `https://phish.example`
that tries `<iframe src="https://auth.zeroship.ai/login">` is blocked before the document renders. This is
strictly better than the deleted server gate — it is enforced client-side, per-frame, with no request path
to misconfigure.

**Exactly ONE first-party origin in the allowlist (plus `'self'`).** Every origin listed is one we trust
not to overlay/clickjack the login. The console is platform-owned and CSP-hardened; creator origins are not
(a creator could lay a transparent overlay over the field). **No wildcards** — `https://*.zeroship.ai` would
re-admit every creator app and defeat the property. Name the exact origin(s). (We are tighter than Stripe,
which accepts arbitrary merchant embedders.)

### 6.3 `X-Frame-Options` vs `frame-ancestors` precedence (and why the middleware must change)

Per CSP Level 2 / Fetch, a browser that supports CSP `frame-ancestors` must **ignore** `X-Frame-Options`.
So leaving `XFO: DENY` on a framed route is harmless on modern browsers but a latent bug — a legacy/edge UA
honoring XFO would refuse the frame the CSP allows (the exact bug a naive implementation hits, because the
global middleware sets `XFO: DENY` unconditionally via `static_set`). **Rule:** framed routes emit
`frame-ancestors` and **DROP** `X-Frame-Options`; everything else keeps `XFO: DENY` + `frame-ancestors
'none'`. One source of truth. Because the middleware sets XFO with `static_set` (unconditional) and runs
after the handler, the **only** way to drop XFO on a framed route is to make the middleware route-aware
(§4.3) — a handler cannot suppress it.

**The framed navigation chain is the load-bearing invariant (§3.1).** Every HTML document loaded inside the
iframe — auth `/login` GET + POST error re-render, `/signup` GET + POST error re-render, the interactive
`/consent` render, the app-origin `popup-callback`, AND any Hydra `/oauth2/auth` HTML error/interstitial —
must carry a `frame-ancestors` that admits the console, or the navigation is killed mid-flow and the relay
never fires (60s timeout). The happy-path Hydra `/oauth2/auth` responses are body-less 302s (not framed
documents), but the invariant is: **no response in the chain may emit `X-Frame-Options` or a
`frame-ancestors` that excludes the console.** Assert in §9 that `ops/hydra.yaml` and `ops/Caddyfile` inject
no global framing headers on `auth.zeroship.ai`.

### 6.4 postMessage threat model

We postMessage **only `{code, state}`** (or `{error, error_description, state}`) — never tokens, never the
session, never the credential (envelope: relay.ts:7, browser_auth.rs:225). A leaked bare `code` is useless:
it is single-use, short-lived, **PKCE-bound** (S256 verifier held by the gateway, never the browser) and
**redirect_uri-bound** (Hydra binds it to the app-origin callback) and **client-bound** (the per-app
public client). The gateway holds the verifier (`mint_session_from_code`); a token would be a bearer secret
and must never cross postMessage.

**The authorize `redirect_uri` same-origin guard is load-bearing for the iframe.** The authorize endpoint
(`browser_auth.rs:119-133`) rejects any supplied `redirect_uri` that does not start with the app's own
origin (`scheme://route.host/`) with a 400 `invalid_request`, and defaults to the app-origin
popup-callback. So even a **tampered iframe `src`** cannot steer the in-iframe code to a foreign callback —
the code ALWAYS returns to the app-origin callback, never off-origin. Keep the regression test that a
foreign `redirect_uri` to `/authorize` yields 400 (it survives the `StateOpts` field removals; §5).

**Origin checks — BOTH ends:**

- **Sender (callback page):** runs on the app/console origin; posts with `targetOrigin = location.origin`
  (its own app origin) — **never `'*'`** (browser_auth.rs:230, pinned by an existing test). It targets
  **exactly one** window — `window.opener` if present-and-distinct (popup leg), else `window.parent` if
  present-and-distinct (iframe leg), else **none** (full-page redirect: parent===self, opener null). The
  `targetOrigin` pin is what makes a wrong-context target safe: a window of a foreign origin silently drops
  the message. **The redirect / full-page flow (and the COOP-severed popup) rely on the BroadcastChannel +
  localStorage same-origin fallback channels** (browser_auth.rs:233-239 / relay.ts:154-180) to carry the
  code — those fallbacks must be retained verbatim. Never reflect query params into the DOM (the only server
  interpolation is the CSP nonce — pinned by the existing XSS test).
- **Receiver (console SDK relay):** accept `zs:authorization_response` ONLY when `ev.origin ===
  expectedOrigin` (the app/console origin = the receiver's own origin → strongest form), relay.ts:139.
  `asEnvelope` (56) rejects non-envelope messages BEFORE the origin check (third-party postMessage traffic
  can't tear the relay down; a well-formed envelope from the wrong origin surfaces as `config_error`).
  `state` matching (`settleIfMine`, 113) prevents cross-delivery on the shared channels.

**Origin-spoof analysis:** `ev.origin` is browser-set, not attacker-controllable → the receiver check is
sound. An attacker can't receive the code (the sender pins `targetOrigin`). The auth-origin iframe is NOT a
postMessage participant for the code — the code flows auth → (302 to app-origin) callback → console via the
same-origin relay. There is no auth→console cross-origin secret channel to harden.

### 6.5 Anti-phishing rule — the iframe is **same-site-console-only**

The redirect/popup model exists so creator apps can never (a) read the platform credential nor (b) present
a pixel-perfect platform-login form they control. The pivot preserves both: (a) SOP (any embedder types
into the auth origin, can't read it); (b) `frame-ancestors` (only the console may frame `/login`; everyone
else is browser-blocked and keeps the popup; federated providers stay a popup for everyone — constraint 1).

**Hard rule:** the immersive iframe is enabled **only** when the top origin is **same-site** with
`auth.zeroship.ai` (eTLD+1 == `zeroship.ai`) — i.e., exactly `https://console.zeroship.ai`. A
custom-domain console on `console.acme.com` is **cross-site** → its iframe cookies (including the Strict
CSRF cookie, §6.1) would be partitioned/blocked → it must use the popup. The popup-vs-iframe selector keys
on **"is this our first-party password UI on the same-site console"** (→ iframe) vs **"any federated
provider, or any non-same-site surface"** (→ popup) — NOT on `google` specifically. Enforced in two
genuinely independent places (defense in depth):

1. **Browser-enforced (the load-bearing control):** the auth-service `frame-ancestors` allowlist. This is
   the control that actually stops a hostile embedder.
2. **SDK-enforced (graceful fallback, not a security boundary):** the `@zeroship/auth` client picks
   iframe-mode only when `immersive === true` AND `eTLD+1(appOrigin) === eTLD+1(authOrigin)` (the explicit
   `authOrigin`/`immersive` inputs from §4.1 — the SDK CANNOT infer this from `appOrigin` alone, which is
   why those inputs exist). Default `immersive=false` → popup. A misconfigured / unknown surface therefore
   fails gracefully to a working popup rather than a broken frame. This is a UX safeguard; the browser
   `frame-ancestors` is the actual security gate.

### 6.6 COOP / COEP — the console must NOT be cross-origin-isolated

Cross-origin isolation is enabled by sending, on the **top-level (console) document**, `COOP: same-origin`
+ `COEP: require-corp` (or `credentialless`). COEP `require-corp` requires every embedded subresource —
including iframes — to opt in via CORP/COEP. The auth login document does not (and should not) opt the
console in, so an isolated console would have the browser **refuse to load the auth iframe** (the exact
Stripe `stripe-js#634` failure). **Therefore the console must NOT set COEP and must NOT be
cross-origin-isolated.**

**Verified, and the resolution is "change nothing on the console":** the gateway sets **no** COOP/COEP/XFO
on app/console responses today (it sets only `frame-ancestors 'self'` on the popup-callback). With no COOP
header, the console's effective COOP is `unsafe-none`, which keeps `window.opener` intact for the federated
popup. **So nothing needs to change on the console** — do NOT add any COOP/COEP header. In particular do
NOT add `COOP: same-origin` (it would sever `window.opener` for the federated popup). The earlier draft's
"recommend `same-origin-allow-popups`" is **dropped** — it is unnecessary (the default already preserves
the opener) and risks an implementer mistakenly adding plain `same-origin`. The relay's BroadcastChannel +
localStorage fallbacks are retained regardless (they cover any future COEP-credentialless / severed-opener
case). The auth service keeps `COOP: same-origin` on its OWN responses (headers.rs:88) — that hardens the
auth origin's own windows and does not interfere with being framed (COOP governs popups, not embedding).

**Documented trade:** a future `SharedArrayBuffer`/`crossOriginIsolated` need on the console is mutually
exclusive with the immersive iframe.

### 6.7 Residual clickjacking risk + mitigations

- **A compromised console origin** is the residual: XSS on `console.zeroship.ai` could overlay/redress the
  framed login (SOP still prevents *reading* keystrokes, but an overlay could redress clicks or
  social-engineer). Mitigations: the console's strict CSP (`script-src 'self'` + nonce, no
  `unsafe-inline`); no untrusted third-party script; SRI on any pinned third-party `<script>`/`<link>`.
  **Accepted explicitly:** a compromised top origin can clickjack any of its own iframes — inherent to all
  framed-widget designs (Stripe included), bounded by keeping the embedder set to one hardened origin.
- **No framebusting.** `frame-ancestors` is a declarative, un-bypassable gate; when correct, no disallowed
  ancestor can frame the page. Legacy framebusting (`if (top!==self)…`) is defeatable AND would break the
  legitimate console embed (the login is *supposed* to be framed). Do not add it.

### 6.8 Net security delta

**Removed (net improvement):** app-origin credential handling (SOP isolation replaces it); a scriptable
credential→code oracle (`auth /password`); a standing shared secret (`auth_internal_key`, provisioned to
both services, must be rotated, catastrophic if leaked); the first-party gate as a credential containment
(replaced by the stronger browser-enforced `frame-ancestors`).

**Added:** a relaxed `frame-ancestors` on the auth login routes (residual: clickjacking by a compromised
console — §6.7); an auth login page that renders framed (no new cross-origin secret channel — the code
still returns via the app-origin callback); the console forgoes cross-origin isolation (§6.6).

**Lost:** `crossOriginIsolated` on the console (a capability trade, not a vuln); the iframe path is bounded
to same-site-console (custom-domain consoles fall to the popup — deliberate scope limit, not a regression).

**Verdict:** net security improvement — deletes a high-value scriptable credential oracle, a standing
shared secret, and all app-origin credential handling; replaces a server-side trust gate with a stronger
browser-enforced one; the only added surface (one-origin `frame-ancestors` + no COEP on the console) is the
Stripe trade-off, bounded to a single hardened embedder.

---

## 7. Dev-tier parity (`pnpm dev`)

There is **no separate `auth.zeroship.ai` origin in dev.** The dev-auth provider
(`sdks/bootstrap/src/dev-auth.ts`) IS the auth service, served **same-origin** through the
`@zeroship/vite-plugin` dev-server proxy (`sdks/vite-plugin/src/dev-server.ts:551-591`, which forwards the
whole `/__zeroship/auth/*` prefix to the spawned `zeroship serve` child runtime — this was the Part-1
"session mint request failed" fix). Isolation is **N/A** in dev (one localhost origin; nothing to isolate),
but the **UX/flow must be identical**:

- The SDK opens the in-page iframe with the same `/__zeroship/auth/authorize?…` `src`. In dev that URL is
  **same-origin** (localhost). The dev `authorize` handler (dev-auth.ts:462-484) has **two documented
  paths**, both ending at the in-frame callback:
  - **Single-user / default-user (frictionless):** no `dev_user` param and ≤1 configured user → 302
    straight to the same-origin `/__zeroship/auth/popup-callback` with a dev code + the original `state`
    (no picker, no second step). The 302 lands on the callback inside the iframe.
  - **Multi-user picker (two-step):** `config.users.length > 1` AND no `dev_user` param → render
    `userPickerHtml` (dev-auth.ts:469-477) **inside the iframe**; its `<form action="/__zeroship/auth/authorize">`
    re-submits within the frame WITH the chosen `dev_user`, which then takes the frictionless 302 path to
    the in-frame callback. (Document this two-step so the iframe e2e expects the picker render when >1 dev
    user is configured.)
- **`popupCallbackHtml()` (dev-auth.ts:362-385, postMessage line 373) gets the SAME both-targets change as
  the gateway** (§4.2): post to `window.opener` if present-and-distinct, else `window.parent`. This keeps
  the "byte-identical to the gateway relay" invariant the module doc (21-23) promises.
- The dev provider sets no XFO/`frame-ancestors` on its login/picker/authorize responses (same-origin) →
  there is **no dev header relax** to do; the framing relax is prod-only (`auth.zeroship.ai`). The
  picker/authorize responses set only `content-type` + `cache-control` and are already frameable
  same-origin. The new `frame_ancestor_origins` config is a no-op in dev (or set to the dev console origin
  for symmetry).
- **Deleted in dev:** the `passwordLogin` arm + its route dispatch + module-doc bullet (§5). The
  `authorize`/`popup-callback`/`session`/`signout` survivors are exactly what the dev iframe drives.
- `dev-server.ts` keeps the `/__zeroship/auth/` prefix forward (no per-path `/password` entry exists to
  remove); comment-only scrub.
- `docs/reference/auth-dev-tier.md` drops the `/password` contract entry and documents this framed
  same-origin dev-login parity story (§5).

---

## 8. Error handling & edge cases

- **Iframe load failure / network error.** `runIframe` (the iframe driver) `Promise.race`s the relay
  against a 60s timeout (mirroring `runPopup`); on timeout it rejects with the SDK's typed timeout error
  and tears down the iframe. The modal surfaces a retry affordance.
- **`frame-ancestors` block (wrong embedder).** If a non-console origin somehow loads the iframe, the
  browser refuses to render the auth document; no relay ever fires → the 60s timeout fires. The SDK's
  same-site gate (§6.5) means this should never happen for a correctly configured surface — and a
  misconfigured surface falls back to popup before attempting the frame.
- **User closes the modal.** The `AuthModal` close affordance cancels the relay race (rejects with a typed
  `user_cancelled`/`popup_closed`-equivalent), tears down the iframe and the `message` listener. (Unlike a
  popup, an iframe has no `closed` event; the modal's own close button is the signal.)
- **Wrong password inside the frame.** The auth `/login` POST re-renders `render_login_error` (a framed
  HTML document that MUST carry the relaxed `frame-ancestors`, §4.3/§6.3) so the retry renders inside the
  iframe rather than dead-ending. The user retries in-frame; on success the dance continues to the callback.
- **Code-exchange failure (`POST /session`).** The surviving `exchangeCode` path maps the gateway error to
  a typed `AuthError`; `completeFlow` surfaces it. `invalid_credentials` (when the framed `/login` rejects
  the password) is surfaced via the relay's `{error, error_description, state}` envelope, mapped through
  the kept `invalid_credentials` union value.
- **Federated (google/github).** Always a popup window (federated IdP `XFO: DENY`); the callback posts to
  `window.opener`; COOP fallback channels cover a severed opener.
- **Custom-domain console / creator apps.** Cross-site → SDK picks popup (§6.5); identical completion path,
  just windowed. (The Strict CSRF cookie that would be dropped in a framed cross-site flow is never reached,
  because the popup top-level navigation is first-party to `auth`.)
- **COOP-severed parent (popup edge).** The BroadcastChannel + localStorage relay channels (same-origin)
  carry the code when `window.opener`/`window.parent` is severed mid-flow.

---

## 9. Testing strategy

Every behavior change carries a regression test (project mandate — one that would fail pre-fix).

**Unit / offline:**

- **SDK iframe driver (new):** `sdks/auth/tests/` — assert `signInWithOAuth({provider:'password'})` with
  `immersive:true` + a same-site `authOrigin` opens an iframe (the injected `createIframe` handle, not
  `window.open`), sets the iframe `src` to the `authorizeUrl` (never assigns `contentWindow.location`),
  resolves on a relay `{code,state}`, and tears down the iframe; assert it falls back to a popup when
  `immersive` is false OR `authOrigin` is cross-site. Replaces the deleted `signInWithCredentials` cases in
  `client.test.ts` / `react.test.tsx`. Type-check: `provider:'password'` compiles (the union re-add).
- **Relay (existing, unchanged):** the `ev.origin`/`asEnvelope`/`state` tests stay green (no relay change).
- **Gateway callback dual-target (modified):** `crates/gateway/tests/browser_auth_test.rs` — update the
  postMessage-targets-`location.origin` assertion (~415) for the both-targets form (`window.opener` ||
  `window.parent`, never `'*'`); keep the no-DOM-reflection + nonce-only-interpolation invariants; confirm
  the authorize foreign-`redirect_uri` → 400 test survives the `StateOpts` field removals. The surviving
  `authorize_*`/`popup_callback_*`/`signout_*` tests must still compile after the `StateOpts`/`build_state`
  field removals.
- **Auth headers `frame-ancestors` (REWRITE of the existing test, replaces the deleted gateway fail-closed
  gate test):** rewrite `crates/auth/tests/threat_model.rs::login_clickjacking_headers_present` to assert
  (a) `/login` (and `/signup`, and the interactive `/consent` render) emit
  `frame-ancestors 'self' https://console.zeroship.ai` (read from `frame_ancestor_origins` config, not
  hard-coded) and **NO** `X-Frame-Options`; (b) every other auth route keeps `X-Frame-Options: DENY` +
  `frame-ancestors 'none'`; (c) `/oauth/google` is NOT in the framed-routes set. **Also add a pure-helper
  unit test** for the route-aware header builder (a function that, given a path + the configured origins,
  returns the header set) so the framing logic is testable WITHOUT booting Hydra (the live `/login` GET
  needs Hydra; the helper does not) — the design feasibility hinge from the critique.
- **Dev-auth (modified):** delete the password-login test; the `authorize`/`popup-callback`/`session`/
  `signout` dev tests stay green; assert the `popupCallbackHtml` both-targets change (parity with the
  gateway relay); assert the multi-user `authorize` renders the picker and the single-user `authorize`
  302s straight to the callback (§7 two-step).
- **Config:** assert that a TOML carrying `[secrets].auth_internal_key` now fails to parse
  (`deny_unknown_fields`) — the no-back-compat intent, as a guardrail test; assert `frame_ancestor_origins`
  parses from `[auth]`.

**Faithful real-browser e2e (must run the REAL path — live runtime + dispatcher + real Hydra/Postgres; no
shims):**

- **Env-gated live integration** (`AUTH_DB_URL` + `HYDRA_ADMIN_URL` + the full-stack docker-compose: Caddy
  + auth + hydra + gateway + control-seeded console worker): an iframe-framed `/login` on the console
  completes — the framed `/oauth2/auth → /login → /consent (skip_consent silent) → ?code=` dance ends in a
  302 to the app-origin callback, the callback posts `{code,state}` to `window.parent`, the console SDK
  drives `POST /session`, and the `__Host-zeroship_app_session` + `__Host-zeroship_app_anchor` cookies are
  set with **no new window**. Assert the `__Host-zsidp_csrf` (`Strict`) cookie rides the framed GET→POST
  (§6.1). Add the negative assertion: a cross-site embedder attempting the framed POST gets a CSRF failure
  (proves the §6.5 gate matters). The interactive `crates/auth/tests/e2e_password.rs::e2e_password_flow`
  survives and continues to pin the `/login → Hydra` dance the iframe frames.
- **Hydra/proxy framing-header guard:** assert (a) `ops/hydra.yaml` emits no `X-Frame-Options` /
  `frame-ancestors` on `/oauth2/*`; (b) `ops/Caddyfile` injects no global framing header on
  `auth.zeroship.ai` (§3.1/§6.3 invariant) — a static config-assertion test, offline-doable.
- **Browser-enforced framing (human-run / Playwright):** assert that `auth.zeroship.ai/login` framed by the
  console origin renders, and framed by a creator-app origin (or external) is blocked by the browser
  (`frame-ancestors`). Assert the console is NOT `crossOriginIsolated` (the iframe loads). Assert SOP:
  console JS cannot read the framed `<input>.value`. The rewritten builder e2e (`auth.spec.ts`,
  `forms.spec.ts`, `a11y.spec.ts`) drive the **dev-tier same-origin** framed login (fillable in-frame) or
  assert iframe-modal presence — they can no longer fill the old cross-origin `login-password`/
  `signup-password` testids (§4.6).

**Human-run only:** the full-stack browser e2e (whole platform up) and the cross-origin-block visual
confirmation are not offline-doable; the offline gates above (SDK iframe driver, gateway dual-target,
rewritten + helper auth `frame-ancestors` tests, dev-auth parity, config guardrail, Hydra/proxy
framing-header assertions) cover the deterministic surface.

---

## 10. Open questions / decisions

1. **Console origin config field (DECIDED + enumerated).** `crates/auth/src/headers.rs` needs the console
   origin(s) for `frame-ancestors`. **Decision:** a deployment-injected `AuthConfig.frame_ancestor_origins:
   Vec<String>` (a one-entry vector in prod; the `Vec` future-proofs staging/preview origins). Full edit
   inventory:
   - **Field:** `crates/auth/src/config.rs` `pub frame_ancestor_origins: Vec<String>`.
   - **Clap arg:** `#[arg(long = "frame-ancestor-origin", env = "FRAME_ANCESTOR_ORIGINS",
     value_delimiter = ',')]` (repeatable / comma-split).
   - **Env:** `FRAME_ANCESTOR_ORIGINS=https://console.zeroship.ai`.
   - **Resolution:** read in `crates/auth/src/main.rs` alongside the other `AuthConfig` fields; in dev it
     may be empty (relax is a no-op) or set to the dev console origin.
   - **Compose:** `docker-compose.yml` auth service env
     `FRAME_ANCESTOR_ORIGINS=https://console.zeroship.localhost`.
   - **TOML:** `ops/zeroship.toml` + `ops/zeroship.example.toml` `[auth] frame_ancestor_origins =
     ["https://console.zeroship.ai"]`.
2. **`trusted_oauth_clients` retained solely for `skip_consent`.** Confirmed: the gateway consumer is
   deleted; the control consumer (console `skip_consent=true` so the framed login auto-accepts identity
   consent) stays. The `ops/*.toml` value + the `zeroship_core::auth::trusted_clients` module + the
   `crates/control` `first_party`/`ensure_app_client`/`bootstrap_console` machinery survive with comments
   reframed to "skip Hydra consent for the first-party console only." **Open (clarity rename, not required):**
   post-pivot, consider renaming the config key from `trusted_oauth_clients` to `skip_consent_clients` to
   remove the now-stale "trusted for the gateway gate" connotation (pre-launch permits it; defer to keep the
   pivot's diff focused).
3. **SDK launcher gate field (DECIDED).** `AuthClientOptions.immersive?: boolean` (default `false`) is the
   primary iframe gate; `authOrigin?: string` provides the same-site sanity check
   (`eTLD+1(appOrigin) === eTLD+1(authOrigin)`). The SDK CANNOT infer same-site from `appOrigin` alone
   (it is the console's own origin; the cross-site hop is server-side), so these explicit inputs are
   required for §6.5's "two independent places" to be true. The console build sets `immersive:true` +
   `authOrigin:'https://auth.zeroship.ai'`; creator-app/custom-domain builds leave both unset → popup.
4. **`frame-ancestors` vs `'self'`.** We include `'self'` alongside the console origin so the auth origin's
   own pages can frame each other (e.g. nested auth flows). **Open:** confirm no auth page actually frames
   another; if not, drop `'self'` for an even tighter single-origin allowlist. (Kept for now — harmless and
   guards an un-audited nested-frame case.)
5. **Modal UX / focus management.** The iframe modal must trap focus and pass keyboard events into the
   frame (the browser does this automatically for the cross-origin frame), and must expose an accessible
   close control. **Open:** a11y review of the `AuthModal` (the e2e a11y spec that targeted the old
   `SignInForm` testids — `a11y.spec.ts` — needs rewriting; §4.6).
6. **CSRF `Strict` cookie in the framed GET→POST (DECIDED).** Static rationale: cross-origin-same-site ≠
   cross-site, so the Strict double-submit cookie rides on the same-site console (§6.1). **Decision:** keep
   `__Host-zsidp_csrf` `SameSite=Strict`; do not relax it speculatively; pin its survival AND a cross-site
   failure in the live e2e (§9). Same for `__Host-zeroship_app_anchor` `Strict`.

**Rejected findings (with rationale):**

- *postMessage to BOTH opener and parent when both are distinct* — **rejected.** The SDK uses exactly one
  launcher per flow (popup XOR iframe), so opener-and-parent are never both distinct in practice; the
  opener-first single-target keeps the snippet simple and the `targetOrigin` pin makes any wrong-context
  target safe regardless. Documented as intentional in §4.2/§6.4.
- *Adding `COOP: same-origin-allow-popups` to the console* — **rejected.** The console sends no COOP today;
  the default `unsafe-none` already preserves `window.opener` for the federated popup, so the
  recommendation is unnecessary and risks an implementer adding plain `same-origin` (which would sever the
  opener). Resolution: change nothing on the console's COOP (§6.6).
