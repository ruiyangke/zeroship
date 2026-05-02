# Builder: full redesign + multi-agent fleet (Plan 01 foundation + Plan 02 finish)

**Branch:** `redesign/plan-01-foundation` → `master`
**Commits:** ~120 since fork
**TypeScript:** `tsc --noEmit` clean (0 errors)
**Tests:** 54 e2e across 13 spec files (~32 always-passing, ~22 env-gated)

---

## Summary

This branch replaces the stub builder app under `apps/zeroship-builder/` with the full design called out in `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md`. The chat surface goes from a static mock to a real OpenAI-backed deepagents runtime with a four-member SubAgent fleet (Critic / Reviewer / PM / SRE), a sandbox-backed file plane, an `interrupt()`-driven survey wire shared between the wizard and Builder, and a translator that converts deepagents/LangGraph events into AI SDK v6 UI Message Stream chunks.

The workspace ships the entire pill-driven canvas layer — preview · files · data · media · logs · env · plan · health · settings — with each canvas wrapped in its own `ErrorBoundary`. The public marketing tree (8 surfaces), the auth shell (login · signup · forgot · account), and the §7 onboarding chain (intent picker → first-run hint → first-deploy celebration → product tour) all land. A responsive sweep, an a11y pass, and an editorial empty-state pass complete the polish layer.

What didn't land: real backing for ~13 control-plane-side features (skill registry, deploy history, telemetry pipeline, real `pg_dump`, scheduled-worker cron, …). Each gap is tracked as a self-contained `ISS-XX` entry in `ISSUES.md` with severity, symptom, workaround, and `Fix path:` block. The branch is shippable as an alpha — see `docs/zeroship-builder-status.md` for the full maturity assessment.

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
- Plan: Issues / Roadmap / Deployments (in-memory stub, ISS-14 / ISS-15).
- Health: Status / Quality / Incidents / Performance (hardcoded scorecard, ISS-16 / ISS-17 / ISS-18).
- Data: 5 sub-tabs (Tables / Schema / Indexes / Migrations / Backups) over `SAMPLE_*` constants (ISS-20 → ISS-25).
- Media: drop-zone grid (base64 data URLs, ISS-26).

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
- Skippable 4-step product tour triggered from the TopBar `?` button.

**Project archive**
- UI in `SettingsCanvas` + filter pill on Home.
- In-memory `Set<string>` (ISS-19); state lost on server restart.

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

**Inventory:**

```
auth.spec.ts                 4 tests   no env required
chat-actions.spec.ts         8 tests   no env required (mock chat path)
chat-openai.spec.ts          3 tests   OPENAI_API_KEY required
critic-loop.spec.ts          1 test    OPENAI_API_KEY required
data-media.spec.ts           4 tests   control plane required
lifecycle.spec.ts            3 tests   no env required
multi-agent.spec.ts          3 tests   OPENAI_API_KEY required
onboarding.spec.ts           6 tests   no env required
plan-health.spec.ts          3 tests   control plane required
public-pages.spec.ts        11 tests   no env required
spine-openai.spec.ts         2 tests   OPENAI_API_KEY + control plane
wizard-openai.spec.ts        2 tests   OPENAI_API_KEY required
workspace-canvases.spec.ts   4 tests   control plane required
─────────────────────────── 54 tests
```

`tsc --noEmit` is clean. ~32 tests always pass without env. ~22 are gated and skip cleanly when their env isn't set.

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

- `docs/zeroship-builder-spec-compliance.md` — section-by-section spec walkthrough table
- `docs/zeroship-builder-status.md` — branch maturity assessment
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

**Tool-call rendering taxonomy (G4).** `write_file` / `edit_file` / `ask_survey` / `task` → custom `data-*` parts via middleware (UI shape ≠ I/O shape). `ls` / `read_file` / `grep` / `glob` / `execute` → native AI SDK v6 `tool-call` / `tool-result` chunks via the translator. The translator skips middleware-handled tool names so the wire shows one card, not two.

---

## Reviewer suggestions

1. **Read the spec compliance table first** (`docs/zeroship-builder-spec-compliance.md`) — it gives you the section-by-section verdict in 5 minutes.
2. **Walk `ISSUES.md` next** — every deferral has a workaround note + `Fix path:` block.
3. **Spot-check the translator** (`_translator.ts`) and one SubAgent (e.g. `_critic.ts`) to convince yourself the deepagents wire is honest.
4. **Run the no-env tests** to confirm nothing is broken at the surface level: `cd apps/zeroship-builder && npm run test:e2e`.
5. **If you have an `OPENAI_API_KEY`**, run `e2e/critic-loop.spec.ts` to see the `task("critic")` while-loop in action.
