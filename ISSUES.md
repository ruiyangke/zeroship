# Known Issues

Tracking known platform-level issues with no immediate fix landed. Each entry
is a self-contained workaround note for downstream work plus a `Fix path:`
pointer for whoever picks it up.

## Status legend

- **open** — not yet fixed; the workaround in this entry is still in use.
- **closed** — fix landed in tree; entry kept for the historical record.
- **superseded** — replaced by a newer entry with a wider fix path.

## Severity legend

- **medium-high** — forces unnatural code shapes or has a public-surface
  consequence; fix soon, but a workaround keeps the product honest in the
  meantime.
- **medium** — a stubbed surface that ships as a UI-honest placeholder while
  the backing wire is missing. Acceptable for V1 alpha; must close before
  GA / production sign-up.
- **low-medium** — feature ships per-process or with reduced fidelity; correct
  for one user / one session, broken across nodes / restarts.
- **low** — power-user / polish feature; not in the V1 critical path.

## Index (by severity, then ID)

| Severity | Issues |
|---|---|
| medium-high | ISS-01 |
| medium | ISS-02 · ISS-09 · ISS-12 · ISS-13 · ISS-14 · ISS-15 · ISS-16 · ISS-17 · ISS-18 · ISS-20 · ISS-21 · ISS-23 · ISS-24 · ISS-25 · ISS-26 · ISS-28 |
| low-medium | ISS-19 |
| low | ISS-10 · ISS-11 · ISS-22 |

All entries below are **open** as of 2026-05-01. None of the ~120 commits on
`redesign/plan-01-foundation` resolved any of these — each is a control-
plane / platform-side fix that the builder app can't land in isolation.

Entries are listed below in the same order as the index above (severity
descending; ties broken by issue id). Anchor links are preserved by ID.

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

---

## ISS-20 · pg_catalog table introspection missing — DataCanvas Tables list is hardcoded

**Status:** open
**Severity:** medium — every project's Tables tab shows the same three
sample rows (`users`, `posts`, `comments`). Without real introspection
the canvas can't reflect what the project actually wrote.
**First observed:** 2026-05-01, building the Data canvas
(`apps/zeroship-builder/src/client/workspace/canvases/DataCanvas.tsx`,
spec §9.3).
**Component:** `crates/control` (per-app db introspection) +
`apps/zeroship-builder/src/server/agents.ts`

### Symptom

Spec §9.3 calls for a Tables list per project sourced from
`pg_class` + `pg_total_relation_size` for the per-app schema. The
control plane has no introspection RPC:

- No `GET /api/apps/:id/db/tables` handler.
- No way to query `pg_catalog.pg_class` scoped to the app's schema.
- No row-count / size aggregation over the per-app `search_path`.

### Workaround in use

`agents.ts` exposes `listTables({appId})` returning a shared
`SAMPLE_TABLES` constant (3 rows). Every appId sees the same data;
nothing the user does in the canvas mutates it.

### Fix path

1. `crates/control/src/db_introspect.rs` — `GET /api/apps/:id/db/tables`
   handler that runs `SELECT relname, reltuples, pg_total_relation_size, ...
   FROM pg_class WHERE relnamespace = (per-app schema oid)`.
2. Cache the result on the control plane (TTL 30s) so repeated
   dashboard polls don't hammer the catalog.
3. Replace the `SAMPLE_TABLES` constant with a proxied call.

---

## ISS-21 · Table row pagination over real per-app schema missing

**Status:** open
**Severity:** medium — clicking into a table on the Data canvas opens
a row browser sourced from the same hardcoded sample data. There's
no path to read actual rows from the per-app schema.
**First observed:** 2026-05-01, building the Data canvas row browser.
**Component:** `crates/control` + `apps/zeroship-builder/src/server/agents.ts`

### Symptom

The Data canvas row browser (clicking a table row) calls
`getTableRows({appId, tableName, limit, offset})`. Backing logic
needs:

- A per-app database connection (the worker has one; the dashboard
  doesn't).
- A safe `SELECT * FROM <schema>.<table> LIMIT $1 OFFSET $2` runner
  that quotes identifiers properly and refuses non-existent relations.
- Type-aware cell rendering (jsonb, timestamptz, numeric) — V1 strings
  every cell which is fine for stub data but loses precision for
  real rows.

### Workaround in use

`getTableRows` returns slices of a fixed in-memory `SAMPLE_ROWS` map
keyed by `users` / `posts` / `comments`. Pagination state echoes back
correctly so the UI's prev/next behaves like the real thing.

### Fix path

1. `crates/control/src/db_introspect.rs` — gain
   `GET /api/apps/:id/db/tables/:name/rows?limit=&offset=` returning
   `{columns, rows, total}`. Use `pg_class.reltuples` for `total`
   (cheap; exact COUNT is too slow).
2. Identifier quoting via the postgres protocol's parameter mechanism
   for the table name (or a strict allowlist filter against the
   introspection list from ISS-20).
3. `agents.ts` — replace the SAMPLE_ROWS lookup with a proxied call;
   the canvas wire stays the same.

---

## ISS-23 · Index introspection (`pg_indexes`) missing — DataCanvas Indexes tab is hardcoded

**Status:** open
**Severity:** medium — same shape as ISS-20 but for indexes.
**First observed:** 2026-05-01, building the Data canvas Indexes tab.
**Component:** `crates/control` + `apps/zeroship-builder/src/server/agents.ts`

### Symptom

Spec §9.3 calls for an Indexes tab listing every index on the per-app
schema with table, name, type, columns, on-disk size, and last-used
timestamp. The control plane has no introspection RPC:

- No `GET /api/apps/:id/db/indexes` handler.
- No reads of `pg_indexes` / `pg_class` / `pg_stat_user_indexes`
  (`idx_scan`, `last_idx_scan`).

### Workaround in use

`agents.ts` exposes `listIndexes({appId})` returning a shared
`SAMPLE_INDEXES` constant (5 sample rows across the three sample
tables).

### Fix path

1. Extend the introspection endpoint family with
   `GET /api/apps/:id/db/indexes` joining `pg_indexes`,
   `pg_class.relpages` (for size), and `pg_stat_user_indexes`
   (for `idx_scan` + `last_idx_scan`).
2. Replace `SAMPLE_INDEXES` with a proxied call.

---

## ISS-24 · Migration log not persisted — DataCanvas Migrations tab is hardcoded

**Status:** open
**Severity:** medium — the timeline is the user's audit trail of
what changed when. Without persistence the only record of a
migration is in the deploy bundle's git history (which the dashboard
doesn't read).
**First observed:** 2026-05-01, building the Data canvas Migrations tab.
**Component:** `crates/control` (migrations table) + `@zeroship/db`
(migration runner emits records) + `apps/zeroship-builder/src/server/agents.ts`

### Symptom

Spec §9.3 calls for a chronological list of every migration: id,
name, status (applied/pending/failed), author, applied-at. There's
no `migrations` table on the control plane and no instrumentation in
the `@zeroship/db` migration runner to record runs.

### Workaround in use

`agents.ts` exposes `listMigrations({appId})` returning a shared
`SAMPLE_MIGRATIONS` constant (4 rows including one `pending` to
exercise the status dot variants).

### Fix path

1. `crates/control/src/migrations.rs` — `migrations` table (id,
   app_id, name, status, author, started_at, finished_at,
   error) + `GET /api/apps/:id/migrations` handler.
2. `@zeroship/db` runner — wrap each migration in a try/finally that
   POSTs status updates to the control plane.
3. Replace `SAMPLE_MIGRATIONS` with a proxied call.

---

## ISS-25 · Backup trigger + history not wired to actual pg_dump

**Status:** open
**Severity:** medium — the "Trigger backup" button and snapshot list
are pure UI today. No real backup is taken; restore doesn't exist.
**First observed:** 2026-05-01, building the Data canvas Backups tab.
**Component:** `crates/control` (backup orchestrator) + cloud storage
(snapshot blob writes) + `apps/zeroship-builder/src/server/agents.ts`

### Symptom

Spec §9.3 calls for daily auto-snapshots plus on-demand manual ones,
with a list view (label, kind, size, taken-at), a restore action, and
a per-app retention policy. The control plane has none of:

- A `backups` table or `POST /api/apps/:id/backups` trigger.
- A pg_dump runner (logical) or volume-snapshot integration (physical).
- Cloud-storage writes for the snapshot blobs.
- A `POST /api/apps/:id/backups/:id/restore` handler.

### Workaround in use

`agents.ts` exposes `listBackups({appId})` (per-appId Map seeded with
3 sample snapshots) and `triggerBackup({appId})` (prepends a fake
`Manual snapshot` entry with a randomized size). The Map survives only
the lifetime of the V8 isolate.

### Fix path

1. `crates/control/src/backups.rs` — `backups` table + REST handlers
   (list, trigger, restore).
2. Backup runner: spawn `pg_dump` in a sandboxed worker, stream the
   output to S3/R2 with content-addressed key, write the metadata row
   when complete.
3. Background scheduler that triggers a backup per app per day.
4. Replace the in-memory Map in `agents.ts` with proxied calls.

---

## ISS-26 · Media canvas backed by in-memory Map, not real `@zeroship/storage`

**Status:** open
**Severity:** medium — every uploaded file lives in the worker's V8
isolate as a base64 data URL. Files vanish on isolate eviction and
aren't reachable from the deployed app's runtime (which is the whole
point of an asset library).
**First observed:** 2026-05-01, building the Media canvas
(`apps/zeroship-builder/src/client/workspace/canvases/MediaCanvas.tsx`,
spec §9.4).
**Component:** `crates/control` (per-app upload RPC) +
`apps/zeroship-builder/src/server/agents.ts`

### Symptom

Spec §9.4 calls for a Media canvas where uploads land in the project's
object store and become servable via a public URL the deployed app
can reference. The platform has the `zeroship.storage.*` primitive
and `@zeroship/storage` SDK package, but there's no dashboard-side
upload RPC:

- No `POST /api/apps/:id/media` (multipart) handler.
- No `GET /api/apps/:id/media` (list) handler.
- No `DELETE /api/apps/:id/media/:key` handler.
- No public-URL minting for uploaded blobs.
- No CDN/cache headers on the asset path.

### Workaround in use

`agents.ts` exposes `listMedia` / `uploadMedia` / `deleteMedia` over
a per-appId `Map<string, MediaEntry[]>` seeded with 3 sample tiles
(2 picsum.photos URLs + 1 PDF). Uploads round-trip the file as base64
and store the bytes inline as a `data:` URL — fine for the canvas
preview, useless for the deployed app.

### Fix path

1. `crates/control/src/media.rs` — multipart upload handler that
   streams the body straight to the platform's object store
   (`LocalFs` in dev, S3/R2 in prod) and writes a `media` row
   (id, app_id, key, name, size, content_type, uploaded_at).
2. URL minting: a `GET /m/:app_id/:key` proxy endpoint on the gateway
   (or a signed S3/R2 URL) so the deployed app and dashboard read
   from the same path.
3. Replace the Map in `agents.ts` with proxied calls; switch the
   tile's `url` field from a `data:` URL to the real public URL.

---

## ISS-28 · Cron / scheduled-worker harness missing — PM digest + SRE monitor are RPC-only

**Status:** open
**Severity:** medium — the dual-shape PM/SRE design (spec §4.8.3.2)
calls for periodic background passes (PM digest, SRE monitor) that
post into the project chat thread on a cadence. The procs exist
(`pmDigest`, `sreMonitor`), but nothing fires them; an external cron
or manual hit is required to see output.
**First observed:** 2026-05-01, building the PM/SRE worker stubs in
`apps/zeroship-builder/src/server/pm_worker.ts` +
`apps/zeroship-builder/src/server/sre_worker.ts`.
**Component:** `crates/control` (scheduler) + chat-thread write path
(out-of-band assistant turns)

### Symptom

Spec §4.8.3.2 calls out that PM and SRE both ship as a dual shape:

- A chat SubAgent (Builder dispatches via `task("pm", …)` /
  `task("sre", …)`) — implemented in `_pm.ts` / `_sre.ts`.
- A scheduled background worker that polls project state on a
  cadence and posts findings into the chat thread.

The worker procs are wired (`pmDigest({appId})` →
`/_zs/v1/pm.digest`, `sreMonitor({appId})` →
`/_zs/v1/sre.monitor`), but there's no scheduler to call them:

- No `crates/control` job that polls `apps` on a timer (e.g. every
  hour for SRE monitor, daily for PM digest) and POSTs to the
  worker procs.
- No "out-of-band assistant turn" path in chat that lets an
  external caller drop a `data-pm-recommendation` /
  `data-sre-finding` chunk into a project's existing thread
  without an HTTP `useChat` round-trip.
- No observability for the cron itself — last-run timestamps,
  failure rates, opt-out per project.

The procs themselves work standalone: a curl from a developer's
shell or an external cron container can hit them today and get a
real structured response back.

### Workaround in use

Manual / external invocation only. The procs are exposed via the
RPC wire (single-input `{appId}` shape) so a one-line curl is
enough to test:

```
curl -X POST http://localhost:5173/_zs/v1/pm.digest \
  -H "content-type: application/json" \
  -d '{"json":{"appId":"<appId>"}}'
```

A future external scheduler (cron container, CI cron, GitHub
Actions on a cron schedule) can drive both procs against the
deployed builder. The dashboard does not yet expose a "run digest
now" affordance.

### Fix path

1. `crates/control/src/scheduler.rs` — small compio-native scheduler
   that ticks every minute, looks at `apps.scheduled_jobs` (new
   table: `app_id, kind, last_run_at, cadence, opt_out`), and
   POSTs to the builder's worker procs when a job is due. Use the
   existing builder RPC wire — no new auth surface needed.
2. New table: `agent_digests` (id, app_id, kind, payload_json,
   created_at) so the cron's results survive worker restarts and
   can be replayed into the chat thread.
3. Out-of-band chat write: `chat.ts` gains a path that accepts a
   `digest_id` and emits the corresponding `data-pm-recommendation`
   /  `data-sre-finding` chunk into the project's chat thread.
4. Dashboard: a "Run digest now" button on the Plan canvas
   (`PlanCanvas.tsx`) and "Scan health now" on Health
   (`HealthCanvas.tsx`) that hit the worker procs directly,
   bypassing the cron, for ad-hoc use.
5. Per-project opt-out + cadence editor in SettingsCanvas (e.g.
   "PM digest: weekly · SRE monitor: hourly · off").

### Related references

- `apps/zeroship-builder/src/server/pm_worker.ts` — `pmDigest` proc
  + the digest-shaped prompt rendering helpers
- `apps/zeroship-builder/src/server/sre_worker.ts` — `sreMonitor`
  proc + the monitor-shaped prompt rendering helpers
- `apps/zeroship-builder/src/server/_pm.ts` /
  `apps/zeroship-builder/src/server/_sre.ts` — the chat-mode
  SubAgent halves of the dual shape
- `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md`
  §4.8.3.2 — the dual PM/SRE shape spec

---

## ISS-19 · Project archive — control-plane backing missing

**Status:** open
**Severity:** low-medium — archive is a reversible UI affordance and
the spec (§8.3) explicitly frames it as a soft alternative to delete.
The current stub gives users the affordance without losing data, but
state doesn't survive a server restart and isn't shared across
control-plane nodes.
**First observed:** 2026-05-01, building the project lifecycle polish.
**Component:** `crates/control` + `apps/zeroship-builder/src/server/apps.ts`

### Symptom

Spec §8.3 calls for archive / delete / transfer in the Settings
canvas. Delete is wired (control plane has DELETE /api/apps/:id).
Archive has no backing column or endpoint:

- `AppRecord` has no `archived_at` / `archived` field.
- There's no `PUT /api/apps/:id/archive` or equivalent.
- No filter on `GET /api/apps?archived=true|false`.

### Workaround in use

`apps/zeroship-builder/src/server/apps.ts` ships a module-level
`Set<string>` (`archivedApps`) that tracks which app ids are flagged
archived. `listApps` / `getApp` decorate the proxied control-plane
records with `archived: bool` from this Set; `archiveApp({appId})`
and `unarchiveApp({appId})` mutate it. The Set is local to the
zeroship-builder dev server process — it's lost on restart and not
shared across multi-node deployments.

The Home gallery and SettingsCanvas read `app.archived` and render
the Archive section / filter pill accordingly. From the user's
perspective the affordance behaves correctly within a single
session.

### Fix path

1. `crates/control` — add `archived_at TIMESTAMPTZ NULL` column to
   `apps` (migration). Index on `(creator_id, archived_at IS NULL)`
   so the active-projects list stays fast.
2. REST: `PUT /api/apps/:id/archive` and
   `PUT /api/apps/:id/unarchive` (or a single
   `PATCH /api/apps/:id { archived: bool }`).
3. `GET /api/apps?include_archived=true` for the archived view;
   default lists active only.
4. Replace the in-memory Set in
   `apps/zeroship-builder/src/server/apps.ts` with proxied calls.

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

## ISS-22 · Schema visualizer not built — DataCanvas Schema tab is a placeholder

**Status:** open
**Severity:** low — the Data canvas's Schema tab is a "coming soon"
card. Tables / Indexes / Migrations cover the day-one introspection
surface; a graph visualizer is a polish piece.
**First observed:** 2026-05-01, building the Data canvas.
**Component:** `apps/zeroship-builder/src/client/workspace/canvases/DataCanvas.tsx`

### Symptom

Spec §9.3 calls for a Schema sub-tab that renders the per-app schema
as a navigable graph: tables as nodes, foreign keys as edges, with
a click-to-zoom interaction. Today the tab ships as a single
editorial card pointing at this issue.

There's no introspection of `pg_constraint` (`contype = 'f'`) for
foreign keys, no layout engine, and no SVG/canvas renderer chosen.

### Workaround in use

`SchemaPane` in `DataCanvas.tsx` renders a static empty state:

> "Schema visualizer coming soon — see ISSUES.md ISS-22."

### Fix path

1. Extend the introspection endpoint from ISS-20 with a
   `GET /api/apps/:id/db/schema` handler returning
   `{tables: [...], foreign_keys: [{from_table, from_column,
   to_table, to_column}, ...]}`.
2. Pick a layout engine — `d3-force` is heavy but well-tested;
   `elkjs` produces nicer hierarchical layouts. Default to a
   force-directed graph for the smallest dependency footprint.
3. Replace `SchemaPane` with the live visualizer; keep the
   "coming soon" copy as a fallback when the schema is empty.
