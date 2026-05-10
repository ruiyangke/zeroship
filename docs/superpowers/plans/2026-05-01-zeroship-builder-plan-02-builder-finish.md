# zeroship-builder Plan 02 — Builder Finish (retroactive)

> **Note:** this is a *retroactive* plan document. The work it describes has already shipped on `redesign/plan-01-foundation`. Unlike Plan 01, the steps below are not instructions for an agent — they are the historical record of how Plan 02 was executed in roughly five logical phases (A-E) across ~120 commits between 2026-04-30 and 2026-05-01.
>
> Reading order if you only want one document: skip to **§Phase E summary** for the end state, then walk back through earlier phases when you need to understand a specific subsystem.

**Goal:** carry the Plan 01 mock workspace from "static shell + mock chat" to a fully-wired multi-agent builder with real OpenAI streaming, the full canvas surface, the public marketing tree, the authentication shell, and the editorial polish layer (responsive · a11y · empty states · ErrorBoundary).

**End state (matches branch HEAD):**
- 0 errors on `tsc --noEmit`.
- 54 e2e tests across 13 spec files. ~32 always-passing; ~22 gated on `OPENAI_API_KEY` / control-plane / sandbox env.
- 8 canvases mounted; 9 canvas pills wired (preview + 8 named).
- 8 public surfaces (`/`, `/pricing`, `/skills`, `/templates`, `/about`, `/changelog`, `/legal/privacy`, `/legal/terms`).
- 4 auth surfaces (`/login`, `/signup`, `/forgot-password`, `/account`).
- 4 multi-agent fleet members shipped (Critic, Reviewer, PM, SRE) wired through deepagents `task` + `wrapToolCall` middleware.
- 28 ISSUES.md entries tracking deferred backend work.

**Spec sections covered (Plan 02 only):** §2 multi-agent · §3.1 IA · §4.8 implementation libraries (full) · §5 pre-auth surfaces · §6 auth flows · §7 onboarding · §8 project lifecycle · §9 workspace canvases · §10 chat polish · §11.1 Critic loop · §13/§14 PM/SRE chat-mode · §25 cross-platform · §26 empty/loading/error · §27 voice · §28 telemetry.

**What's deliberately deferred (V1 not in this plan):**
- Real backing for ISS-09 → ISS-28 (control-plane handlers, telemetry pipeline, cron scheduler, real `pg_dump`, etc.). Each issue carries a `Fix path:` block.
- Plan 03+: admin canvas, theme migration skill, feature-set installer, billing / payouts UI, notifications center, ⌘K search.

---

## Phase A · Spine: home → wizard → createApp → workspace

**What landed:** the chassis that turns a stranger's prompt into a live `/p/:appId` workspace. Before Phase A, `App.tsx` mounted the workspace shell at `*` and the chat was a mock. After Phase A, `/` is the marketing landing for unauthed visitors, `/home` is the authed gallery, `/new` is a chat-style wizard, Begin calls `createApp` and stashes the brief in sessionStorage, and `/p/:appId/preview` is the workspace shell.

**Commits in this phase:**

```
7c6e4f0a   checkpoint: pre-Plan-01 builder WIP + design specs + plan
288bff1d   builder: bump AI SDK to v6, add @zeroship/{rpc-client,server}
a506133c   builder: add api.ts (rpc + chatTransport) mirroring example
7970de38   builder: rewrite chat.ts as v6 UIMessageStream Response (text-only mock)
d0f2ab5a   builder: rewrite ChatRail/ChatMessages on v6 useChat + chatTransport
8cffb08c   builder: drop mock middleware + simplify vite/playwright configs
4409d3e2   builder: update e2e for v6 text-only mock chat
1cd8a0c1   wizard: plain-LangGraph project-creation runtime
a4e14ba5   wizard: client surface at /new — chat-style brief refinement
06207906   client: re-export server procedures from api.ts; mount AuthProvider
ee58f561   wizard: Begin → createApp + sessionStorage stash
ce2c1a39   workspace: /p/:appId route, app fetch, brief seed
dca233fa   home: wire / route to existing project gallery
f5149264   e2e: full home → wizard → workspace spine
```

**What this carved out:**
- **Two server runtimes.** The wizard runs *before* a project exists; loading deepagents and provisioning a sandbox per visitor is wasteful, so the wizard is a hand-rolled plain-LangGraph `StateGraph` (`src/server/_wizard.ts`, ~430 LOC). Builder is `createDeepAgent({...})` (`src/server/_translator.ts`). They share the AI-SDK v6 wire format on the way out — same `data-survey` chunk shape, same `<SurveyCard>`, same resume protocol. Spec §4.8.2b documents the cost test that decides which runtime fits new agents.
- **Two-node wizard architecture.** ISS-01 (`AsyncLocalStorage` not propagated by the V8 runtime past native fetch) forced the wizard into a `decide → act` split. `decide` calls `model.invoke()` and stashes the decision in graph state; `act` reads it and calls `interrupt()` with no awaits before. Workaround documented at the top of `_wizard.ts`.
- **Single-input RPC wire.** Every server proc takes one object input (`{appId, ...}`). The vite-plugin's RPC discovery loop forwards `args[0]` only, so this is the only safe shape if we want multi-arg procs (ISS-02 traces the related opt-in-marker gap).
- **AI SDK v6 chat transport.** `client/api.ts` exports `chatTransport` and `wizardTransport` factories that wrap `DefaultChatTransport`'s `prepareSendMessagesRequest` to ship `{json: {messages, id}}` on a fresh turn and `{json: {resume, id}}` on a SurveyCard submit.

**What didn't land in Phase A:**
- Real LLM calls (Phase B).
- Sandbox-backed file writes (Phase B).
- Critic / Reviewer / PM / SRE fleet (Phase B).

---

## Phase B · Builder runtime: real OpenAI, deepagents, multi-agent fleet

**What landed:** the chat surface goes from text-only mock to a real deepagents agent talking to OpenAI, with sandbox-backed file ops, structured `responseFormat` SubAgents (Critic / Reviewer / PM / SRE), `wrapToolCall` middleware that emits `data-*` UI parts, the `ask_survey` tool with shared resume protocol, and end-to-end smoke tests gated on `OPENAI_API_KEY`.

**Commits in this phase:**

```
56bc9fad   builder: add @ai-sdk/openai dep
c54f684e   builder: add server-side stream translator (deepagents -> AI SDK v6)
55fc807d   builder: replace mock chat with deepagents+OpenAI via translator
86d1dcf6   builder: gate Builder e2e on OPENAI_API_KEY
7fba339d   builder: switch model to gpt-5.4-mini (gpt-5-nano doesn't stream)
bad91812   builder: delete orphan e2e tests + fix teardown noise
daef90df   spec: revise §4.8 for deepagents-native architecture
37e5d303   spec: §4.8.9 — known gaps and follow-ups (post-Phase-A review)
0f219256   builder: plumb AbortSignal through translator → streamEvents          [G2]
bb965ff0   builder: add in-memory checkpointer; thread_id from useChat session id [G1]
e354c216   builder: implement resume-after-interrupt server protocol             [G3]
d93beaca   builder: extend UIMessage→LangChain converter for tool-call/result    [G7]
dc7bc00c   spec: §4.8.3.4 — verify AnyBackendProtocol against actual deepagents  [G10]
8c5055e7   builder: extract _prompts.ts; standardise zod via @zeroship/server    [G12+G14]
2f7fcd41   builder: add SandboxBackendProtocolV2 adapter against sandbox controller
afb7726f   builder: wire SandboxBackend into createDeepAgent
8cc053f6   builder: tell Builder it has fs/exec tools in the system prompt
2a0f3688   builder: .env.example — add SANDBOX_URL / SANDBOX_TOKEN
bba5a6ee   builder: add Critic SubAgent (gpt-4o-mini + structured responseFormat)
f939d221   builder: wire Critic SubAgent into createDeepAgent
10c07a66   spec + builder: standardise on gpt-5.4-mini across all SubAgents      [G6]
987301e6   builder: add wrapToolCall middleware emitting data-diff for write_file
ea4175f5   builder: tighten BUILDER_SYSTEM — insist on tool-first behaviour
c924d9ce   builder: extend ChatMessages to render tool-call + data-diff parts
b10c1618   builder: ask_survey tool + shared survey wire + client resume
52d1f288   builder: BUILDER_SYSTEM directs critic loop after writes
d9c474f5   builder: emit data-critic-round when task("critic", …) returns
3b23b32d   client: render data-critic-round as CriticRoundCard in ChatMessages
3e8f5367   e2e: critic loop smoke test
71d1cf31   builder: add Reviewer SubAgent (pre-deploy hard gate)
5e4d8429   builder: add PM SubAgent (chat-mode product manager)
b854b17a   builder: add SRE SubAgent (chat-mode reliability engineer)
de18f011   builder: wire Reviewer / PM / SRE into deepagents + middleware
846f415d   builder: client cards for Reviewer / PM / SRE rounds
40c20915   e2e: multi-agent fleet smoke (Reviewer / PM / SRE)
e7c920a1   agents: PM digest + SRE monitor scheduled-worker procs
56b27a7e   docs: ISS-28 in ISSUES.md (cron / scheduled-worker harness)
```

**What this carved out:**
- **Phase B.0 preflight gaps closed** — G1 (in-memory `MemorySaver` checkpointer keyed by `thread_id` = useChat session id), G2 (`AbortSignal` plumbed via stream `cancel()` since the RPC fast path doesn't expose `request.signal`), G3 (resume-after-interrupt: server detects `body.json.resume` and feeds `new Command({resume: value})` into the same thread), G7 (UI-message → LangChain converter preserves `tool-call` / `tool-result` parts so Builder doesn't repeat tool calls), G10 (verified `AnyBackendProtocol` against `node_modules/deepagents/dist/index.d.ts`; we target V2 with V1 adapter for back-compat), G12 (extracted `_prompts.ts`), G14 (standardised `z` from `@zeroship/server`).
- **Sandbox backend** — `_sandbox_backend.ts` (314 LOC) implements `SandboxBackendProtocolV2` over HTTP to `crates/sandbox/`. `id` is per-project (`proj_<id>`). All methods async. `filesUpdate: null` everywhere — the sandbox owns the bytes.
- **Critic loop** — Builder calls `task("critic", { changes })` after each commit; the SubAgent returns structured `{approved, issues[]}` via Zod `responseFormat`. Builder's planning is told (via `BUILDER_SYSTEM`) to iterate until approved or N=3 rounds. The "loop" is a plain JS while inside Builder's planning, not a custom LangGraph cycle. `wrapToolCall` middleware emits `data-critic-round` when `task("critic", …)` returns.
- **Multi-agent fleet** — Critic / Reviewer / PM / SRE all share the same shape: `name`, `description`, `systemPrompt` (from `_prompts.ts`), `model: "openai:gpt-5.4-mini"`, `tools: []`, `responseFormat: z.object(...)`. PM and SRE also ship as standalone scheduled-worker procs (`pm_worker.ts` `pmDigest`, `sre_worker.ts` `sreMonitor`) for the dual chat-mode + background pattern from spec §4.8.3.2 — but no scheduler fires them yet (ISS-28).
- **`ask_survey` shared wire** — `_tools.ts` `askSurveyTool` calls `interrupt({survey})` directly (not deepagents' `interruptOn` config — the `humanInTheLoopMiddleware` resume shape is too narrow). Wizard does the same via its `clarifierNode`. Both runtimes emit identical `data-survey` chunks; the same `<SurveyCard>` renders.
- **Tool-call rendering rule (G4)** — write_file / edit_file / ask_survey / task → `data-*` custom parts via middleware; ls / read_file / grep / glob / execute → native AI SDK v6 `tool-call` / `tool-result` chunks via the translator's `on_tool_start` / `on_tool_end` events. The translator skips middleware-handled tools so the wire shows one card, not two.

**What didn't land in Phase B:**
- Postgres-backed checkpointer (in-memory `MemorySaver` only).
- Critic dimension scoring persisted to a `quality_scores` table (ISS-16).
- Background scheduler for PM digest / SRE monitor (ISS-28).
- LangSmith / observability wiring (G13 deferred).

---

## Phase C · Canvases: Files / Logs / Env / Settings + Plan / Health + Data / Media

**What landed:** all 8 named canvases. The workspace shell drops the orphan tabs tree from Plan 01 and mounts canvases keyed by the `CanvasPills` active state, each wrapped in a per-canvas `ErrorBoundary`. Eight canvases over ~3000 LOC of canvas code, all reading from server procs (real or in-memory).

**Commits in this phase:**

```
9bcbe1b1   sandbox: listSandboxFiles + readSandboxFile procs (object input)
d7ee9d68   canvas: FilesCanvas — read-only manuscript view (3-col)
645f494a   canvas: LogsCanvas — filtered ledger with 2s poll + sticky-bottom
dfd35f1e   canvas: EnvCanvas — variables + secrets sections
bb02b7c5   canvas: SettingsCanvas — identity, plan picker, danger zone
cd6fdc57   workspace: mount Files/Logs/Env/Settings canvases; drop orphan tabs tree
b809efa0   e2e: workspace-canvases smoke (4 canvases, control-plane-gated)
68272dfd   workspace canvases: Plan + Health (spec §9.8 + §9.9)
97bad93f   ISSUES + e2e: track Plan/Health backing gaps (ISS-14 → ISS-18)
7d2228f2   data/media: server stubs + canvas pill list (ISS-20 → ISS-26)
44a0c930   client: re-export Data/Media procs from api.ts
8928ed8b   canvas: DataCanvas (5 subtabs over agents.ts stubs)
f3171ccd   canvas: MediaCanvas (drop zone + grid)
87ad52b8   workspace: mount Data + Media canvases
281649f4   e2e: data + media canvases smoke
2d7d2b00   docs: ISS-20 → ISS-26 in ISSUES.md
```

**Per-canvas verdict:**

| Canvas | Spec | LOC | Backing | Status |
|---|---|---|---|---|
| Files | §9.2 | 395 | sandbox HTTP procs | partial — read-only view; no Monaco / save / context menu |
| Logs | §9.5 | 200 | `getAppLogs` (control plane) | partial — 2s poll + sticky-bottom; no SSE |
| Env | §9.6 | 347 | env-var procs (control plane) | shipped |
| Settings | §9.7 | 363 | apps procs + archive Set | partial — archive in-memory (ISS-19); custom-domain placeholder |
| Plan | §9.8 | 473 | `agents.ts` in-memory (ISS-14, ISS-15) | stubbed |
| Health | §9.9 | 270 | hardcoded scores (ISS-16, ISS-17, ISS-18) | stubbed |
| Data | §9.3 | 598 | `SAMPLE_*` constants (ISS-20 → ISS-25) | stubbed |
| Media | §9.4 | 296 | base64 data URLs in Map (ISS-26) | stubbed |

**What this carved out:**
- **Wire convention.** Every server proc takes `{appId, ...}` so the single-input RPC plumbing stays honest even when we add `addIssue({appId, title, description})`. Documented in `agents.ts` header.
- **In-memory storage.** All stubbed canvases share a `Map<appId, …>` per resource. Survives only the V8 isolate's lifetime — that's intentional, ISSUES entries trace each gap to its `Fix path:`.
- **ErrorBoundary per pane.** A render crash in one canvas doesn't blank the workspace; the user can switch tabs out of the broken one (`WorkspaceShell.tsx` lines 209-241).
- **Re-exports through `client/api.ts`.** The client never imports a server module directly — every proc is re-exported from `api.ts` so the vite-plugin's RPC transform fires on the import chain.

**What didn't land in Phase C:**
- Real iframe Preview canvas with device-frame switcher (still a stub).
- Branch switcher in Data canvas (platform-side dependency on `compio-postgres` schema namespacing).
- Schema visualizer (ISS-22 deferred V1.5).

---

## Phase D · Public surfaces + auth + onboarding

**What landed:** all 8 public marketing surfaces from spec §5; the four auth surfaces from §6; the §7 onboarding chain (intent picker → first-run hint → first-deploy celebration → product tour); the `AuthGuard` route gating; project archive (UI in `SettingsCanvas` + `Home` filter pill).

**Commits in this phase:**

```
31a96966   auth: ForgotPassword page (UI stub, no-enumeration reply)
0b2d4cdc   auth: AuthGuard + route wiring (login/signup/forgot public; …)
f53f005f   account: identity + plan + deferred sections + sign out → /login
c0ee5bf4   e2e: auth UI surfaces smoke (login/signup/forgot/account)
b25d4db5   marketing: landing page + PublicNav (per spec §5.1)
56867295   pricing: three-tier plan grid + 15% share band (per spec §5.2)
6e8cb245   skills: static catalogue + filter pills (per spec §5.3, ISS-13)
ff29556b   templates: rebuild on PublicNav for unauthed visitors (per spec §5.4)
8a9ec54f   public: About + Changelog + Privacy + Terms stubs (per spec §5.5)
6aa3a668   routing: / = Marketing (public), /home = authed gallery; …
0eb0ee17   e2e: public pre-auth surfaces smoke (8 routes + nav + CTA flows)
8a5dc596   lib: analytics emitter + graceful localStorage helpers
71732724   onboarding: intent picker (spec §7.1) + signup hand-off
5683e1e1   wizard: first-run hint + edge-case polish + analytics
3f4068fb   workspace: first-deploy celebration + product tour
c77ec06d   project archive: settings UI + Home filter pill (spec §8.3, ISS-19)
4bf46ac4   e2e: onboarding intent + first-run + archive filter (no-API tests)
```

**What this carved out:**
- **Public vs authed routes.** `/` is the Marketing landing for unauthed visitors. `/home` is the authed project gallery. Post-login redirects target `/home`. `/new` is intentionally **public** so the wizard can collect a brief without auth — the 401 fires at Begin → `createApp` time.
- **Editorial skill catalogue.** `lib/skills.ts` ships an 8-item static list (Auth, Email, Realtime, Payments, Photos, Search, AI, Analytics). Every "Add to project →" is disabled with a "Coming soon" caption + a footer note pointing at ISS-13.
- **Pricing 15% explainer.** Per spec §5.2 the "$100 → Stripe ~$3.20 → platform $14.52 → creator $82.28" worked example appears at first deploy and first earning copy; the math is inline on `/pricing` and rationalised in the LiveBanner whisper at first deploy.
- **AuthGuard semantics.** Wraps `/home`, `/p/:appId/*`, `/account`, and the catch-all. Public-by-design routes (`/login`, `/signup`, `/forgot-password`, `/onboarding/intent`, `/new`, all marketing pages) are explicitly NOT guarded.
- **Analytics emitter.** `lib/analytics.ts` `track(name, props)` is a localStorage-buffered emitter. Spec §28 events fire from `WizardWorkspace.tsx` (project.creation_*), `OnboardingIntent.tsx` (intent), `WorkspaceShell.tsx` (project.first_deploy), `ChatRail.tsx` (chat.turn_*). Pipeline sink is a follow-on.
- **Project archive.** Soft archive ships with control-plane backing missing — `apps.ts` `archiveApp` / `unarchiveApp` mutate a module-level `Set<string>`. `listApps` decorates each record with `archived: bool` from the Set. Home gallery filter pill + Settings UI work end-to-end *for one session*. ISS-19 traces the missing column.

**What didn't land in Phase D:**
- Real OAuth (Google / GitHub buttons render but click is a no-op).
- Real password-reset email (UI stub only — ISS-09).
- 2FA / sessions list / account deletion (deferred-section stubs only — ISS-10/11/12).
- Template detail pages (`/templates/:slug`).

---

## Phase E · Polish: chat actions, scheduled workers, responsive, a11y, empty states, ErrorBoundary

**What landed:** the polish layer that takes the branch from "works" to "shippable". Hover-revealed message actions in chat. @-mention dropdown. Markdown rendering. Retry on error. PM digest + SRE monitor scheduled-worker procs (the chat-mode SubAgents already shipped in Phase B; this completes the dual shape from spec §4.8.3.2). Responsive sweep for 375 / 768 / 1280. A11y pass (focus-visible ring, modal focus mgmt, aria-labels). Editorial empty states across canvases + filter pages. ErrorBoundary at the root + per-canvas wraps + phone chat drawer.

**Commits in this phase:**

```
8a1399f7   chat: hover actions (copy / regenerate / edit) + markdown polish + retry
123c1347   chat: @-mention dropdown for files / issues / recent errors
af563a88   e2e: chat-actions covers hover row + @-mention dropdown + retry
e7c920a1   agents: PM digest + SRE monitor scheduled-worker procs
56b27a7e   docs: ISS-28 in ISSUES.md (cron / scheduled-worker harness)
becdf8cf   builder: ErrorBoundary at root + per-canvas wraps; phone chat drawer
ffdc0288   builder: editorial empty states across canvases + filter pages
25407c50   builder: responsive sweep for phone / tablet (375 / 768 / 1280)
b61cfe2d   builder: a11y pass — focus-visible ring, modal focus mgmt, aria-labels
aadaf753   docs: accessibility audit for zeroship-builder
```

**What this carved out:**
- **Chat hover actions.** `MessageActions.tsx` renders inline copy / regenerate / edit-prior on hover of any assistant turn, with a dedicated retry button on failed turns. Edit-prior truncates the conversation and re-fires the new turn.
- **@-mention dropdown.** `MentionDropdown.tsx` is keyboard-navigable; types resolved against `lib/skills.ts`, the project's issue list, and a "recent errors" feed.
- **Scheduled workers as RPC procs.** `pmDigest({appId})` and `sreMonitor({appId})` ship as procs callable from any external cron or curl. ISS-28 documents the missing platform scheduler. The dual shape from spec §4.8.3.2 is now complete in surface area; only the trigger is missing.
- **Responsive layouts.** Phone (< 768 px) collapses chat into a drawer; topbar gains a chat-toggle button (`topbar-chat-toggle`). Marketing pages get airy padding at desktop, divided by 2 on phone. `useMediaQuery` is wrapped in `lib/useMediaQuery.ts` to avoid SSR hydration mismatch.
- **A11y pass.** Every interactive element has a focus-visible ring keyed to `--color-tomato`. Modals trap focus and restore on close. Aria-labels on icon buttons. Audit notes captured at `docs/superpowers/accessibility-audit.md`.
- **Empty states.** Per spec §26, every canvas + gallery + filter page has an editorial empty-state copy with a next-action CTA. Errors have a retry affordance.
- **ErrorBoundary.** Root boundary wraps the entire `<App>`. Per-canvas boundaries wrap each canvas under `WorkspaceShell.tsx`. Crash in one canvas doesn't blank the workspace.

**What didn't land in Phase E:**
- Click-to-edit overlay on Preview canvas (V1.5 per spec §9.1).
- ⌘K global search (V2 per spec §22).
- Keyboard-shortcut help dialog.
- Per-canvas search inputs beyond Logs / Files.

---

## Phase E summary — branch end state

| Surface | LOC range | State |
|---|---|---|
| Public marketing tree | ~1100 | shipped |
| Auth surfaces | ~500 | shipped (UI), backend gaps tracked |
| Workspace shell | ~300 | shipped |
| Chat surface | ~1700 | shipped |
| 8 canvases | ~3000 | shipped UI, varying backing depth |
| Server runtime (Builder + wizard + agents) | ~3300 | shipped |
| ISSUES.md | ~1050 | 28 entries, all open |

**Test inventory:**

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

Default `npm run test:e2e` runs all tests. Tests gated on env auto-skip when env not set; ~32 always pass without any env.

**TypeScript:** `tsc --noEmit` — 0 errors.

**Bundle audit:** not enforced as a CI budget yet. Spec §25.5 calls for ≤ 200 KB initial; current is higher because Monaco is not yet lazy-loaded (`canvases/FilesCanvas.tsx` ships the read-only manuscript view, no editor; the 1.2 MB Monaco bundle hasn't landed).

---

## Decisions made during Plan 02

A handful of architecture decisions were made on the branch and recorded in the spec:

1. **Two server runtimes (wizard plain LangGraph; Builder deepagents)** — committed in `c5cd66c4 spec: split wizard runtime from Builder; mark G3 resolved`. The cost test in spec §4.8.2b answers "is this a deepagents agent?" for any future runtime.
2. **Single-input RPC wire** — every server proc takes one object. Documented at the top of `apps.ts`, `agents.ts`, `sandbox.ts`, etc. ISS-02 traces the missing opt-in marker that makes this convention fragile.
3. **Two-node `decide → act` workaround for ISS-01** — the only known way to call `interrupt()` after an `await model.invoke()` in the V8 runtime. Documented at the top of `_wizard.ts`.
4. **`gpt-5.4-mini` family parity across all SubAgents** — `10c07a66` chose model parity over per-agent cost optimisation (G6). Critic running 3× per turn is the heaviest spend; parity ensures Critic doesn't mis-grade Builder's output. Revisit if cost telemetry justifies divergence.
5. **In-memory checkpointer (`MemorySaver`)** — G1 ships a process-local checkpointer for dev. Production swaps in a Postgres-backed `BaseCheckpointSaver`; deferred to control-plane work.
6. **Tool-call rendering taxonomy** — G4 split: write_file / edit_file / ask_survey / task → custom data parts via middleware; ls / read_file / grep / glob / execute → native AI SDK v6 tool-call chunks via the translator.

---

## Pointers for Plan 03

- **Wire control-plane backing** for ISS-09 → ISS-28. Concretely: ISS-13 (skill registry) unblocks §15 / §16 / §12; ISS-14 / 15 / 16 unblock §9.8 / §11 deeply; ISS-18 unblocks §11.3 / §11.4 / §14.
- **Ship the cron scheduler** (ISS-28) so PM digest + SRE monitor become real background passes.
- **Bundle audit + Monaco lazy-load** for spec §25.5's ≤ 200 KB initial budget.
- **Admin canvas** at `/admin/*` per spec §23.

---

## Self-review (retroactive)

Spec coverage:
- ✅ §2 multi-agent — fleet wired with Critic / Reviewer / PM / SRE; chat-mode + scheduled-stub procs; mention dropdown
- ✅ §3 IA — public + authed split; route table in `App.tsx`
- ✅ §4 design system — primitives shipped; data-part renderers shipped
- ✅ §4.8 implementation libraries — translator + middleware + sandbox backend + resume protocol all live
- ✅ §5 pre-auth surfaces — 8 routes, smoke tested
- ✅ §6 auth flows — UI complete; backend stubs tracked
- ✅ §7 onboarding — intent picker, first-run hint, deploy celebration, product tour
- ✅ §8 project lifecycle — wizard + workspace + archive; advanced flow's inferred-card is partial
- ✅ §9 canvases — 8 canvases mounted; varying backing depths
- ✅ §10 chat surface — composer, streaming, stop, receipts, surveys, diffs, critic rounds, hover actions, retry
- ✅ §11.1 Critic loop — `task("critic")` while-loop with structured output
- ⚠ §11.2-§11.6 — pre-deploy gates / verification / auto-rollback / branching / skill budgets all deferred (platform-side)
- ✅ §13 / §14 PM/SRE chat-mode — both ship; scheduled workers exist as procs only (ISS-28)
- ⚠ §12.4 active skills — deferred (registry first, ISS-13)
- ⚠ §15 / §16 themes / feature sets — deferred
- ⚠ §17 / §18 / §19 account / billing / payouts — partial / deferred
- ⚠ §20 / §21 / §22 / §23 sharing / notifications / search / admin — deferred
- ✅ §25 cross-platform — responsive sweep at 375 / 768 / 1280
- ✅ §26 empty / loading / error — sweep complete
- ✅ §27 voice & copy — de-atelier replacements applied
- ✅ §28 telemetry — emitter wired; pipeline sink follow-on

Scope: Plan 02 produced a complete, testable artifact. The branch is ready for review and merge. Plan 03 picks up from the ISSUES.md catalogue.
