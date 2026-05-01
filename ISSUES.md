# Known Issues

Tracking known platform-level issues with no immediate fix landed. Each entry
is a self-contained workaround note for downstream work plus a `Fix path:`
pointer for whoever picks it up.

---

## ISS-01 · `node:async_hooks` / `AsyncLocalStorage` not propagated by `@zeroship/vite-plugin`

**Status:** open
**Severity:** medium-high — forces unnatural code shapes for any agent doing structured human-in-the-loop
**First observed:** 2026-05-01, building the project-creation wizard on plain LangGraph (`apps/zeroship-builder/src/server/_wizard.ts`)
**Component:** `sdks/vite-plugin` (dev mode, RPC-handler module loading)

### Symptom

Calling `interrupt()` from `@langchain/langgraph` after **any** awaited HTTP
fetch (e.g. `await model.invoke(...)`) inside a `StateGraph` node throws:

```
Error: Called interrupt() outside the context of a graph.
    at interrupt (.../dist-DKxU9jCg.js:8994:21)
    at clarifierBody (.../src/server/_wizard.ts:307:24)
    at RunnableCallable.invoke (.../dist-DKxU9jCg.js:9052)
```

The same `interrupt()` call **succeeds** if placed at the top of the node body
before any awaits.

### Root cause

`@langchain/langgraph` initializes its singleton via:

```js
import { AsyncLocalStorage } from "node:async_hooks";
AsyncLocalStorageProviderSingleton.initializeGlobalInstance(new AsyncLocalStorage());
```

then uses `runWithConfig(config, callback)` to attach the per-node config to
the storage so `interrupt()` can find the graph context via
`AsyncLocalStorageProviderSingleton.getRunnableConfig()`.

This requires:

1. A working `node:async_hooks` module in the V8 isolate.
2. An `AsyncLocalStorage` whose `.run(value, cb)` actually preserves `value`
   across `await` boundaries — including continuations that resume from
   native (Rust-implemented) async work like `fetch`.

The Vite plugin currently does NOT supply either of those:

- `crates/runtime/src/embed/` has no `async_hooks` polyfill (`node-globals.js`
  doesn't define one). `docs/reference/node-compat.md` describes one
  conceptually, but it isn't shipped.
- Whatever shim resolves `node:async_hooks` in the dev bundle either returns
  the **mock** `AsyncLocalStorage` from `@langchain/core` (a no-op whose
  `getStore()` always returns `undefined`) or a closure-based polyfill that
  doesn't survive native fetch resumption.

Empirical evidence: `interrupt()` works at the top of a node (no await
before it) but fails after `await model.invoke()`. That is the exact
signature of the storage being torn down at the V8/Rust async boundary.

### Affected code paths

Any langchain/langgraph code that calls one of these after an HTTP fetch:

- `interrupt(value)` — human-in-the-loop halt
- `getStore()` — context vars
- `getCurrentTaskInput()` — node input access
- `getWriter()` / `getStreamWriter()` — custom-stream emit
- `getStore` / `getRunnableConfig()` — generic config lookup

This affects **plain LangGraph** code we write directly. **deepagents**
agents (the Builder runtime) currently work because their natural shape
separates the LLM call (its own RunnableCallable) from the tool body that
calls `interrupt()` — the AsyncLocalStorage gets re-set when the next
RunnableCallable starts. So Builder is fine; only hand-rolled StateGraphs
have to dodge this.

### Workaround in use

Split any node that needs both an await and an `interrupt()` into **two
nodes**:

1. **`decide`** node: does the fetch (`await model.invoke(...)`) and stashes
   the result in graph state. No `interrupt()` here.
2. **`act`** node: reads the stashed result; if it needs to halt, calls
   `interrupt()` with NO awaits before it.

Routing: `decide → act → (loop or END)`.

Reference implementation: `apps/zeroship-builder/src/server/_wizard.ts`
(`decide` + `act` nodes; comment-block at the top documents the workaround).

### Related references

- `docs/reference/node-compat.md:237-313` — describes the AsyncLocalStorage
  polyfill that should exist (currently aspirational)
- `apps/zeroship-builder/src/server/_wizard.ts` — current workaround, with
  inline comment block explaining the failure mode
- `apps/zeroship-builder/node_modules/.vite/deps_zeroship/dist-DKxU9jCg.js:8994` —
  the `interrupt()` implementation that throws the visible error
- `apps/zeroship-builder/node_modules/.vite/deps_zeroship/base-BsBbQDQi.js:13775-13817` —
  the `MockAsyncLocalStorage` and `AsyncLocalStorageProvider` from
  `@langchain/core` that gets used when no real instance is initialized

---

## ISS-02 · `@zeroship/vite-plugin` registers every exported function as an RPC procedure — no opt-in marker

**Status:** open
**Severity:** medium — silently publishes internal helpers as public endpoints if a developer mis-routes an import
**First observed:** 2026-05-01, while documenting the underscore-prefix file convention in `apps/zeroship-builder/src/server/`
**Component:** `sdks/vite-plugin/src/rpc-registry.ts`

### Symptom

Any function exported from a module re-exported by `src/server.ts` becomes a
live RPC procedure at `/_zs/v1/<exportName>`. There is no opt-in marker
(decorator, `.config`, naming rule). The plugin's procedure-discovery loop is:

```js
// sdks/vite-plugin/src/rpc-registry.ts:97-106
import * as _zsUser from <userImport>;
const _procedures = {};
for (const _k of Object.keys(_zsUser)) {
  if (_k === "default") continue;
  const _v = _zsUser[_k];
  if (typeof _v !== "function") continue;
  const _id = (_v.config && typeof _v.config.id === "string" && _v.config.id) || _k;
  _procedures[_id] = _v;
}
```

If a developer writes `export * from "./server/_translator"` in `server.ts` to
get a type or share a helper, every exported function in `_translator.ts`
(`buildTranslatedStream`, `convertUIMessagesToLangChain`, …) is registered as
a public, callable, network-reachable RPC endpoint. That's an unintended
attack/abuse surface — these helpers take stream writers, abort signals,
and other server-internal arguments that have no safe public-input shape.

### Why it's a real risk, not theoretical

- Internal helpers often have **looser input validation** because they trust
  their callers (other server modules). Once exposed via RPC, they accept
  arbitrary user input.
- The leak is **silent**: there's no startup warning, no diff in the manifest
  emitter, no "this looks suspicious" check. A misplaced `export * from` in
  a code review is easy to miss.
- The current safety relies entirely on the file-naming convention
  (underscore-prefixed files = "internal"). Conventions don't enforce
  themselves; the next contributor onboarding has no automated guardrail.

### Workaround in use

Project-side discipline:

1. Underscore-prefix every file under `src/server/` that contains internal
   helpers (`_translator.ts`, `_middleware.ts`, `_critic.ts`, `_tools.ts`,
   `_prompts.ts`, `_sandbox_backend.ts`, `_survey_wire.ts`, `_wizard.ts`).
2. Only re-export non-underscore files from `src/server.ts`.
3. Comment block at the top of each `_*.ts` file noting it is internal.

This is fragile — one accidental `export * from "./server/_wizard"` would
publish `buildWizardStream` as an RPC endpoint that takes a writer and an
abort signal as arguments.

### Related references

- `sdks/vite-plugin/src/rpc-registry.ts:90-110` — the discovery loop that
  registers everything
- `apps/zeroship-builder/src/server.ts` — current opt-in re-export entry
  (the only thing standing between an internal helper and a public endpoint)
- `apps/zeroship-builder/src/server/_*.ts` — the eight files that rely on
  the underscore-naming convention to stay private

---

## ISS-09 · `/auth/forgot-password` endpoint not exposed by control plane

**Status:** open
**Severity:** medium — password recovery is unavailable; users locked out
of their account have no self-serve path back in.
**First observed:** 2026-05-01, polishing the auth UI surfaces in
`apps/zeroship-builder` (spec §6.3).
**Component:** `crates/control` + gateway `/auth/*` proxy

### Symptom

The dashboard's `ForgotPassword` page (`src/client/pages/ForgotPassword.tsx`)
posts an email and expects the control plane to (a) generate a one-shot
reset token, (b) email it to the user, and (c) accept that token at a
follow-up `/auth/reset-password` endpoint. None of that exists yet:

- No `forgot_password` handler in `crates/control/src/auth_*.rs`.
- No `password_reset_tokens` table in the auth schema.
- No SMTP / email-provider wiring (and email templates) in the platform.

### Workaround in use

The page ships as a **UI stub** that always shows the standard
no-enumeration confirmation reply:

> "If an account exists for that email, we sent reset instructions."

This keeps the visible UX correct (so we don't have to redo the page
when the backend lands) and avoids the email-enumeration leak that a
"user not found" response would create. When the endpoint ships,
`submit()` should call it and ignore the status, preserving the same
visible reply.

### Fix path

1. `crates/control/src/auth_password_reset.rs` — new handlers for
   `POST /auth/forgot-password` (always-200) and `POST /auth/reset-password`
   (validates token, rotates password, invalidates other sessions).
2. New table `password_reset_tokens` (id, user_id, token_hash, expires_at,
   used_at, ip).
3. SMTP/email-provider wiring + a templated reset email.
4. Update `ForgotPassword.tsx` to call `/auth/forgot-password` (still
   ignoring status for no-enumeration).
5. New page `ResetPassword.tsx` for the token-bearing landing URL.

---

## ISS-10 · `/auth/sessions` list/revoke endpoints not exposed

**Status:** open
**Severity:** low — power-user feature; not blocking sign-up/sign-in.
**First observed:** 2026-05-01, building the Account page (spec §6.5).
**Component:** `crates/control` + gateway `/auth/*` proxy

### Symptom

Spec §6.5 calls for an Account → Sessions card listing every active
session for the current user (created_at, ip, user-agent) with a
"sign out" button per row plus "sign out everywhere". The control
plane stores sessions in the `auth_sessions` table but doesn't expose
them via the API:

- No `GET /auth/sessions` handler.
- No `DELETE /auth/sessions/:id` handler.
- No `POST /auth/sessions/revoke-all` handler.

### Workaround in use

The Account page renders a **deferred-section stub** for Sessions:

> "Coming soon — see ISSUES.md ISS-10."

The stub keeps the layout from collapsing and signposts the gap.

### Fix path

1. `crates/control/src/auth_sessions.rs` — list/revoke handlers, scoped
   to the current user.
2. Marker for "this session" in the list response so the UI can show
   "current device".
3. Wire into `src/server/auth.ts` as `listSessions` / `revokeSession` /
   `revokeAllSessions` proxy procedures.
4. Replace the stub in `Account.tsx` with a real table + per-row
   action button.

---

## ISS-11 · Two-factor (TOTP) enrollment not wired

**Status:** open
**Severity:** low — security upgrade, not a blocker for V1.
**First observed:** 2026-05-01, building the Account page (spec §6.5).
**Component:** `crates/control` + gateway `/auth/*` proxy

### Symptom

Spec §6.5 calls for a TOTP enrollment flow on the Account page:
QR-code scan → 6-digit confirm → backup-codes panel. None of that
exists in the control plane yet:

- No `totp_secrets` table.
- No `POST /auth/2fa/enroll` (returns provisioning URI + QR data).
- No `POST /auth/2fa/verify` (consumes the first 6-digit code, marks
  enrollment as active).
- No `POST /auth/2fa/disable`.
- Login flow does not branch on a `requires_2fa` response.

### Workaround in use

The Account page renders a **deferred-section stub** for Two-factor:

> "Coming soon — see ISSUES.md ISS-11."

### Fix path

1. `crates/control/src/auth_totp.rs` — enroll/verify/disable handlers,
   secret stored encrypted at rest.
2. `auth_sessions` augmented with `mfa_validated_at` so the session
   carries the proof.
3. Login response gains a `{requires_2fa: true, challenge_id}` branch
   when 2FA is on; UI prompts for the code.
4. Replace the Account stub with the QR enrollment card + backup-codes
   reveal.

---

## ISS-12 · Account-deletion endpoint not exposed

**Status:** open
**Severity:** medium — GDPR/right-to-be-forgotten, but not in the
critical path for V1 launch.
**First observed:** 2026-05-01, building the Account page (spec §6.5).
**Component:** `crates/control`

### Symptom

Spec §6.5 calls for a "Delete account" button that wipes the user's
projects, sessions, OAuth connections, and identity. The control plane
has no `DELETE /auth/me` (or equivalent) handler — there is no path for
a user to remove themselves.

Compliance angle: if zeroship is to onboard EU users, this can't stay
deferred indefinitely; a request → grace-period → hard-delete flow is
the standard shape.

### Workaround in use

The Account page renders a **deferred-section stub** styled with the
tomato (danger) tone and points at this issue:

> "Coming soon — see ISSUES.md ISS-12."

### Fix path

1. `crates/control/src/auth_delete.rs` — `POST /auth/delete` handler
   that schedules deletion (status: `pending`, `scheduled_for`) and
   sends a confirmation email with an "undo" link.
2. Background job that hard-deletes after the grace period: removes
   apps + bundles + blobs, sessions, oauth links, billing records
   (per Stripe's data-retention rules), and finally the user row.
3. UI flow: confirmation modal ("type your email to confirm"), then a
   banner on the Account page showing the pending deletion + undo CTA.
