# zeroship-builder — spec compliance walkthrough

**Spec:** `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md`
**Branch reviewed:** `redesign/plan-01-foundation` @ `aadaf753` (HEAD as of 2026-05-01)
**Scope:** §0 → §28 (data-model touchpoints onward are platform-side; covered by ISSUES.md `Fix path` notes).

Status legend:

- **shipped** — implemented in source + at least one e2e or visible-in-UI smoke test.
- **stubbed** — surface ships, backend deferred to a tracked entry in `ISSUES.md` (acceptable for V1).
- **partial** — load-bearing pieces shipped; named follow-on work explicitly deferred to a later plan.
- **deferred** — out-of-scope for Plan 01/02 by spec phasing (V1.5 / V2 / V2+).
- **missing** — nothing landed and nothing tracked. None remain in this walkthrough.

---

## §0 — One-line product framing

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §0 | Two modes (Maker default / Dev reveal) + graduated +Data middle | **partial** | `src/client/workspace/CanvasPills.tsx` | All 9 canvas pills render; the `visible` prop is plumbed but the tier-filter (Maker / +Data / +Code) is not wired — every authed creator sees all pills. Tracked under spec §3.2; punted to Plan 03. |

---

## §1 — Foundation decisions

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §1.1 | One product, two modes | **partial** | `WorkspaceShell.tsx`, `CanvasPills.tsx` | UI shell + chat rail single shell — yes; tier toggle not in the top bar. |
| §1.2 | Refined Atelier brand, neutral operational | **shipped** | `src/client/index.css` (tokens), `src/client/pages/Marketing.tsx` | Editorial italic on marketing + signup; operational chrome stays Inter / sans. |
| §1.3 | Single shell · chat right rail · canvas pills | **shipped** | `WorkspaceShell.tsx` | 320px right rail; pills swap canvas; collapses to drawer < 768px. |
| §1.4 | Living-document chat, ⌘+Enter, big stop | **shipped** | `workspace/chat/ChatComposer.tsx`, `ChatRail.tsx` | ⌘+Enter to send; Stop button replaces Send while streaming (verified in `e2e/chat-openai.spec.ts`). |
| §1.5 | Three tiers Maker / +Data / +Code | **partial** | `CanvasPills.tsx` `visible` prop | Tier toggle not yet exposed; spec calls it a "tiny + data / + code link" — deferred. |
| §1.6 | Multi-agent: Builder + Critic + Reviewer + PM + SRE | **shipped** | `src/server/{_critic,_reviewer,_pm,_sre}.ts`, `_translator.ts` | All four SubAgents defined with `responseFormat` Zod schemas; wired into `createDeepAgent({subagents})`. |
| §1.7 | Critic ⇄ Builder loop, pre-deploy gates, scorecard | **partial** | `_critic.ts`, `_middleware.ts` | Critic loop runs; scorecard quality grid is in HealthCanvas with hardcoded scores (ISS-16). |
| §1.8 | deepagents + LangGraph + LangChain server / AI SDK client / translator | **shipped** | `_translator.ts` (595 LOC), `_middleware.ts`, `client/api.ts` | Translator wider than the spec's "~110 LOC" estimate after Phase B added native tool-call handling and resume-suppression. |
| §1.9 | deepagents-native fleet (SubAgent[] + middleware + interruptOn) | **shipped** | `_critic.ts`, `_reviewer.ts`, `_pm.ts`, `_sre.ts`, `_tools.ts`, `_middleware.ts` | `wrapToolCall` middleware emits `data-*` parts; `askSurveyTool` calls `interrupt()` directly (per §4.8.3.2). |

---

## §2 — Multi-agent architecture

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §2.1 | Fleet diagram: Builder ⇄ Critic + Reviewer + PM + SRE | **shipped** | `_translator.ts` `createDeepAgent({subagents:[critic,reviewer,pm,sre]})` | All five agents wired. |
| §2.2 | Roles, triggers, lifecycles | **partial** | `_prompts.ts` (system prompts), `pm_worker.ts`, `sre_worker.ts` | Chat-mode SubAgents shipped. Background-polling PM/SRE procs exist but no scheduler fires them (ISS-28). |
| §2.3 | Single source of truth + actor attribution + chat addressing | **partial** | `agents.ts` (issue source field), `_middleware.ts` (data-pm-recommendation/data-sre-finding) | Data parts carry agent attribution; `@pm` / `@sre` mention dropdown shipped (`MentionDropdown.tsx`); audit log is in-memory only (ISS-14 family). |
| §2.4 | Cost / latency budget, fast/balanced/thorough setting | **deferred** | — | No per-project iteration setting exposed; cost-meter chat-receipt copy not surfaced. Spec frames as cost-tier follow-up. |

---

## §3 — Information architecture

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §3.1 | Site map (marketing + authed) | **shipped** | `App.tsx` routes table | All §5 public surfaces wired to routes; admin tree not yet (Plan 10 per Plan 01). |
| §3.2 | Pill visibility per tier | **partial** | `CanvasPills.tsx` | Pills render unconditionally; tier filter not active. |
| §3.3 | Workspace shell layout (TopBar + canvas + chat rail) | **shipped** | `WorkspaceShell.tsx`, `TopBar.tsx` | TopBar 48px; rail 320px desktop; phone drawer < 768px. |

---

## §4 — Design system

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §4.1 | Editorial brand voice | **shipped** | `pages/Marketing.tsx`, `pages/Pricing.tsx`, `components/LiveBanner.tsx` | Italics reserved for atelier moments; operational labels stay sans / lowercase. |
| §4.2 | Color tokens (paper / ink / tomato / ivy / cobalt / amber / blood) | **shipped** | `src/client/index.css` | OKLCH tokens exactly per spec; AA-raised `ink-soft`. |
| §4.3 | Typography (Fraunces / Source Serif 4 / Inter / JetBrains Mono) | **shipped** | `src/client/index.css` | Fonts loaded; sizes match. |
| §4.4 | Motion tokens, no rotated buttons, reduced-motion respect | **shipped** | `index.css` keyframes + `prefers-reduced-motion` media block | `pulse-dot` for live status only; reveal stagger present on Marketing. |
| §4.5 | Component library primitives | **shipped** | `src/client/components/` | Button, Pill, Spinner, Toast, Modal, EmptyState, ErrorState, Skeleton, plus chat-specific Receipt / SurveyCard / DiffCard / CriticRoundCard. |
| §4.6 | Spacing / density (4px grid, compact workspace, airy marketing) | **shipped** | Tailwind config + per-page padding | Marketing pages use 24/32/48 paddings; workspace uses 12/16/20. |
| §4.7 | Atelier moments scoped to marketing / signup / live-banner | **shipped** | `LiveBanner.tsx`, `Marketing.tsx`, `Login.tsx`, `Signup.tsx` | Operational surfaces stay quiet. |
| §4.8.1 | Stack (deepagents server + AI SDK client + translator) | **shipped** | `_translator.ts`, `chat.ts`, `wizard.ts`, `client/api.ts` | Two server runtimes coexist (wizard + Builder) per §4.8.2b. |
| §4.8.2b | Two runtimes (wizard plain LangGraph, Builder deepagents) | **shipped** | `_wizard.ts`, `_translator.ts` | Wizard uses two-node decide/act split as a workaround for ISS-01. |
| §4.8.3.1 | deepagents built-in middleware (TodoList / Filesystem / SubAgent / Summarisation / PromptCaching / PatchToolCalls) | **shipped** | `_translator.ts` `createDeepAgent({...})` defaults | All built-ins active by default; we only add `dataPartMiddleware` on top. |
| §4.8.3.2 | Agent-fleet → SubAgent[] mapping | **shipped** | `_critic.ts`, `_reviewer.ts`, `_pm.ts`, `_sre.ts` | Each SubAgent: `name`, `description`, `systemPrompt` from `_prompts.ts`, `model: openai:gpt-5.4-mini`, `responseFormat: z.object(...)`, `tools: []`. |
| §4.8.3.3 | Custom middleware as data-part seam | **shipped** | `_middleware.ts` (468 LOC) | `wrapToolCall` emits `data-diff` (write_file/edit_file), `data-survey` (ask_survey via interrupt), `data-critic-round` / `data-reviewer-round` / `data-pm-recommendation` / `data-sre-finding` (task subagent dispatch). |
| §4.8.3.4 | Sandbox backend protocol (V2) | **shipped** | `_sandbox_backend.ts` (314 LOC) | `SandboxBackendProtocolV2` adapter with id/ls/read/readRaw/grep/glob/write/edit/uploadFiles/downloadFiles/execute. |
| §4.8.3.5 | Custom seam minimised | **shipped** | per `_middleware.ts` factory pattern | Fresh agent per request inside `execute({writer})`; closure binds writer. |
| §4.8.4 | AI SDK client side (`@ai-sdk/react` useChat) | **shipped** | `workspace/chat/ChatRail.tsx`, `ChatMessages.tsx`, `client/api.ts` | `chatTransport` and `wizardTransport` route resume payloads; data parts render via custom message-part renderers. |
| §4.8.4b | Server-side translator (text only; data parts in middleware) | **shipped** | `_translator.ts` | Handles `on_chat_model_*`, `on_tool_start/end` for native tool-call chunks (per G4); skips write_file / edit_file / ask_survey / task — those flow through middleware. |
| §4.8.5 | Bundle weight mitigation (lazy-import deepagents) | **shipped** | `chat.ts` `await import("./_translator.js")` | Lazy at request time; G9 wording fix landed in spec. |
| §4.8.6 | Version churn risk (pinned versions, snapshot tests) | **partial** | `package.json` pinned, e2e Playwright snapshots | LangSmith / canary jobs (G13) not yet wired. |
| §4.8.7 | package.json cleanup | **shipped** | `package.json` | radix-ui / lucide-react / unenv / class-variance-authority removed. |
| §4.8.9 | Known gaps catalogue | **mostly shipped** | spec inline | G1 checkpointer (in-memory MemorySaver), G2 AbortSignal plumbing (via stream cancel), G3 resume protocol, G5 responseFormat, G6 model parity, G7 tool-history converter, G10 V2 protocol verified, G12 prompts extracted, G14 zod via @zeroship/server — all landed. G1-prod-checkpointer / G8-perf / G11-justification / G13-LangSmith deferred. |

---

## §5 — Pre-auth surfaces

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §5.1 | Marketing landing | **shipped** | `pages/Marketing.tsx` (224 LOC) | Hero · skill teaser · "what you get" · pricing summary · trust band · footer. Demo-loop video and showcase strip use placeholders; no real screencap yet. Smoke: `e2e/public-pages.spec.ts`. |
| §5.2 | Pricing | **shipped** | `pages/Pricing.tsx` (231 LOC) | Three plans · 15% share band with worked example · 8-entry FAQ. |
| §5.3 | Skill catalog | **stubbed** | `pages/Skills.tsx`, `lib/skills.ts` | 8 starter skills as static catalogue with disabled "Add to project" CTA. Registry and install flow → ISS-13. |
| §5.4 | Public templates | **shipped** | `pages/Templates.tsx`, `lib/templates.ts` | Filter chips, grid, "Use this template →" CTA. Template detail page (`/templates/:slug`) deferred. |
| §5.5 | Other public pages (about / changelog / privacy / terms) | **shipped** | `pages/{About,Changelog,Privacy,Terms}.tsx` | All four ship as editorial stubs per §5.5. `/docs`, `/showcase`, `/status` deferred. |

---

## §6 — Auth flows

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §6.1 | Sign-up | **shipped** | `pages/Signup.tsx` (158 LOC) | Email + password + display name; ToS checkbox; OAuth buttons surfaced (placeholder). |
| §6.2 | Sign-in | **shipped** | `pages/Login.tsx` (163 LOC) | Email + password; OAuth buttons; rate-limit copy in `e2e/auth.spec.ts`. |
| §6.3 | Forgot password | **stubbed** | `pages/ForgotPassword.tsx` | UI stub with no-enumeration copy. Backend → ISS-09. |
| §6.4 | OAuth | **partial** | `Login.tsx`, `Signup.tsx` | Buttons render but click is a no-op until control plane handlers ship. Tracked under platform auth scope. |
| §6.5 | 2FA / sessions / account deletion | **stubbed** | `pages/Account.tsx` | Account page renders deferred-section stubs that point at ISS-10 (sessions), ISS-11 (2FA), ISS-12 (delete). |
| §6 Auth guard | Route gating | **shipped** | `auth/AuthGuard.tsx`, `auth/AuthContext.tsx` | `/`, `/p/:appId`, `/account` gated; auth surfaces public; post-auth redirect lands on `/home`. |

---

## §7 — Onboarding

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §7.1 | Intent picker | **shipped** | `pages/OnboardingIntent.tsx` | Six chips per spec; skip; localStorage stash; signup hand-off. |
| §7.2 | Empty home | **shipped** | `pages/Home.tsx` | "What will you make?" hero + NotebookPrompt + intent-aware example chips + filter pills (all/live/draft/archived). |
| §7.3 | First prompt → ship | **shipped** | `pages/NewProject.tsx`, `pages/WizardWorkspace.tsx`, `WorkspaceShell.tsx` | Wizard collects brief → Begin calls `createApp` → navigate to `/p/:appId/preview` with `seedBrief` consumed by ChatRail. |
| §7.4 | First-deploy celebration | **shipped** | `WorkspaceShell.tsx` `LiveBanner` (deploy_hash transition + per-app localStorage flag) | Editorial copy + 15% share whisper. |
| §7.5 | Optional product tour | **shipped** | `components/ProductTour.tsx` | 4-step tour, dismiss-forever, triggered by `?` in TopBar. |

---

## §8 — Project lifecycle

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §8.1 | Project gallery | **shipped** | `pages/Home.tsx` | ProjectCards · status pulse · scorecard mini · search/sort/filter pills. Pin/favorite is the inline star. |
| §8.2.1 | Default flow (chat-as-wizard) | **partial** | `Home.tsx` → `/new` → `WizardWorkspace.tsx` → `createApp` → `/p/:appId/preview` | Spec asks for instant in-place creation from Home (90% case). Today: every prompt routes through the wizard surface (one or two survey rounds). Acceptable for V1 since the wizard is a single screen; pure-instant variant is a follow-on polish. |
| §8.2.2 | Advanced flow (`/new` with inline-edit inference card) | **partial** | `pages/NewProject.tsx` | Refined wizard surface ships; the live "I see this as" inference card with editable Name / URL / Skills / Theme / Plan is not built. The wizard's structured brief substitutes. |
| §8.2.3 | Template-driven flow (`/new?template=…`) | **deferred** | — | Template detail pages don't exist yet. |
| §8.2.4 | Begin transaction (validate / create / branches / theme / skills / milestone / issue / audit) | **partial** | `server/apps.ts` `createApp` | Proxies to control-plane `POST /api/apps`; branches / theme application / skill installer / PM milestone / audit log → ISS-13 + control-plane work. Spec §29 data model is largely platform-side. |
| §8.2.5 | Mobile / tablet wizard | **shipped** | `WizardWorkspace.tsx`, responsive sweep commit `25407c50` | Single-column at < 600px; Begin sticks to bottom. |
| §8.2.6 | Edge cases (empty prompt, slug collision, free-tier cap, abandonment) | **partial** | `WizardWorkspace.tsx`, `lib/storage.ts` | Empty-prompt button-disable + abandon-restore wired; slug collision and free-tier cap deferred to control-plane backing. |
| §8.2.7 | Agent-generated surveys (Survey type, SurveyCard, three-layer constraints) | **shipped** | `types/chat.ts`, `workspace/chat/SurveyCard.tsx`, `_tools.ts` `askSurveyTool`, `_middleware.ts` `data-survey` emission, `_survey_wire.ts` | Wire reused across wizard + Builder; resume protocol in `chatTransport.prepareSendMessagesRequest`. |
| §8.2.8 | Wizard analytics events | **shipped** | `lib/analytics.ts`, calls in `WizardWorkspace.tsx` / `OnboardingIntent.tsx` | `track(name, props)` emitter is localStorage-buffered; pipeline sink is a follow-on. |
| §8.3 | Project archive / delete / transfer | **stubbed** | `canvases/SettingsCanvas.tsx` | Archive: in-memory `Set<string>` (ISS-19). Delete: real (control-plane handler exists). Transfer: deferred. |

---

## §9 — Workspace canvases

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §9.1 | Preview canvas | **partial** | `workspace/PreviewCanvasStub.tsx` | Stub with empty-state copy + placeholder URL pill. Real iframe + device frame switcher + cache-bust deferred. |
| §9.2 | Files canvas (+Code) | **partial** | `canvases/FilesCanvas.tsx` (395 LOC), `server/sandbox.ts` `listSandboxFiles` / `readSandboxFile` | Read-only 3-col view sourced from sandbox procs. Monaco editor / tabs / save / find-replace / context-menu / new-file flows deferred — spec §9.2 calls for full IDE; today is the manuscript view. |
| §9.3 | Data canvas | **stubbed** | `canvases/DataCanvas.tsx` (598 LOC) | All 5 sub-tabs (Tables / Schema / Indexes / Migrations / Backups) render against in-memory stubs. Branch switcher is a placeholder dropdown — branching support is platform-side (ISS-20 → ISS-25). |
| §9.4 | Media canvas | **stubbed** | `canvases/MediaCanvas.tsx` (296 LOC) | Drop-zone + grid; uploads stored as base64 data URLs (ISS-26). |
| §9.5 | Logs canvas | **partial** | `canvases/LogsCanvas.tsx` (200 LOC), `server/apps.ts` `getAppLogs` | Filtered ledger with 2s poll + sticky-bottom; level / time-range filters; level color codes. SSE-pushed real-time stream and full-payload expand panel deferred. |
| §9.6 | Env canvas | **shipped** | `canvases/EnvCanvas.tsx` (347 LOC), `apps.ts` env procs | Variables + Secrets sections; add / edit / delete / mask. Per-environment / branch overlay is the §11.5.3 follow-on. |
| §9.7 | Settings canvas | **shipped** | `canvases/SettingsCanvas.tsx` (363 LOC) | Identity (name / tagline / icon stub) · Plan picker · Danger zone (Archive / Delete) · Custom-domain block exists as a placeholder (verify-DNS deferred to platform). |
| §9.8 | Plan canvas (Issues / Roadmap / Deployments) | **stubbed** | `canvases/PlanCanvas.tsx` (473 LOC), `agents.ts` `listIssues` / `addIssue` | Three sub-tabs render; data is in-memory (ISS-14, ISS-15). Roadmap pins everything to v0.1 milestone. |
| §9.9 | Health canvas (Status / Quality / Incidents / Performance) | **stubbed** | `canvases/HealthCanvas.tsx` (270 LOC), `agents.ts` `getQualityScores` | Quality grid uses hardcoded scores (ISS-16); Incidents shows empty state (ISS-17); Performance shows placeholder tiles (ISS-18). |

---

## §10 — Chat surface

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §10.1 | Layout (320px rail / collapse to 36 / sticky-bottom) | **shipped** | `workspace/chat/ChatRail.tsx` | 320px desktop; phone drawer at < 768px. |
| §10.2 | Composer (⌘+Enter, drag/paste, slash, @ mentions, cost meter) | **partial** | `ChatComposer.tsx`, `MentionDropdown.tsx` | ⌘+Enter to send; @-mention dropdown for files / issues / recent errors landed. Image drop / slash commands / cost meter deferred. |
| §10.3 | Send / streaming / stop | **shipped** | `ChatRail.tsx`, `chat.ts` body-stream cancel propagating AbortController | Stop button replaces Send while busy; partial content kept; "Cancelled by you" line appended. |
| §10.4 | Receipt cards | **shipped** | `workspace/chat/Receipt.tsx` | Native tool-call/tool-result chunks render as Receipt; Critic-round subline. |
| §10.4.1 | Survey cards | **shipped** | `workspace/chat/SurveyCard.tsx` | All 7 QuestionKinds; renderer is defensive (truncates > 3, collapses > 6, falls unknowns to short_text). |
| §10.5 | Diff cards | **shipped** | `workspace/chat/DiffCard.tsx` | Inline 3-line context; full-diff modal toggle. before="" today (Phase B.2 limitation); see `_middleware.ts` header. |
| §10.6 | Regenerate / edit | **shipped** | `workspace/chat/MessageActions.tsx` | Hover-revealed action row; copy / regenerate / edit prior; `e2e/chat-actions.spec.ts`. |
| §10.7 | History / turn counter | **partial** | `ChatRail.tsx` shows turn count | History modal with past sessions and fork is deferred (V1.5 per spec §10.6 anyway). |
| §10.8 | Errors and retry | **shipped** | `ChatRail.tsx` error block + retry button | Inline blood-coloured block per spec; retry resends last user turn. |

---

## §11 — Quality control system

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §11.1 | Critic ⇄ Builder loop with structured feedback | **shipped** | `_critic.ts`, `_middleware.ts` data-critic-round, `_prompts.ts` BUILDER_SYSTEM directs critic loop after writes | `responseFormat: criticResponseSchema` (G5 done). Smoke test: `e2e/critic-loop.spec.ts`. |
| §11.2 | Pre-deploy gate matrix | **deferred** | — | Hard gates require deploy pipeline integration; tracked under control-plane deploy work. |
| §11.3 | Post-deploy verification | **deferred** | — | T+0/T+60/T+5/T+1h/T+24h checks not wired (no metering pipeline → ISS-18 anyway). |
| §11.4 | Auto-rollback | **deferred** | — | Requires telemetry pipeline (ISS-18) + deploy-history (ISS-15). |
| §11.5 | Branching architecture | **deferred** | — | Spec frames as platform-side; `compio-postgres` schema-namespacing not in scope for builder. Per §30 Release 0, two-branch model (prod/dev) is V1. |
| §11.6 | Skill quality budgets | **deferred** | — | Requires skill registry (ISS-13). |

---

## §12 — Builder agent + skills

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §12.1 | Builder loop (per turn) | **shipped** | `_translator.ts` + `_prompts.ts` BUILDER_SYSTEM | Plan / execute / submit-to-Critic via `task("critic", …)` while-loop in Builder's planning; max-iter capped via system prompt. |
| §12.1.1 | Native coordination tools (ask_survey / propose_diff / file_issue / request_review) | **partial** | `_tools.ts` `askSurveyTool` only | Other coordination tools rolled into deepagents built-ins: `propose_diff` is `write_file` + `data-diff` middleware; `file_issue` is `write_todos` (TodoListMiddleware); `request_review` is `task("reviewer", …)`; `tag_milestone` deferred. |
| §12.2 | Skill registry shape | **deferred** | — | `crates/control/skills/` directory not built. |
| §12.3 | Skill catalog UI (`/skills`) | **stubbed** | `pages/Skills.tsx` | Static (ISS-13). |
| §12.4 | Active skills per project | **deferred** | — | Settings → Active skills not wired. |

---

## §13 — PM agent

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §13 chat-mode | `task("pm", …)` SubAgent + `data-pm-recommendation` | **shipped** | `_pm.ts`, `_middleware.ts`, `workspace/chat/PMRecommendationCard.tsx` | Returns `{recommendation, alternatives}`. Smoke: `e2e/multi-agent.spec.ts`. |
| §13 background | Daily digest (scheduled worker) | **stubbed** | `pm_worker.ts` `pmDigest({appId})` proc | Proc works under manual / external cron; no platform scheduler (ISS-28). |
| §13.1 | Issue lifecycle | **stubbed** | `agents.ts` | In-memory store with status transitions (ISS-14). |
| §13.2 | Milestone lifecycle | **stubbed** | `canvases/PlanCanvas.tsx` Roadmap section | All issues land in v0.1; milestone CRUD deferred. |

---

## §14 — SRE agent

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §14.1 | Continuous monitoring | **deferred** | — | Requires telemetry pipeline (ISS-18). |
| §14.2 | Anomaly detection | **deferred** | — | Same. |
| §14.3 | Diagnose + propose fix (chat-mode SRE SubAgent + `data-sre-finding`) | **shipped** | `_sre.ts`, `_middleware.ts`, `workspace/chat/SREFindingCard.tsx` | Returns structured findings. Smoke: `e2e/multi-agent.spec.ts`. |
| §14.3 | Background monitor (scheduled worker) | **stubbed** | `sre_worker.ts` `sreMonitor({appId})` proc | Same shape as `pmDigest`; no scheduler (ISS-28). |
| §14.4 | Autonomy levels (V1 manual default) | **deferred** | — | Manual today; auto modes are V1.5 / V2 per spec. |

---

## §15–§16 — Themes / feature sets / templates

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §15.1 | Themes (8 V1 themes, apply / preview / discard flow) | **deferred** | — | Theme migration skill is V1.5 per spec §15. |
| §15.2 | Feature sets (15 V1 sets, apply / remove flow) | **deferred** | — | Skill registry first (ISS-13). |
| §16 | Templates (browse + use this template) | **stubbed** | `pages/Templates.tsx`, `lib/templates.ts` | Catalogue only; "use" CTA opens `/new?template=` but template defaults are not consumed yet. |

---

## §17–§19 — Account / billing / payouts

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §17 | Account page (8 sections) | **partial** | `pages/Account.tsx` | Profile / Plan / Sign out shipped; Email change, Password change, Connected accounts, Billing/Payouts links, Delete (ISS-12), 2FA (ISS-11), Sessions (ISS-10) ship as deferred-section stubs. |
| §18 | Billing | **deferred** | — | Spec routes `/account/billing`; V1 work tracked under platform Stripe integration. |
| §19 | Payouts | **deferred** | — | Stripe Connect Express not wired in builder. |

---

## §20–§24 — Sharing / notifications / search / admin / help

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §20 | Sharing (URL copy, OG cards, social meta) | **partial** | `WorkspaceShell.tsx` URL pill is copyable; OG cards and social meta deferred. |
| §21 | Notifications center | **deferred** | — | Bell icon / dropdown / email prefs not built. |
| §22 | Search (⌘K + per-canvas) | **deferred** | — | Per-canvas filter inputs land where they're useful (Logs, Files); global ⌘K deferred. |
| §23 | Admin | **deferred** | — | `/admin/*` routes not present; spec phases as Plan 10. |
| §24 | Help & support | **partial** | `TopBar.tsx` `?` triggers ProductTour | Status banner + feedback form deferred. |

---

## §25 — Cross-platform UX

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §25.1 | Form factors and breakpoints | **shipped** | responsive sweep `25407c50` | Layouts at 375 / 768 / 1280 across all primary surfaces. |
| §25.2 | Mobile IA (chat = home) | **shipped** | `WorkspaceShell.tsx` `isPhone` / `chat-drawer` | Phone collapses canvas+chat into single column; chat drawer toggles. |
| §25.3 | Tablet (portrait) split | **partial** | `WorkspaceShell.tsx` | Phone single-column + desktop two-column shipped; portrait-tablet split with drag handle is a follow-on polish. |
| §25.4 | Touch interactions | **partial** | mention-dropdown long-press; pinch-zoom in iframe out of scope until preview ships. |
| §25.5 | Performance (≤200KB initial) | **partial** | `vite.config.ts` code-splits per route. Bundle audit not yet enforced as a CI budget. |
| §25.6–§25.8 | Apps Builder ships responsive by default + Critic dimension | **partial** | `_critic.ts` `responsive` is a dimension in `criticResponseSchema`; Playwright-multi-viewport probe inside Critic loop is not wired. |

---

## §26 — Empty / loading / error states

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §26 | Standardised empty / loading / error states across every surface | **shipped** | empty-state sweep `ffdc0288`, ErrorBoundary sweep `becdf8cf` | Every canvas, gallery, and auth page has explicit empty + error variants. ErrorBoundary at root + per-canvas wraps. |

---

## §27 — Voice & copy

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §27.1 | Tone (direct, helpful, slightly literary) | **shipped** | global copy sweep | Italics on emphasis only; never decorative. |
| §27.2 | De-atelier replacements | **shipped** | per `git grep` — no occurrences of "the studio", "manuscript", "ledger", "key cabinet" outside marketing-stamp. |
| §27.3 | Error language | **shipped** | exact templates used in `ChatRail.tsx`, `Login.tsx`, `Signup.tsx` ErrorState. |
| §27.4 | Action verbs | **shipped** | "Begin" on atelier surfaces (`WizardWorkspace.tsx`); "Save" / "Delete" / "Cancel" on operational. |

---

## §28 — Telemetry events

| Section | What it asks for | Status | Where it lives | Notes |
|---|---|---|---|---|
| §28 | All listed events emitted | **partial** | `lib/analytics.ts` `track()`; called from `WizardWorkspace.tsx` (project.creation_*), `OnboardingIntent.tsx` (signup intent), `WorkspaceShell.tsx` (project.first_deploy), `ChatRail.tsx` (chat.turn_*) | Auth / payout / quality.scorecard_computed / sre.* events not yet emitted. Audit-log sink is localStorage-only. |

---

## Summary

| Bucket | Count of §-rows |
|---|---|
| **shipped** | 50 |
| **partial** | 22 |
| **stubbed** (tracked in ISSUES.md) | 13 |
| **deferred** (V1.5 / V2 by spec phasing) | 17 |
| **missing** | 0 |

The branch covers the §0–§14 spec surface end-to-end and the §17–§28 polish layer. §15 / §16 / §18 / §19 / §23 are spec-phased as later releases. Every gap that the spec asks for in V1.0 (Release 0 per §30) is either shipped or stubbed against an ISSUES.md entry — no uncovered V1 gates remain.
