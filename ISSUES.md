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

---

## ISS-13 · Skill registry not implemented (catalogue-only)

**Status:** open
**Severity:** medium — blocks the "Add to project" CTA on `/skills`;
the public catalogue ships as a static list until the registry lands.
**First observed:** 2026-05-01, building the public skill catalogue
(spec §5.3) for `apps/zeroship-builder`.
**Component:** `crates/control` (registry endpoints) + builder skill
manifest + agent prompt-fragment loader

### Symptom

Spec §5.3 calls for a Skill catalogue at `/skills` where each item
can be added to a project — installing it wires up the relevant
SDK (`@zeroship/auth`, `@zeroship/payments`, etc.), seeds env vars,
adds an agent prompt fragment so Builder/Critic know about the
capability, and updates a per-project `skills.json` manifest.
Today none of that exists:

- No `skills` table or registry API in the control plane.
- No "install" RPC procedure (`POST /apps/:id/skills`).
- No per-project `skills.json` manifest read by Builder or Critic.
- No agent prompt fragments in `apps/zeroship-builder/src/server/_prompts.ts`
  keyed by skill slug.

### Workaround in use

The `/skills` page renders a **static catalogue** sourced from
`apps/zeroship-builder/src/client/lib/skills.ts` (8 starter skills:
Auth, Email, Realtime, Payments, Photos, Search, AI, Analytics).
Each card shows the skill metadata and a disabled "Add to project →"
button captioned "Coming soon". A note at the bottom of the page
points readers at this issue.

This keeps the marketing surface honest (we ship what's on the
roadmap, not vapor) and unblocks signup-funnel work.

### Fix path

1. `crates/control/src/skills.rs` — registry table + `GET /skills`
   (catalogue) and `POST /apps/:id/skills` (install) handlers.
2. `apps/zeroship-builder/src/server/skills.ts` — RPC procedures
   wrapping the control-plane endpoints; per-project `skills.json`
   read/write helpers.
3. `apps/zeroship-builder/src/server/_prompts.ts` — keyed prompt
   fragments injected into Builder + Critic system prompts when a
   skill is active.
4. Replace the static `lib/skills.ts` catalogue with a TanStack
   Query against `GET /skills` so the catalogue stays in sync with
   what the platform actually supports.
5. Wire the per-card "Add to project" button to a project picker
   modal (or — when invoked from inside a workspace — an inline
   confirm) calling `installSkill`.

---

## ISS-14 · Issues table missing — PlanCanvas reads from in-memory stub

**Status:** open
**Severity:** medium — every issue (filed by the user, the PM agent,
the Critic, or the SRE agent) lives only in the worker's V8 isolate
and disappears when the process restarts. Roadmap and Deployments are
adjacent symptoms.
**First observed:** 2026-05-01, building the Plan canvas
(`apps/zeroship-builder/src/client/workspace/canvases/PlanCanvas.tsx`,
spec §9.8).
**Component:** `crates/control` + `apps/zeroship-builder/src/server/agents.ts`

### Symptom

Spec §9.8 calls for an Issues list per project — open / in-progress /
done buckets, status icons, source attribution (PM / SRE / you /
Builder), comments, and a "+ New issue" composer. The control plane
has no `issues` (nor `milestones`) table and no API surface for
reading or writing them:

- No `GET /api/apps/:id/issues` handler.
- No `POST /api/apps/:id/issues`, `PATCH /api/apps/:id/issues/:issue`,
  or comment endpoints.
- No tables: `issues`, `issue_comments`, `milestones`,
  `milestone_issues`.
- No agent-attribution column / event stream so the PM agent's writes
  show up in the same list as the user's.

### Workaround in use

`apps/zeroship-builder/src/server/agents.ts` exposes a module-level
`Map<appId, Issue[]>` lazily seeded with three sample issues per
project. `listIssues({appId})` reads the map; `addIssue({appId,
title, description})` prepends to it. The map evicts on V8 isolate
eviction (worker LRU) so the data is best described as "ephemeral".

The PlanCanvas Roadmap section attributes every issue to the first
milestone (`v0.1`) because there's no milestone-association data.
v0.2 / v0.3 render with empty progress bars.

### Fix path

1. `crates/control/src/issues.rs` — `issues`, `issue_comments`,
   `milestones`, `milestone_issues` tables + REST handlers
   (list/create/update/comment).
2. Per-project `agent_attribution` enum (`pm`, `sre`, `critic`,
   `builder`, `user`) so the PlanCanvas can render the right source
   chip.
3. `apps/zeroship-builder/src/server/agents.ts` — replace the in-
   memory Map with proxy calls to the new endpoints. Same wire shape
   (single-input objects).
4. Wire the PM agent's tool calls (file-issue, transition-status,
   comment) to those endpoints so its writes flow into the same list.

---

## ISS-15 · Deploy-history table missing — PlanCanvas shows current deploy only

**Status:** open
**Severity:** medium — rollback is impossible without history; a
multi-environment release narrative (v0.11 → v0.12) is the entire
point of spec §9.8 Deployments.
**First observed:** 2026-05-01, building the Plan canvas.
**Component:** `crates/control` (deploy lifecycle)

### Symptom

The control plane stores a single `deploy_hash` field on the `apps`
row — the most recent successful deploy. There is no `deploys` table,
so:

- No way to list past deploys with timestamp + author + scorecard +
  changelog (spec §9.8 mock).
- No `POST /api/apps/:id/deploys/:hash/rollback` handler.
- No diff-vs-prior view, no per-deploy linked issues.

### Workaround in use

`PlanCanvas` Deployments section renders a single "current deploy"
row when `app.deploy_hash` is set. The hash is shown short (12 chars).
A trailing italic note points at this issue. Empty state ("No deploys
yet — ship something first.") fires when `deploy_hash` is null.

### Fix path

1. `crates/control/src/deploys.rs` — `deploys` table (id, app_id,
   hash, author, message, scorecard_snapshot, created_at,
   superseded_at, status), with `GET /api/apps/:id/deploys` and
   `POST /api/apps/:id/deploys/:hash/rollback`.
2. Wire `deployApp` (control plane) to insert a row on success and
   set `superseded_at` on the previous live row.
3. `apps/zeroship-builder/src/server/agents.ts` — `listDeploys
   ({appId})` proxy + replace the single-row UI with a list.

---

## ISS-16 · Critic → quality scoreboard wiring missing

**Status:** open
**Severity:** medium — HealthCanvas's quality grid ships with hardcoded
scores. Spec §11.1 promises a live scorecard updated by the Critic
loop; that wire doesn't exist yet.
**First observed:** 2026-05-01, building the Health canvas
(`apps/zeroship-builder/src/client/workspace/canvases/HealthCanvas.tsx`,
spec §9.9 + §11.1).
**Component:** `apps/zeroship-builder/src/server/_critic.ts` →
`apps/zeroship-builder/src/server/agents.ts`

### Symptom

Spec §11.1 names seven dimensions (correctness, security,
performance, accessibility, ux_completeness, responsive, code_health)
that the Critic should grade on every Builder turn. The Critic
implementation in `_critic.ts` returns an internal verdict
(`approved` / `needs_revision` / `comments`) but does **not**:

- Emit a per-dimension grade.
- Persist a per-app "latest scorecard" anywhere readable from the
  client.
- Stream a `quality-update` chunk that the HealthCanvas could
  subscribe to in real time.

### Workaround in use

`apps/zeroship-builder/src/server/agents.ts` exposes
`getQualityScores({appId})` returning a hardcoded snapshot
(per-dimension grade + rationale + null `last_run_at`). HealthCanvas
renders that snapshot with overall + per-dimension cards. Grades are
the same for every app for now.

### Fix path

1. Extend `_critic.ts` verdict shape with a `dimensions` array that
   matches `QualityDimension`.
2. Persist the most recent scorecard alongside the deploy row (see
   ISS-15) so HealthCanvas can read it as part of the deploy detail.
3. Stream a `quality-update` chunk on the chat wire after every
   Critic round, with the live grade snapshot. HealthCanvas
   subscribes via the same SSE path as ChatRail.
4. Replace the hardcoded `defaultScores()` with a query against the
   persisted scorecard.

---

## ISS-17 · Incidents table missing — HealthCanvas shows empty state only

**Status:** open
**Severity:** medium — spec §9.9 promises a timeline of incidents
(detection → mitigated → resolved) with a root-cause analysis from
the SRE agent. Without a backing table the SRE agent has nowhere to
file its findings.
**First observed:** 2026-05-01, building the Health canvas.
**Component:** `crates/control` + SRE agent harness

### Symptom

The HealthCanvas Incidents section is supposed to render a timeline
of past incidents per app: detection time, root cause, fix applied,
downtime, scorecard delta. There's no `incidents` table, no SRE
agent yet (the agent shape is sketched in the spec but isn't
implemented), and no event source to derive incidents from
(uptime probes, error-rate threshold breaches).

### Workaround in use

HealthCanvas Incidents section ships as a static empty state styled
with the editorial "all quiet" copy:

> "All quiet — no incidents on record."

A trailing italic line points readers at this issue.

### Fix path

1. `crates/control/src/incidents.rs` — `incidents` table (id, app_id,
   detected_at, mitigated_at, resolved_at, root_cause, fix_applied,
   scorecard_delta, severity, agent_id) + REST handlers.
2. SRE agent harness (separate sub-agent in
   `apps/zeroship-builder/src/server/`) that watches the metering
   stream + uptime probes and files incident records.
3. `getIncidents({appId})` proxy in `agents.ts` and a real timeline
   UI in HealthCanvas (replace the empty state).

---

## ISS-18 · Performance metering pipeline missing — HealthCanvas shows placeholders

**Status:** open
**Severity:** medium — the HealthCanvas Performance section is fully
gated on metering data that doesn't flow from the worker yet.
**First observed:** 2026-05-01, building the Health canvas.
**Component:** `crates/runtime` (instrumentation) + `crates/control`
(aggregation API)

### Symptom

Spec §9.9 calls for live charts: requests/sec, p50/p95/p99 latency,
error rate, over 24h / 7d / 30d, filterable by route. The platform
has the `zeroship.meter.*` primitive in `crates/plugin-*` but:

- The runtime doesn't auto-emit per-request latency / status counters.
- There's no aggregation endpoint
  (`GET /api/apps/:id/perf?window=24h&route=/api/login`).
- There's no time-series store wired up (Prometheus / VictoriaMetrics /
  ClickHouse — choice deferred).

### Workaround in use

HealthCanvas Performance section renders three placeholder tiles
(p95 latency · error rate · requests, all 24h) with em-dashes and
the hint "Connect a deploy to see live performance." A trailing
italic line points at this issue.

### Fix path

1. Auto-instrument the worker request handler to emit per-request
   `latency_ms`, `status_class`, `route` counters via
   `zeroship.meter.*`.
2. Pick the time-series backend and wire writes from the metering
   pipe.
3. `crates/control/src/perf.rs` — query handler returning bucketed
   latency percentiles + error rate + RPS for the window.
4. `apps/zeroship-builder/src/server/agents.ts` —
   `getPerformance({appId, window})` proxy.
5. Replace the placeholder tiles with real charts (lightweight SVG
   sparkline, no chart-lib dependency).

