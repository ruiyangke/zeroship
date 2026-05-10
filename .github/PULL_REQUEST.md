# Builder: full redesign + multi-agent fleet (Plan 01 foundation + Plan 02 finish)

**Branch:** `redesign/plan-01-foundation` → `master`
**Commits:** ~150 since fork
**TypeScript:** `tsc --noEmit` clean (0 errors)
**Tests:** 221 e2e across ~25 spec files — **all passing** with `OPENAI_API_KEY` + control plane up; ~150 always-passing without env, the rest skip cleanly when their env isn't set
**Production build:** clean (`vite build` → `dist/app.zship` 3.91 MB, 559 blobs)

---

## Summary

This branch replaces the stub builder app under `apps/zeroship-builder/` with the full design called out in `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md`. The chat surface goes from a static mock to a real OpenAI-backed deepagents runtime with a four-member SubAgent fleet (Critic / Reviewer / PM / SRE), a sandbox-backed file plane, an `interrupt()`-driven survey wire shared between the wizard and Builder, and a translator that converts deepagents/LangGraph events into AI SDK v6 UI Message Stream chunks.

The workspace ships the entire pill-driven canvas layer — preview · files · data · media · logs · env · plan · health · settings — with each canvas wrapped in its own `ErrorBoundary`. The public marketing tree (8 surfaces), the auth shell (login · signup · forgot · account), and the §7 onboarding chain (intent picker → first-run hint → first-deploy celebration → product tour) all land. A responsive sweep, an a11y pass, and an editorial empty-state pass complete the polish layer.

What didn't land: real backing for ~13 control-plane-side features (skill registry, deploy history, telemetry pipeline, real `pg_dump`, scheduled-worker cron, …). Each gap is tracked as a self-contained `ISS-XX` entry in `ISSUES.md` with severity, symptom, workaround, and `Fix path:` block. The branch is shippable as an alpha — see `docs/superpowers/zeroship-builder-status.md` for the full maturity assessment.

---

## What's new

**Spine**
- New routing tree: `/` is the public marketing landing; `/home` is the authed gallery; `/p/:appId/*` is the workspace shell behind `AuthGuard`.
- Full home → wizard → `createApp` → workspace flow, with `seedBrief` consumed from `sessionStorage` on workspace mount.
- Two-runtime split: wizard is plain LangGraph (no sandbox); Builder is deepagents (`createDeepAgent({...})`).

**Builder runtime**
- Real OpenAI streaming via the translator at `src/server/_translator.ts`.
- In-memory `MemorySaver` checkpointer keyed by useChat session id (G1).
- Sandbox backend (`SandboxBackendProtocolV2`) wired against `crates/sandbox/`.
- `ask_survey` tool calls `interrupt()` directly; resume protocol (`body.json.resume` → `Command({resume})`) shared with wizard.
- AbortSignal propagation: SSE cancel aborts the in-flight LLM call (G2).
- UI-message → LangChain converter preserves tool-call / tool-result history across turns (G7).

**Multi-agent fleet**
- Critic SubAgent with `responseFormat: criticResponseSchema` (7 quality dimensions).
- Reviewer SubAgent (pre-deploy hard gate, structured pass/block).
- PM SubAgent (chat-mode product manager) + `pmDigest` worker proc (background dual shape).
- SRE SubAgent (chat-mode reliability) + `sreMonitor` worker proc (background dual shape).
- `wrapToolCall` middleware emits `data-critic-round` / `data-reviewer-round` / `data-pm-recommendation` / `data-sre-finding` UI parts; client renders dedicated cards.

**Canvases (8 pills)**
- Preview · Files · Data · Media · Logs · Env · Plan · Health · Settings — all mounted from `WorkspaceShell.tsx`, each in its own `ErrorBoundary`.
- Files: read-only 3-col view sourced from sandbox HTTP procs.
- Logs: filtered ledger with 2s poll + sticky-bottom.
- Env: variables + secrets (real control-plane backing).
- Settings: identity · plan picker · archive · danger zone.
- Plan: Issues / Roadmap / Deployments — KV-backed issues + "Run digest" CTA wired to the PM SubAgent (ISS-14 partial, ISS-15 still in-memory).
- Health: Status / Quality / Incidents / Performance — quality grid is now **live from the Critic** (KV-persisted via `setQualityFromCritic` on every `data-critic-round` emit, ISS-16); Incidents has "Scan for issues" CTA wired to the SRE SubAgent (ISS-17 stays partial); Performance section now derives **real signals from log lines** (request rate / error rate / p95 latency) with hand-rolled SVG sparklines (ISS-18 partial).
- Data: 5 sub-tabs (Tables / Schema / Indexes / Migrations / Backups) over `SAMPLE_*` constants (ISS-20 → ISS-25); KV-backed media + backups list.
- Media: drop-zone grid (base64 data URLs persisted to KV, ISS-26).
- Files: shiki syntax highlighting (resolves ISS-27).

**Public surfaces**
- Marketing landing · Pricing (3 plans + 15 % share band with worked example) · Skills (static catalogue) · Templates · About · Changelog · Privacy · Terms.
- Public nav with sticky bottom CTAs.
- Routes split: `/` is unauthed-friendly; `/home` is authed.

**Auth**
- Login · Signup · ForgotPassword · Account.
- `AuthGuard` wrapping `/home`, `/p/:appId/*`, `/account`, and the catch-all.
- Account page renders deferred-section stubs for Sessions (ISS-10), 2FA (ISS-11), Delete (ISS-12) — each pointing at its tracked issue.

**Onboarding**
- `/onboarding/intent` picker (six chips per spec §7.1).
- First-run hint on the `/new` wizard composer.
- First-deploy celebration banner (`<LiveBanner>`) gated on `deploy_hash` transition + per-app localStorage flag.
- 4-step product tour with **surface highlighting** — each step targets a real DOM surface by testid (chat-composer, canvas-pills, topbar-url), draws a 4px outlined frame around it via the box-shadow inset trick, floats the tooltip adjacent with viewport-clamped fallback sides. Esc closes; ←/→ navigate; Prev disables on step 1; backdrop click skips. 4 e2e tests verify the full walk.

**Project archive**
- UI in `SettingsCanvas` + filter pill on Home.
- KV-backed `Set<string>` (survives HMR / isolate eviction within a single process; multi-node consistency still tracked under ISS-19).

**Tier toggle (§1.5)**
- Editorial chip next to canvas pills cycles Maker → +Data → +Code; choice persists in localStorage; active pill snaps to "preview" if a tier change hides it.

**Persistence layer**
- `_persist.ts` wraps `@zeroship/kv` with a same-process `Map` fallback. Internal helpers tagged `_internal.<name>` (ISS-02 mitigation) so the prod build's manifest emitter accepts them.
- Issues, archive set, quality scorecard, media, backups all moved off bare module-level `Map`s onto KV.

**Analytics + telemetry (§28)**
- `lib/analytics.ts` `track()` ring-buffered emitter + `subscribeEvents`.
- `DevEventsBadge` floating popover (DEV-only, bottom-left) shows the last 50 events live so devs can verify `track()` calls without console-diving.

**Chat polish**
- Hover actions: copy / regenerate / edit-prior.
- @-mention dropdown for files / issues / recent errors.
- Markdown rendering (react-markdown + remark-gfm).
- Inline retry on failed turns.

**Scheduled workers**
- `pmDigest({appId})` + `sreMonitor({appId})` procs reachable via the existing RPC wire (ISS-28 tracks the missing platform scheduler).

**Polish layer**
- Responsive sweep (375 / 768 / 1280); phone collapses chat into a drawer.
- A11y pass: focus-visible ring, modal focus management, aria-labels everywhere.
- Editorial empty states across every canvas, gallery, and filter page.
- Root + per-canvas `ErrorBoundary`.

---

## Scope deferred

All deferred work is tracked in `ISSUES.md`. Quick index:

- **ISS-01** · `node:async_hooks` / `AsyncLocalStorage` not propagated by `@zeroship/vite-plugin` — workaround in use (two-node `decide → act` split in `_wizard.ts`).
- **ISS-02** · `@zeroship/vite-plugin` registers every exported function as RPC — workaround: underscore-prefix internal files.
- **ISS-09** · `/auth/forgot-password` endpoint not exposed by control plane — UI ships as no-enumeration stub.
- **ISS-10** · `/auth/sessions` list / revoke endpoints not exposed — Account page renders deferred-section stub.
- **ISS-11** · 2FA / TOTP enrollment not wired — deferred-section stub.
- **ISS-12** · Account-deletion endpoint not exposed — deferred-section stub.
- **ISS-13** · Skill registry not implemented — `/skills` ships as static catalogue.
- **ISS-14** · Issues table missing — PlanCanvas reads in-memory `Map<appId, Issue[]>`.
- **ISS-15** · Deploy-history table missing — PlanCanvas shows current deploy only.
- **ISS-16** · Critic → quality scoreboard wiring missing — HealthCanvas renders hardcoded scores.
- **ISS-17** · Incidents table missing — HealthCanvas Incidents shows empty state.
- **ISS-18** · Performance metering pipeline missing — HealthCanvas Performance shows placeholder tiles.
- **ISS-19** · Project archive — control-plane backing missing — in-memory `Set<string>`.
- **ISS-20** · `pg_catalog` table introspection missing — DataCanvas Tables list is hardcoded.
- **ISS-21** · Table row pagination over real per-app schema missing.
- **ISS-22** · Schema visualizer not built — Schema sub-tab is a placeholder.
- **ISS-23** · Index introspection (`pg_indexes`) missing — Indexes tab is hardcoded.
- **ISS-24** · Migration log not persisted — Migrations tab is hardcoded.
- **ISS-25** · Backup trigger + history not wired to actual `pg_dump`.
- **ISS-26** · Media canvas backed by in-memory Map, not real `@zeroship/storage`.
- **ISS-28** · Cron / scheduled-worker harness missing — PM digest + SRE monitor are RPC-only.

---

## Tests

`tsc --noEmit` is clean. **221 e2e tests passing** with `OPENAI_API_KEY` + control plane up; ~150 always pass without env, the rest skip cleanly when their env isn't set.

Highlights since the original Plan 02:

- **Real-LLM full-spine** (`e2e/full-spine-real.spec.ts`) — landing → wizard → survey → brief → Begin → workspace → seeded user msg → assistant reply, against the live OpenAI API. Surfaced four wire bugs that are now fixed (commit `dd3f2f30`).
- **Critic iteration loop** (`e2e/critic-loop.spec.ts`) — exercises the `task("critic", …)` while-loop end-to-end with real LLM dispatch.
- **Multi-agent deep flows** (`e2e/multi-agent.spec.ts` + Reviewer / PM / SRE deep specs) — each SubAgent verified to fire on its trigger and emit its data-part shape.
- **Foundation-polish spec** (`e2e/foundation-polish.spec.ts`) — tier filter, KV persistence, run-digest, mobile wizard, dev-events-badge.
- **Product tour** (4 new tests in `e2e/onboarding.spec.ts`) — opens, walks all 4 steps with surface highlighting visible, Esc closes, Skip closes, Prev navigation.
- **Health + Plan canvases** (`e2e/plan-health.spec.ts`) — three sections + four-section verifies; sparklines under the new Performance section.

---

## How to run

**Local dev (no LLM):**

```bash
cd apps/zeroship-builder
npm install
npm run dev          # vite at http://localhost:5173
npm run test:e2e     # 32 always-passing + 22 skipped (no env)
```

**With OpenAI streaming:**

```bash
export OPENAI_API_KEY=sk-...
npm run test:e2e     # all 54, except control-plane-gated ones
```

**With control-plane + sandbox:**

```bash
# In repo root, three terminals:
zeroship-control --port 9090 --db postgres://... --bundles ./bundles --auth-secret <s>
zeroship-worker  --port 8080 --workers 16 --control http://localhost:9090
zeroship-gate    --port 80   --control http://localhost:9090 --workers http://localhost:8080 --auth-secret <s>

# Then in apps/zeroship-builder:
export SANDBOX_URL=http://localhost:7000
export SANDBOX_TOKEN=...
export OPENAI_API_KEY=sk-...
npm run test:e2e     # all 54
```

---

## Files of interest

For reviewers picking a starting point:

- `docs/superpowers/zeroship-builder-spec-compliance.md` — section-by-section spec walkthrough table
- `docs/superpowers/zeroship-builder-status.md` — branch maturity assessment
- `docs/superpowers/plans/2026-05-01-zeroship-builder-plan-02-builder-finish.md` — retroactive plan covering the ~120 commits
- `ISSUES.md` — 28 tracked deferrals with `Fix path:` blocks
- `apps/zeroship-builder/src/server/_translator.ts` — the deepagents → AI SDK v6 seam
- `apps/zeroship-builder/src/server/_wizard.ts` — plain-LangGraph wizard (decide / act split)
- `apps/zeroship-builder/src/server/_middleware.ts` — `wrapToolCall` data-part emitter
- `apps/zeroship-builder/src/server/_sandbox_backend.ts` — `SandboxBackendProtocolV2` adapter
- `apps/zeroship-builder/src/server/{_critic,_reviewer,_pm,_sre}.ts` — SubAgent fleet
- `apps/zeroship-builder/src/server/_prompts.ts` — system prompts (extracted per G12)
- `apps/zeroship-builder/src/client/api.ts` — typed RPC client + `chatTransport` / `wizardTransport`
- `apps/zeroship-builder/src/client/workspace/WorkspaceShell.tsx` — canvas + chat layout
- `apps/zeroship-builder/src/client/workspace/canvases/` — the 8 canvases
- `apps/zeroship-builder/e2e/` — 13 spec files

---

## Trade-offs / decisions

**Two server runtimes (wizard plain LangGraph; Builder deepagents).** The wizard runs *before* a sandbox or project exists. Loading deepagents and provisioning a sandbox per visitor would be wasteful. The two runtimes share the AI SDK v6 wire format on the way out — same `data-survey` chunk, same `<SurveyCard>`, same resume protocol — so the runtime decision is invisible to the client. Spec §4.8.2b documents the cost test for any future runtime.

**Two-node `decide → act` workaround for ISS-01.** `interrupt()` from `@langchain/langgraph` requires a working `node:async_hooks` `AsyncLocalStorage` whose `.run(value, cb)` survives `await` boundaries — including continuations from native (Rust-implemented) async work like `fetch`. The zeroship V8 runtime's `async_hooks` shim doesn't survive native fetch resumption today. Workaround: split any node that needs both an `await model.invoke(...)` and an `interrupt()` into two nodes — `decide` does the fetch, `act` calls `interrupt()` with no awaits before. The spec annotates this in `_wizard.ts`'s top comment block. Builder is naturally fine because deepagents separates LLM calls from tool bodies into different RunnableCallables.

**`gpt-5.4-mini` family parity across all SubAgents.** Per-agent model differentiation defers until cost telemetry justifies it. Critic running 3× per turn is the heaviest spend; parity ensures Critic doesn't mis-grade Builder's output. Spec §4.8.9 G6.

**Single-input RPC wire.** Every server proc takes one object input (`{appId, ...}`). The vite-plugin's RPC discovery loop forwards `args[0]` only, so this is the only safe shape for multi-arg procs. ISS-02 traces the missing opt-in marker that makes this convention fragile.

**In-memory checkpointer.** `MemorySaver` is process-local and dev-only. Production needs a Postgres-backed `BaseCheckpointSaver`. Deferred to control-plane work.

**KV instead of Postgres for stub state.** The night's persistence sweep moved issues / archive set / quality scorecard / media / backups onto `@zeroship/kv` so they survive HMR and isolate eviction within a single process. KV is in-memory in dev (no real cluster yet), so multi-node is still inconsistent — that's tracked in the relevant ISSUES.md entries (ISS-14 / ISS-15 / ISS-19 / ISS-26). The `_persist.ts` wrapper falls back to a process-local Map when KV throws, so the canvas always renders something.

**Internal helper id convention (ISS-02).** The vite-plugin's manifest emitter refuses prod builds when any exported procedure lacks an explicit `.config.id`. To unblock prod builds without changing the plugin's discovery loop, each underscore-prefixed internal helper now ships with `<name>.config = { id: "_internal.<name>" }`. The `_internal.` prefix lets reviewers and a future kernel-side filter spot/skip them. Plugin-side opt-out marker is still the right fix.

**Health perf via log parsing.** Rather than wait for the structured metering pipeline (ISS-18 fix path), the Performance section now derives request-rate / error-rate / p95-latency from log lines via loose regexes (`HTTP_METHOD_RE`, `ERROR_RE`, `LATENCY_RE`). It's best-effort by design — apps that don't log in a method/latency-ms style will show zeros, which is honest. Sparkline is hand-rolled SVG (single `<polyline>`) — no chart-lib dependency.

**Tool-call rendering taxonomy (G4).** `write_file` / `edit_file` / `ask_survey` / `task` → custom `data-*` parts via middleware (UI shape ≠ I/O shape). `ls` / `read_file` / `grep` / `glob` / `execute` → native AI SDK v6 `tool-call` / `tool-result` chunks via the translator. The translator skips middleware-handled tool names so the wire shows one card, not two.

---

## Reviewer suggestions

1. **Read the spec compliance table first** (`docs/superpowers/zeroship-builder-spec-compliance.md`) — it gives you the section-by-section verdict in 5 minutes.
2. **Walk `ISSUES.md` next** — every deferral has a workaround note + `Fix path:` block.
3. **Spot-check the translator** (`_translator.ts`) and one SubAgent (e.g. `_critic.ts`) to convince yourself the deepagents wire is honest.
4. **Run the no-env tests** to confirm nothing is broken at the surface level: `cd apps/zeroship-builder && npm run test:e2e`.
5. **If you have an `OPENAI_API_KEY`**, run `e2e/critic-loop.spec.ts` to see the `task("critic")` while-loop in action.
