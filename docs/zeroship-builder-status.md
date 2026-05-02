# zeroship-builder — branch status

**Branch:** `redesign/plan-01-foundation`
**HEAD:** `aadaf753` (2026-05-01)
**Companion docs:** `docs/zeroship-builder-spec-compliance.md` · `ISSUES.md` · `docs/superpowers/plans/2026-05-01-zeroship-builder-plan-02-builder-finish.md` · `.github/PULL_REQUEST.md`

---

## Where this branch is

**Maturity: alpha — shippable as a closed beta against a single creator cohort, not yet production-ready for public sign-up.**

What works end-to-end:
- A stranger lands on `/`, browses marketing, signs up, lands on `/onboarding/intent`, picks an intent, lands on `/home`, types an idea into the prompt, the wizard collects a brief through one or two surveys, Begin creates the project (real `createApp` against the control plane), the workspace mounts at `/p/:appId/preview` with the brief seeded into the chat, Builder responds via real OpenAI streaming, the Critic / Reviewer / PM / SRE SubAgents fire on the right turns, and the user can navigate every canvas pill.

What's brittle:
- Most canvas data behind the workspace tabs is **stubbed against in-memory `Map`s and constants** (issues, deploys, scorecard, incidents, performance, tables, indexes, migrations, backups, media). Server restarts wipe state. Multi-node deployments can't share state.
- **Project archive is per-process.** A creator who archives a project on one node and refreshes the gallery against another sees the project unarchived (ISS-19).
- **Forgot-password is a UI-only stub** — no email actually goes out (ISS-09).
- **Scheduled workers (PM digest / SRE monitor) only fire when curl'd** — there is no platform cron (ISS-28).
- **Telemetry events fill localStorage.** No pipeline sink exists yet — `analytics.track()` writes to a buffer but nothing reads it.

What's solid:
- TypeScript: `tsc --noEmit` clean.
- 32 e2e tests pass without any env (auth UI · onboarding · public surfaces · lifecycle · chat actions).
- 22 e2e tests pass with `OPENAI_API_KEY` and/or control plane (Builder, wizard, Critic loop, multi-agent fleet, plan / health / data / media canvases, full spine).
- The chat surface, the multi-agent fleet wire, the survey resume protocol, and the sandbox backend protocol are all well-covered by tests + code comments.
- Editorial polish layer (responsive · a11y · empty states · ErrorBoundary) is complete.

---

## What blocks production

These must land before opening sign-up to the public. Each is a discrete chunk of control-plane / platform work; none requires builder-side changes.

| # | Issue | Why it blocks production |
|---|---|---|
| 1 | **ISS-09** · forgot-password endpoint | Users locked out of their account today have no self-serve path back in. |
| 2 | **ISS-19** · project archive backing | State doesn't survive restart and isn't shared across nodes. The Home filter pill lies in a multi-node deployment. |
| 3 | **ISS-14** · issues table | Every issue (filed by user / PM / Critic / SRE) lives in an in-memory map and disappears on isolate eviction. |
| 4 | **ISS-15** · deploy history | Rollback is impossible without history. Single-deploy view is misleading. |
| 5 | **ISS-26** · media canvas backed by base64 in a Map | Uploaded files vanish on isolate eviction and aren't reachable from the deployed app's runtime — which is the whole point of an asset library. |
| 6 | **ISS-28** · cron / scheduled-worker harness | The dual PM/SRE shape from spec §4.8.3.2 is half-shipped (chat-mode only). Without cron, "your AI PM checks in daily" is a lie. |
| 7 | **ISS-12** · account deletion | GDPR-compliance angle. Required before EU sign-ups. |
| 8 | **ISS-16** · Critic → quality scoreboard wiring | HealthCanvas's quality grid lies (same scores for every project). Spec §11.1 promises a live scorecard. |

These are platform-side fixes — none touch the builder app itself except to flip a stub off when the control plane is ready.

What does **not** block production (acceptable as deferred-section stubs for an alpha):

- ISS-10 / ISS-11 (sessions list / 2FA) — power-user features.
- ISS-13 (skill registry) — `/skills` ships as a marketing-honest static catalogue.
- ISS-17 / ISS-18 (incidents / performance metering) — empty-state copy is sufficient until a deploy with traffic exists.
- ISS-20 → ISS-25 (data canvas hardcoded data) — ships as a "preview mode" of the data plane until per-project schema introspection lands.
- ISS-22 (schema visualizer) — explicit V1.5 in the spec.

---

## Recommended next focus

Three concrete next-up tasks, in priority order. Each is a self-contained piece of work with clear "done" criteria.

### 1. Wire control-plane backing for the four highest-severity stubbed canvases (ISS-14 · ISS-15 · ISS-19 · ISS-26)

Why: these are the four "the canvas lies on a multi-node deploy" cases. Each one's `Fix path:` block in `ISSUES.md` is concrete and small (one new table, one set of REST handlers, swap in the proxied call in `agents.ts` / `apps.ts`).

Done criteria:
- `archived_at` column on `apps` + `PUT /api/apps/:id/archive`; `archiveApp` proxies through.
- `issues`, `issue_comments`, `milestones` tables + REST; `agents.ts` `listIssues` / `addIssue` proxy through.
- `deploys` table + `GET /api/apps/:id/deploys`; PlanCanvas Deployments section reads real history.
- `media` table + multipart upload to object store + URL minting; `agents.ts` `listMedia` / `uploadMedia` / `deleteMedia` proxy through.
- All four ISSUES entries marked `closed`.

### 2. Real cron for ISS-28 — PM digest + SRE monitor

Why: the dual-shape design from spec §4.8.3.2 is half-shipped. "Your AI PM checks in daily" is the line that takes the multi-agent story from "fancy chat" to "your project has a team". The chat-mode SubAgents are already done; only the trigger is missing.

Done criteria:
- `crates/control/src/scheduler.rs` ticks every minute, reads `apps.scheduled_jobs`, POSTs to the builder's `/_zs/v1/pm.digest` and `/_zs/v1/sre.monitor` procs.
- `agent_digests` table stores results; chat thread has an out-of-band write path for `data-pm-recommendation` / `data-sre-finding`.
- "Run digest now" button on PlanCanvas; "Scan health now" on HealthCanvas — both bypass the cron for ad-hoc use.
- Per-project opt-out + cadence editor in SettingsCanvas.

### 3. Plan 03 — admin canvas at `/admin/*`

Why: spec §23 lays out the surface; nothing is built. Without an admin canvas, every operational question (which projects are over their plan, what's the platform-wide error rate, what's the audit log say about user X) needs a database shell. The data shape is largely platform-side, but the dashboard is builder work.

Done criteria:
- Apps list (with creator / plan / status / scorecard avg / last deploy / MRR).
- App detail (project + scorecard history + recent deploys + audit log).
- Users list (with signups / plan / projects / MRR contribution).
- Revenue (MRR / ARR / churn / per-plan / platform fee).
- System health (workers / control plane / gateway / DB pool / request rate / error rate).
- Audit log (searchable; filter by actor + action).
- Role-gated by user record's `role: admin` flag.

### Optional: Plan 04 — branching + Data canvas wire-up (ISS-20 → ISS-25)

Why: spec §11.5 + Appendix A frame branching as the largest architectural addition after the agent fleet, and the Data canvas's branch switcher is the most prominent control on that canvas because operating on the wrong branch is the most-common foot-gun. This is a platform-heavy plan — `compio-postgres` schema namespacing, gateway host parsing, migration safety hard-gate — but the builder-side payoff is concrete: the Data canvas stops lying.

Done criteria: `compio-postgres` `BranchContext`; `app_<id>__<branch>` schemas; gateway `dev--{slug}` resolution; `pg_indexes` / `pg_class` introspection RPC; `agents.ts` `listTables` / `getTableRows` / `listIndexes` / `listMigrations` / `listBackups` all proxy through; ISS-20 → ISS-25 marked closed.

---

## Bundle audit (deferred)

Spec §25.5 calls for a ≤ 200 KB initial bundle. Today's bundle is larger because Monaco isn't yet lazy-loaded — but Monaco isn't actually used anywhere in this branch (FilesCanvas ships the read-only manuscript view, not an editor). Once Plan 03's admin canvas lands, audit and code-split.

---

## Honest framing

This branch is the editorial draft of zeroship-builder. The voice, the design system, the multi-agent wire, and the canvas surface area are all done. The data behind several canvases is held together with `Map<appId, …>` until the control plane catches up.

That's an acceptable shape for a closed beta — every surface is wireable in real-time as the platform fills in — but it's not the shape we want to invite a stranger into. ISS-09, ISS-12, ISS-14, ISS-15, ISS-19, ISS-26, ISS-28 are the seven that matter for "open to the public".

The branch is ready to merge. Plan 03 picks up the ISSUES.md catalogue.
