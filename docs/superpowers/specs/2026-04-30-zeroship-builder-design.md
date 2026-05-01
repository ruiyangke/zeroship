# zeroship-builder — comprehensive design

**Status:** v1 design, ready for review
**Date:** 2026-04-30
**Companion:** `2026-04-30-zeroship-builder-features.md` (full feature inventory)
**Replaces:** the current `apps/zeroship-builder/` implementation

This document is the design surface that the implementation plan will reference. Feature-level granularity stays in the inventory; *behavior, UX, architecture* live here.

---

## 0 · One-line product framing

> **zeroship-builder** is the platform's first-party app builder. Creators describe an app in natural language; a fleet of AI agents (Builder, Critic, Reviewer, PM, SRE) builds it, ships it on the zeroship runtime, monitors it, fixes it, and helps the creator monetize it. The platform takes 15 %; the creator keeps the rest.

Two modes for two users:

- **Maker mode (default)** — Ali, the non-technical creator. Sees a chat, a preview, a settings page, and a logs view. Doesn't see code.
- **Dev mode (revealed)** — Sam, the technical creator. Adds files, data, env, full IDE.

A graduated middle tier (**+Data**) sits between them.

---

## 1 · Foundation decisions (recap)

| # | Decision | Choice |
|---|----------|--------|
| 1 | Persona | One product, two modes (Ali default + Sam reveal) |
| 2 | Brand | Refined Atelier — editorial soul, neutral operational surfaces, no vocabulary tax |
| 3 | Workspace IA | Single shell. Chat right rail constant. Canvas swaps via top-bar pills. |
| 4 | Chat | Living-document right rail, linear, multi-modal, ⌘+Enter, big stop |
| 5 | Mode model | Three tiers: Maker / +Data / +Code |
| 6 | Product shape | Multi-agent: Builder + Critic + Reviewer + PM + SRE |
| 7 | Quality | Critic ⇄ Builder loop in generation; pre-deploy gates; scorecard tracked over time |
| 8 | Implementation libraries | **deepagents + LangGraph + LangChain models on server**; **Vercel AI SDK (`@ai-sdk/react`) on client**; thin translator at the seam (~110 LOC after Plan 02 Phase A — narrower than originally planned). Per §4.8. Viable because zeroship V8 runtime ships node-compat. |
| 9 | Agent fleet implementation | **deepagents-native** (post-docs review): Critic / Reviewer / PM / SRE = `SubAgent[]` configs; built-in `TodoListMiddleware` + `FilesystemMiddleware` + `SubAgentMiddleware` cover much of the planned custom code; `interruptOn` implements `ask_survey`; custom `wrapToolCall` middleware emits `data-*` UI parts; `backend` adapter wires built-in fs tools to `crates/sandbox`. Per §4.8.3. |

---

## 2 · Multi-agent architecture

> **Implementation note** (post-deepagents-docs review). The multi-agent fleet is built on **deepagents + LangGraph + LangChain models**. Critic / Reviewer / PM / SRE map directly onto deepagents' `SubAgent` config (the ones invoked from chat) — no custom orchestration runtime. PM and SRE *also* run as standalone scheduled workers for background polling work that doesn't sit inside a chat turn. `ask_survey` maps onto `interruptOn`. Custom `wrapToolCall` middleware emits `data-*` UI parts (Survey / Diff / CriticRound / Issue). See §4.8.3 for the concrete mapping; this section describes agent *semantics* independent of implementation.

### 2.1 The fleet

```
                         ┌──────────────────────────────────────┐
                         │  Shared project state (control plane) │
                         │  · file tree                          │
                         │  · deployment history                 │
                         │  · logs, metrics, incidents           │
                         │  · conversation history               │
                         │  · issues, backlog, milestones        │
                         │  · quality scorecard (per-deploy)     │
                         │  · audit log (with attribution)       │
                         └──────────────────────────────────────┘
                                    ▲
              ┌────────────┬────────┼────────┬─────────────┐
              ▼            ▼        ▼        ▼             ▼
         ┌─────────┐  ┌────────┐ ┌─────────┐ ┌────┐  ┌─────────┐
         │ Builder │⇄ │ Critic │ │Reviewer │ │ PM │  │   SRE   │
         └─────────┘  └────────┘ └─────────┘ └────┘  └─────────┘
              │                       │               │
              ▼                       ▼               ▼
         (writes code)          (CI gate)       (monitors live URL)
```

### 2.2 Roles, triggers, lifecycle

| Agent | Triggered by | Output | Lifecycle |
|-------|--------------|--------|-----------|
| **Builder** | Creator chat turn; PM-suggested feature accepted; SRE auto-fix proposal | Code changes (commits) | Per-turn; loops with Critic |
| **Critic** | Every Builder commit during a generation turn | Structured feedback (issues by dimension) | Loops with Builder until approved or N iterations |
| **Reviewer** | Every commit (after Critic loop ends) | Pass/fail per check; merge or block | One pass per commit, deterministic checks |
| **PM** | Project state changes (new deploy, new issue, milestone reached); chat `@pm`; scheduled digests | Suggestions, digests, status replies | Persistent (background); reactive when @mentioned |
| **SRE** | Time (polling); deploy completion; incoming logs/metrics | Anomaly reports, auto-fix proposals, incident timelines | Persistent (background) |

### 2.3 Communication and ownership

- **Single source of truth:** the control plane stores all shared state. Agents never have private memory beyond a turn.
- **Attribution:** every action in the audit log carries `actor: human | builder | critic | reviewer | pm | sre`. Visible in chat receipts, deploy detail, scorecard history.
- **Chat addressing:** Builder is default. `@pm`, `@sre`, `@critic` route to the named agent. (Reviewer is implicit; doesn't speak in chat.)
- **Tiebreakers:**
  - Critic outranks Builder for *blocking* issues (security, correctness, hard-gate violations).
  - Builder outranks Critic for *style* issues (architecture, naming) — prevents ping-pong.
  - Reviewer outranks both if a hard gate fails — change is blocked regardless of Critic approval.
  - SRE proposals always require Reviewer + creator approval before deploy (V1).

### 2.4 Cost / latency budget

- Each Builder ⇄ Critic loop iteration adds ~3–8 s and ~1 LLM round-trip.
- Default 3 iterations → ~10–24 s added per code-gen turn vs no-Critic baseline.
- Per-project setting:
  - **Fast** = 1 iteration, ~5 s overhead. Free tier default.
  - **Balanced** = 3 iterations. Pro tier default.
  - **Thorough** = 5+ iterations. Enterprise / monetized projects.
- Cost meter (chat receipt) shows: "3 iterations · 12.4 s · 18 k tokens".

---

## 3 · Information architecture

### 3.1 Site map

```
zeroship.dev (marketing)
├── /                       Landing
├── /pricing                Plans + 15 % share explainer
├── /templates              Public template gallery
├── /showcase               Public app discovery (V2+)
├── /docs                   Documentation hub
├── /changelog              Product updates
├── /skills                 "What can zeroship build?" — skill catalog
├── /legal/{terms,privacy,aup}
├── /status                 Status page (external)
└── /about

zeroship.dev/app (authed)
├── /signup, /login, /forgot-password
├── /                       Project gallery (home)
├── /new                    Create project (prompt-first, single page)
├── /templates              Authed templates gallery (use → /new)
├── /skills                 Authed skill catalog (browse what you can ask for)
├── /account                Profile + billing entry + payouts entry
├── /account/billing        Payment methods, plan, invoices
├── /account/payouts        Stripe Connect onboarding, earnings, bank
├── /p/:appId               Workspace shell
│   ├── /preview            (default canvas)
│   ├── /files              [+Code tier]
│   ├── /data               [+Data tier]
│   ├── /media              [+Data tier]
│   ├── /logs               [Maker tier]
│   ├── /env                [+Code tier]
│   ├── /plan               [Maker tier]   PM canvas: issues / roadmap / deploys
│   ├── /health             [Maker tier]   SRE canvas: status / quality / incidents / perf
│   └── /settings           [Maker tier]
└── /admin                  (role-gated, real)
    ├── /apps, /users, /revenue, /audit, /flags
```

### 3.2 Pill visibility per tier

| Pill | Maker | +Data | +Code |
|------|-------|-------|-------|
| preview | ✓ | ✓ | ✓ |
| logs | ✓ | ✓ | ✓ |
| plan | ✓ | ✓ | ✓ |
| health | ✓ | ✓ | ✓ |
| settings | ✓ | ✓ | ✓ |
| data | — | ✓ | ✓ |
| media | — | ✓ | ✓ |
| files | — | — | ✓ |
| env | — | — | ✓ |

The tier toggle is in the top bar as a tiny "+ data" / "+ code" link (not a hard binary pill). State persists per project.

### 3.3 Workspace shell layout (single, all tiers)

```
┌──────────────────────────────────────────────────────────────────┐
│ TopBar:  zeroship.   /   project-name   pills…   [+code]  URL  ◯ │
├──────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Canvas (preview by default — swaps via pill)        Chat rail  │
│                                                                  │
│  ┌────────────────────────────────────┐    ┌──────────────────┐ │
│  │                                    │    │ Notes & thoughts │ │
│  │       (canvas content)             │ ◀▶ │                  │ │
│  │                                    │    │  msgs…           │ │
│  │                                    │    │                  │ │
│  └────────────────────────────────────┘    │  composer        │ │
│                                            └──────────────────┘ │
│  Status pulse / drawer (deploy/incident toast)                  │
└──────────────────────────────────────────────────────────────────┘
```

- Chat rail: 320 px default, 280–640 px resizable via gutter, collapsible to 36 px strip on right edge.
- Canvas: takes remaining width; min 480 px. Internal layout per canvas type.
- Top bar: 48 px. Crumb + pills + tier toggle + URL pill + account dot.
- No separate "if you're curious" drawer — pills replace it.

---

## 4 · Design system

### 4.1 Brand voice

- **Editorial in tone, neutral in operation.** "What will you make?" on the marketing page; "Logs" not "Ledger" inside the workspace.
- Italics only for emphasis in marketing surfaces and the very-short copy on the home hero / signup. Never in operational labels, never in error messages.
- All-caps small-tracked labels: limited to navigation eyebrows, scorecard dimension titles, and tier badges. Always ≥ 12 px and only on >75 % luminance backgrounds.
- "The studio" personification: dropped. The agent is "Builder" in attribution, "the agent" in casual reference. PM and SRE are named in audit + plan / health surfaces.

### 4.2 Color tokens

```
--color-paper:       oklch(0.97 0.012 89)     # page bg
--color-paper-2:     oklch(0.94 0.014 86)     # subtle bg, panels
--color-paper-3:     oklch(0.91 0.014 84)     # elevated panels
--color-ink:         oklch(0.18 0.013 60)     # primary text
--color-ink-soft:    oklch(0.40 0.013 65)     # secondary text  ⚠ raised from .36 → .40 for AA
--color-pencil:      oklch(0.55 0.012 70)     # tertiary; only on paper / paper-2 ≥ 16 px
--color-rule:        oklch(0.78 0.014 75)     # borders, dividers
--color-rule-2:      oklch(0.85 0.014 78)     # subtle dividers

--color-tomato:      oklch(0.61 0.21 27)      # primary action only
--color-tomato-2:    oklch(0.55 0.21 25)      # press / shadow
--color-tomato-3:    oklch(0.90 0.06 27)      # tinted bg

--color-ivy:         oklch(0.58 0.16 152)     # success, live, healthy
--color-ivy-2:       oklch(0.50 0.16 150)
--color-ivy-3:       oklch(0.93 0.05 150)     # tinted bg

--color-cobalt:      oklch(0.55 0.18 252)     # info, links inside operational copy
--color-amber:       oklch(0.72 0.15 80)      # warning

--color-blood:       oklch(0.50 0.21 28)      # destructive (delete, ban) — distinct from tomato
```

**Six accent jobs, six distinct colors** (vs the one-tomato-does-everything mistake of v0):

| Meaning | Color |
|---------|-------|
| Primary action | tomato |
| Live / success / healthy | ivy |
| Info / link / neutral pop | cobalt |
| Warning | amber |
| Error / destructive | blood |
| Brand mark | tomato |

### 4.3 Typography

```
--font-display:  Fraunces (variable)         # marketing, hero, project titles only
--font-serif:    "Source Serif 4"            # body in marketing + chat messages
--font-sans:     Inter (variable)            # operational UI: labels, buttons, tables
--font-mono:     JetBrains Mono              # code, file paths, IDs, timestamps
```

- **Marketing**: Fraunces 64–96 px display; Source Serif 17–19 px body.
- **Operational chrome (top bar, pills, buttons, tables)**: Inter 13–15 px.
- **Chat user turns**: Source Serif 15 px with tomato left rule (already nice, kept).
- **Chat assistant turns**: Source Serif 14.5 px.
- **Code blocks, file paths, IDs, timestamps**: JetBrains Mono 12.5 px.
- **All-caps eyebrows**: Inter 11.5 px / 0.14 em tracking. *Down from* the 10–10.5 px / 0.18–0.22 em that fails AA.

### 4.4 Motion

- All transitions: `cubic-bezier(.2, .7, .2, 1)`, durations 120 / 200 / 380 ms.
- Reveal stagger on first paint: 60 ms / 160 ms / 260 ms / 380 ms / 520 ms (kept from current).
- Mode-switch transition: ✗ no full-screen dance. Just a pill row crossfade (200 ms) and panel reveal slide (380 ms). Mode switch is a quiet act.
- Pulse: 1.5 s ease-in-out infinite for live status only. Never on errors.
- **No rotated buttons.** Stamp button keeps its shadow/letterpress feel but rotation = 0 deg. The current −0.5 deg rotate causes accessibility / vestibular issues and breaks form alignment.
- Reduced motion: respect `prefers-reduced-motion` everywhere; reveal animations replaced with opacity-only fades.

### 4.5 Component library (canonical primitives)

| Primitive | Replaces | Notes |
|-----------|----------|-------|
| `<Button variant="primary | secondary | ghost | destructive | link" size="sm | md | lg">` | StampButton + GhostButton + ad-hoc buttons | One component, variants. Primary uses tomato; destructive uses blood. |
| `<Pill active>{children}</Pill>` | FilterPill | Used for canvas pills, tag pills, filter pills. |
| `<Card>` | ProjectCard, TemplateCard | Single card with header / body / footer slots. ProjectCard etc. are *uses*. |
| `<Field label hint error>` | inline label/input | One field component used everywhere. Includes error state. |
| `<NotebookPrompt>` | (kept) | Kept for marketing + new-project surfaces only. Not used in workspace chat. |
| `<Receipt>` | (rewritten) | Tool-call receipt. Fixed broken `details` toggle (current bug). Inline diff card sub-variant. |
| `<Toast>` | — | New. Toasts for deploy success, errors, actions. |
| `<Modal>` / `<Dialog>` | — | New. Confirmation dialogs everywhere. |
| `<EmptyState>` | ad-hoc | New. Standardized empty states. |
| `<ErrorState retry={…}>` | "couldn't load logs" | New. Standardized errors with retry. |
| `<Skeleton>` | — | New. Loading skeletons for tables, cards, text. |
| `<Stat label value delta>` | Kpi | Renamed; same idea. |
| `<Crumb>` | inline | Breadcrumb component; used in TopBar. |
| `<Spinner>` | `.spin` | Inline 14 px spinner for loading buttons. |
| `<ScoreBadge dim score>` | — | New. Quality scorecard badge per dimension. |
| `<SurveyCard survey onSubmit onSkip>` | — | New. Renders an agent-emitted Survey (see §8.2.7). Handles all 7 `QuestionKind`s. Defensive against malformed input — truncates > 3 questions, dropdown-collapses > 6 options, falls back unknown kinds to `short_text`. |

### 4.6 Spacing / density

- Base unit: 4 px. All paddings, margins on the 4 px grid.
- Workspace density: compact (12 / 16 / 20 / 24).
- Marketing density: airy (24 / 32 / 48 / 64).
- Mobile: divide all desktop padding by 2 (12 → 6, 24 → 12) at < 640 px.

### 4.7 Atelier moments (used sparingly)

The editorial flair is reserved for these surfaces only:
- Marketing landing hero
- Sign-up / sign-in cards
- Home hero ("What will you make?")
- New-project page (NotebookPrompt)
- Live banner (post-deploy celebration)
- Public showcase / template detail pages

Inside the workspace, atelier flourishes are absent. Operational surfaces are quiet.

### 4.8 Implementation libraries

The agent fleet sits on top of an off-the-shelf orchestration framework. Choice committed:

#### 4.8.1 The stack (full)

```
┌────────────────────────────────────────────────────────────────┐
│  Client (React)                                                │
│   · @ai-sdk/react · useChat hook                               │
│   · Custom data-part renderers:                                │
│      <SurveyCard>  <Receipt>  <DiffCard>  <CriticRoundCard>    │
└────────────────────────────────────────────────────────────────┘
                            │ SSE (AI SDK stream protocol + data parts)
                            ▼
┌────────────────────────────────────────────────────────────────┐
│  Server function (V8)                                          │
│   Translator layer — deepagents/LangGraph events               │
│     →  AI SDK stream chunks (text/tool/data)                   │
└────────────────────────────────────────────────────────────────┘
                            │ uses
                            ▼
┌────────────────────────────────────────────────────────────────┐
│  zeroship-specific layer                                       │
│   · BranchContext, Skill registry, Critic dimensions,          │
│     ask_survey/propose_diff/file_issue tool definitions,       │
│     scorecard compute, deploy gates, post-deploy verification  │
└────────────────────────────────────────────────────────────────┘
                            │ uses
                            ▼
┌────────────────────────────────────────────────────────────────┐
│  deepagents — patterns layer                                   │
│   · planner agent (= our Builder)                              │
│   · sub-agent invocation (= Critic loop, Reviewer gate)        │
│   · todos (= PM backlog)                                       │
│   · file-as-memory (= project codebase)                        │
└────────────────────────────────────────────────────────────────┘
                            │ uses
                            ▼
┌────────────────────────────────────────────────────────────────┐
│  LangGraph — state-machine engine                              │
│   · cyclical Builder ⇄ Critic loop                             │
│   · supervisor for PM / SRE handoffs                           │
│   · checkpointing for long-running flows                       │
└────────────────────────────────────────────────────────────────┘
                            │ uses
                            ▼
┌────────────────────────────────────────────────────────────────┐
│  LangChain models — provider abstraction                       │
│   · ChatAnthropic (primary)                                    │
│   · ChatOpenAI (fallback / specific tasks)                     │
└────────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌────────────────────────────────────────────────────────────────┐
│  zeroship V8 runtime + node-compat polyfills                   │
│   · fetch + streams + crypto + buffer (per node-compat.md)     │
└────────────────────────────────────────────────────────────────┘
```

Client uses Vercel AI SDK for the chat hook. Server uses LangChain ecosystem for orchestration. A small translator at the boundary maps deepagents/LangGraph events into the AI SDK stream protocol (with custom data parts for Survey, Receipt, Diff, Critic-round). Best of both ecosystems where they each excel.

#### 4.8.2 Why deepagents

- **Pattern fit**: planner + sub-agents + todos + file-as-memory maps onto Builder + Critic/Reviewer + PM backlog + project codebase respectively.
- **Less custom orchestration code**: ~250 LOC of zeroship-specific glue, vs ~500 LOC of fully hand-rolled.
- **Battle-tested**: Claude Code-style autonomous patterns; deepagents takes from that lineage.
- **LangChain ecosystem**: provider routing, observability via LangSmith, model selection, prompt management all available.
- **Node-compat works**: per `docs/reference/node-compat.md`, the V8 runtime polyfills the Node modules LangChain/LangGraph/deepagents need.

#### 4.8.3 Mapping deepagents to our agent fleet

This mapping is grounded in the official deepagents JS docs (https://docs.langchain.com/oss/javascript/deepagents/). Most of what we'd otherwise build by hand is already provided as built-in middleware, tools, or first-class config options.

##### 4.8.3.1 What deepagents already gives us (built-in)

**Built-in middleware** (always-on, in `createDeepAgent`'s default chain):

| Middleware | What it does | What we get for free |
|------------|--------------|----------------------|
| `TodoListMiddleware` | tracks/manages a per-conversation todo list | PM agent's planning + backlog primitive |
| `FilesystemMiddleware` | virtual fs operations | foundation for project-codebase-as-memory (§12.4) |
| `SubAgentMiddleware` | spawns and coordinates subagents | Critic / Reviewer / PM / SRE delegation |
| `SummarizationMiddleware` | condenses message history | context-window mgmt for long sessions |
| `AnthropicPromptCachingMiddleware` | reduces redundant Anthropic tokens | cost optimisation when we add Anthropic (§4.8.6) |
| `PatchToolCallsMiddleware` | auto-fixes interrupted tool calls | resilience to partial failures |

**Built-in tools** (registered automatically; no `.tool()` definition needed):

| Tool | Replaces our planned | Notes |
|------|----------------------|-------|
| `write_todos` | PM agent's "create issue / feature" | already wires into the TodoList middleware |
| `ls` / `read_file` / `write_file` / `edit_file` | our `propose_diff` write path | needs a `backend` to point at the zeroship sandbox (see §4.8.3.4) |
| `execute` | shell access in sandbox | activates only when `backend` supports it |
| `task` | "Builder spawns Critic" | the canonical subagent-invocation tool |

**First-class config options** that map directly onto our spec:

| `createDeepAgent` option | Spec section it implements |
|--------------------------|----------------------------|
| `subagents: SubAgent[]` | §2.1 fleet (Critic / Reviewer / PM / SRE) |
| `middleware: Middleware[]` | §4.8.4b translator (data-part emission seam) |
| `interruptOn: { tool: …}` | §8.2.7 `ask_survey` (the survey IS the interrupt) |
| `skills: string[]` | §BB skill registry |
| `backend: AnyBackendProtocol` | wires built-in fs/exec tools to `crates/sandbox` |
| `checkpointer` | multi-turn conversation memory |
| `interruptOn` (other tools) | "review before deploy" gates (§11.4 auto-rollback approval) |

**String-based model spec**: `model: "openai:gpt-5.4"` or `"claude-sonnet-4-6"` — deepagents resolves the provider. We don't have to instantiate `ChatOpenAI` / `ChatAnthropic` ourselves.

> **See also §4.8.9** for known gaps that affect this section: G1 (`checkpointer`), G5 (`responseFormat` for Critic), G6 (per-SubAgent model selection).

##### 4.8.3.2 Revised agent-fleet mapping

| Our agent | deepagents primitive | Implementation |
|-----------|----------------------|----------------|
| **Builder** | the top-level agent (`createDeepAgent({...})`) | runs the planning loop, calls tools, emits `task` for subagents |
| **Critic** | `SubAgent` config | `{ name: "critic", description: "Reviews Builder output...", systemPrompt: CRITIC_PROMPT, model: "..." }`. Builder's loop calls `task("critic", { changes })` after each commit; Critic returns approved/issues; Builder revises. The "loop" is just Builder calling `task` repeatedly until approved or N iterations — natural deepagents shape, not a LangGraph cycle. |
| **Reviewer** | `SubAgent` (one-shot, like Critic but cheaper) + `interruptOn` for hard gates | Builder calls `task("reviewer", ...)` before merge. For human-in-the-loop hard gates (security, destructive prod migrations), `interruptOn: { deploy: { reviewerNotApproved: true } }` halts until creator confirms. |
| **PM** | dual: `SubAgent` for in-conversation queries (`@pm what's next?`) + a *separate scheduled worker* for background digests | The conversational PM is a SubAgent. The background polling PM is a worker process that reads project state and POSTs to a chat thread. |
| **SRE** | same dual: `SubAgent` for `@sre why is the app slow?` + scheduled worker for monitoring | Same pattern. The SubAgent variant runs in Builder's chat. The worker runs on a cron, files issues via the same `write_todos` interface. |
| **ask_survey** | `interruptOn` config | The agent halts when emitting a `survey` payload; the UI renders the SurveyCard; user submits → agent resumes with answers in state. *Cleaner than a tool call.* |
| **propose_diff** | wrapping built-in `write_file` with custom middleware | The `wrapToolCall` hook intercepts `write_file` calls, emits a `data-diff` UI part to the stream, then runs the actual write. Single seam. |
| **file_issue** | wrapping `write_todos` with middleware | Same pattern: `wrapToolCall` on `write_todos` fans the new todo into a `data-issue` part. |

> **See also §4.8.9** G4 — not all tools become data parts. `execute` / `read_file` / generic HTTP-fetch / SQL tools wire as native AI SDK `tool-call` / `tool-result` chunks; only tools whose UI shape ≠ I/O shape (write_file → DiffCard, write_todos → IssueCard) become data parts.

##### 4.8.3.3 Custom middleware as the data-part seam (replaces §4.8.4b's translator role)

The original §4.8.4b plan was: translator subscribes to `streamEvents` and emits AI SDK chunks for both text and custom data parts. Plan 02 Phase A landed the text-only half of that. **For data parts, use custom middleware instead.**

`createMiddleware({ wrapToolCall })` runs *around* every tool call. From inside the wrapper we have access to:
- the tool call args (before it runs)
- the tool result (after)
- a writer/state mechanism shared with `createUIMessageStream`

So `data-survey` / `data-diff` / `data-critic-round` / `data-issue` parts come from middleware, not from the translator's switch statement. The translator stays narrow: text events → text-* chunks, plus a `finish` at the end.

```ts
// Sketch — concrete code in Plan 02 Phase B
const dataPartMiddleware = createMiddleware({
  name: "DataPartEmitter",
  wrapToolCall: async (req, handler) => {
    if (req.toolCall.name === "write_file") {
      const before = await readBeforeContent(req.toolCall.args.path);
      const result = await handler(req);
      writeUIPart({ type: "data-diff", diff: { path, before, after: req.toolCall.args.content } });
      return result;
    }
    if (req.toolCall.name === "write_todos") {
      const result = await handler(req);
      for (const todo of result.added) writeUIPart({ type: "data-issue", issue: todo });
      return result;
    }
    return handler(req);
  },
});
```

The `writeUIPart` callback is plumbed through from the stream's `execute({ writer })` scope into the middleware via closure.

> **G10 verified (Phase B.0 preflight)** — interface below is copied verbatim from `node_modules/deepagents/dist/index.d.ts` (deepagents@1.9.0), not inferred. Two protocol versions ship: V1 (deprecated, returns plain values + error strings) and V2 (current, returns structured `Result` types). New backends should target V2; both are accepted via `AnyBackendProtocol = BackendProtocolV1 | BackendProtocolV2`, and `adaptBackendProtocol(...)` normalises a V1 instance to V2 shape.

##### 4.8.3.4 Backend wiring (zeroship sandbox)

Built-in fs tools (`read_file`, `write_file`, `edit_file`, `ls`) talk to deepagents' filesystem via the `backend` config. Default backend is in-memory (`StateBackend` — good for prototyping; bad for actual project files).

For real Builder behaviour, we implement a backend that talks to `crates/sandbox/` over HTTP — operations route to the per-project Docker sandbox where Builder's code edits actually land. **This is the bulk of Plan 02 Phase B work**: implement the `BackendProtocolV2` adapter (or `SandboxBackendProtocolV2` if we also expose `execute`) and pass it as `backend:`.

###### Actual interface (verified against `node_modules/deepagents/dist/index.d.ts`)

`AnyBackendProtocol = BackendProtocolV1 | BackendProtocolV2`. We target **V2**.

```ts
// dist/index.d.ts:149 — BackendProtocolV2
interface BackendProtocolV2 {
  // listing
  ls(path: string): MaybePromise<LsResult>;
  // reading
  read(filePath: string, offset?: number, limit?: number): MaybePromise<ReadResult>;
  readRaw(filePath: string): MaybePromise<ReadRawResult>;
  // searching
  grep(pattern: string, path?: string | null, glob?: string | null): MaybePromise<GrepResult>;
  glob(pattern: string, path?: string): MaybePromise<GlobResult>;
  // mutating
  write(filePath: string, content: string): MaybePromise<WriteResult>;
  edit(
    filePath: string,
    oldString: string,
    newString: string,
    replaceAll?: boolean,
  ): MaybePromise<EditResult>;
  // optional batch ops (omit if backend doesn't need them)
  uploadFiles?(files: Array<[string, Uint8Array]>): MaybePromise<FileUploadResponse[]>;
  downloadFiles?(paths: string[]): MaybePromise<FileDownloadResponse[]>;
}

// dist/index.d.ts:206 — sandbox extension (adds shell exec)
interface SandboxBackendProtocolV2 extends BackendProtocolV2 {
  execute(command: string): MaybePromise<ExecuteResponse>;
  readonly id: string;   // unique sandbox instance id
}

// Result shapes — every method returns `{ error?: string, ...data }`:
interface LsResult       { error?: string; files?: FileInfo[] }
interface ReadResult     { error?: string; content?: string | Uint8Array; mimeType?: string }
interface ReadRawResult  { error?: string; data?: FileData }
interface GlobResult     { error?: string; files?: FileInfo[] }
interface GrepResult     { error?: string; matches?: GrepMatch[] }
interface WriteResult    { error?: string; path?: string; filesUpdate?: Record<string, FileData> | null; metadata?: Record<string, unknown> }
interface EditResult     { error?: string; path?: string; filesUpdate?: Record<string, FileData> | null; occurrences?: number; metadata?: Record<string, unknown> }
interface ExecuteResponse{ output: string; exitCode: number | null; truncated: boolean }
```

Notes:
- **All methods can be sync or async** — `MaybePromise<T> = T | Promise<T>`. Our HTTP-backed sandbox adapter will be Promise-only; that's fine, the type accepts it.
- **`filesUpdate` semantics.** Checkpoint-style backends (`StateBackend`) populate `filesUpdate` so LangGraph patches its persisted state from the tool result. External-storage backends (our case — the sandbox owns the bytes) set `filesUpdate: null`. Note the `@deprecated` comment in the V2 source: zero-arg backends now send state updates internally via `__pregel_send`; new code should `if (result.filesUpdate)` before using.
- **`SandboxBackendProtocolV2.id`** is required and must be stable for the sandbox's lifetime. Maps cleanly onto our per-project sandbox id (e.g. `proj_<id>` from `crates/sandbox/`).
- **Async-bridge implication.** The protocol is `MaybePromise`; deepagents' built-in tools `await` results uniformly. No special handling needed for our HTTP-backed methods — they just return Promises. The `execute` method's `output` field carries combined stdout+stderr as a single string (truncated flag separate), so the sandbox HTTP API needs to merge streams server-side, not stream them through.
- **No `lsInfo` / `globInfo` / `grepRaw` in V2** — those were V1 names. The V2 method names are `ls` / `glob` / `grep`; the same operations exist but the V2 returns wrap data + error in a single Result object.

```ts
import { createSandboxBackend } from "../_backends/sandbox";       // we write this

const agent = createDeepAgent({
  model: "openai:gpt-5.4-mini",
  systemPrompt: BUILDER_SYSTEM,
  backend: createSandboxBackend({ sandboxUrl, sandboxToken, projectId }),
  middleware: [dataPartMiddleware],
  subagents: [criticSubagent, reviewerSubagent],
  interruptOn: { ask_survey: true },
});
```

##### 4.8.3.5 What stays "non-trivial bend"

After the deepagents docs review, almost nothing remains custom:
- The data-part emission seam is `wrapToolCall` middleware — clean.
- The Critic loop is `task("critic")` in a Builder-local while-loop — natural.
- `ask_survey` is `interruptOn` — first-class.
- File ops are built-in tools + a custom backend — straightforward adapter work.

**One thing still bespoke**: when middleware writes UI parts, it needs the `writer` from `createUIMessageStream({ execute })`. That writer is local to each request's stream scope, so middleware can't be a module-level singleton — we instantiate the agent (and its middleware chain) fresh per chat request, with the writer bound via closure. Not hard, just worth flagging.

#### 4.8.4 Client side

The client uses **Vercel AI SDK's React hooks** (`@ai-sdk/react`) for the chat surface. AI SDK does not run on the server here — its job is purely client-side message-state + streaming-UX.

Client deps:
```
@ai-sdk/react           # useChat hook + stream protocol parser
ai                      # core types (Message, UIMessage, data-part shapes)
react-markdown          # message body markdown
remark-gfm              # tables, strikethrough, task lists
shiki                   # syntax highlighting (TextMate grammars, fast)
zod                     # already present
@tanstack/react-query   # already present, used for non-chat queries
react-router-dom        # already present
```

What the AI SDK client gives us:
- **`useChat()` hook** — append, regenerate, edit-and-resend, abort, error-retry, optimistic updates, partial-frame accumulation.
- **Stream protocol parser** — handles SSE reconnection, mid-stream tool calls, data parts.
- **Type-safe message data model** — `UIMessage`, `Message`, content parts (text / tool-call / tool-result / data).
- **Data-part rendering** — custom typed parts (`Survey`, `Diff`, `CriticRound`, etc.) flow through the same stream as text and tool calls.

What the server does NOT use AI SDK for:
- LLM provider calls — those go through LangChain models (`ChatAnthropic`, `ChatOpenAI`).
- Tool definitions — those are LangChain's `tool()` schemas (consumed by deepagents).
- Agent orchestration — that's deepagents/LangGraph.

#### 4.8.4b Server-side translator (deepagents → AI SDK protocol)

> **Revised after Plan 02 Phase A + deepagents docs review.** The translator is now narrower than originally planned — text events only. Data parts (Survey, Diff, CriticRound, Issue) flow through custom middleware (§4.8.3.3), not through the translator's switch statement.
>
> **Outstanding gaps** before this section is implementation-ready (see §4.8.9): **G2** plumb `AbortSignal` through `streamEvents`; **G3** add resume-after-interrupt handling for `ask_survey`; **G7** extend `convertUIMessagesToLangChain` to preserve `tool-call` / `tool-result` parts.

A focused module (`apps/zeroship-builder/src/server/_translator.ts`, **~110 LOC**, landed in Plan 02 Phase A) lives at the chat-procedure boundary. Its single job: drain the agent's LangGraph stream-events and emit AI-SDK v6 UI Message Stream chunks for the **text** path. Tool args, tool results, and our custom data-part shapes all enter the stream from middleware (§4.8.3.3), which has cleaner access to args/results than `streamEvents` does.

##### What the translator handles (text only)

```typescript
// concrete code — see apps/zeroship-builder/src/server/_translator.ts on the redesign branch
export async function buildTranslatedStream(input: BuilderTurnInput) {
  const { createDeepAgent } = await import("deepagents");
  const { ChatOpenAI } = await import("@langchain/openai");

  const agent = createDeepAgent({
    model: "openai:gpt-5.4-mini",                  // string-based provider spec
    systemPrompt: BUILDER_SYSTEM,
    backend: sandboxBackend(...),                   // Phase B
    middleware: [dataPartMiddleware(writer)],       // §4.8.3.3 emits data parts
    subagents: [critic, reviewer],                  // §4.8.3.2 fleet
    interruptOn: { ask_survey: true },              // §8.2.7 → first-class
  });

  return createUIMessageStream({
    async execute({ writer }) {
      const textId = crypto.randomUUID();
      let textStarted = false;

      for await (const ev of agent.streamEvents(
        { messages: convertUIMessagesToLangChain(input.messages) },
        { version: "v2" as const },
      )) {
        switch (ev.event) {
          case "on_chat_model_stream": {
            const delta = extractTextDelta(ev);
            if (delta) {
              if (!textStarted) {
                writer.write({ type: "text-start", id: textId });
                textStarted = true;
              }
              writer.write({ type: "text-delta", id: textId, delta });
            }
            break;
          }
          case "on_chat_model_end":
            if (textStarted) {
              writer.write({ type: "text-end", id: textId });
              textStarted = false;
            }
            break;
          // Tool/data events: handled by middleware, not here.
        }
      }
    },
  });
}
```

##### What does NOT belong here (anymore)

| Was planned in v1 of this section | Now lives in |
|------------------------------------|--------------|
| `case "on_tool_start"` → `tool-call-streaming-start` chunk | not emitted; AI SDK v6 doesn't need streaming-input-start when the tool result is a data part |
| `case "on_tool_end"` → tool-result chunk | not emitted for our custom tools (write_file/ask_survey/etc.); their outputs become data parts via middleware |
| `case "on_chain_end"` → `critic-round` data part | replaced by middleware that wraps the `task("critic")` invocation and writes a `data-critic-round` part |
| Survey output as `data-part` from tool-end | replaced by `interruptOn: { ask_survey: true }` — the agent's interrupt state directly carries the Survey shape; middleware writes it as `data-survey` when the interrupt fires |

**Reason for the narrowing**: `streamEvents` doesn't give middleware-level access to before/after tool args, and parsing tool args from generic LangGraph events is brittle across model providers. `wrapToolCall` middleware sees the typed args and result directly, so emitting data parts from there is cleaner and provider-agnostic.

##### What still belongs in the translator (and only here)

- `on_chat_model_start/stream/end` → `text-start` / `text-delta` / `text-end` chunks. Provider-specific delta shape handled in `extractTextDelta()`.
- The lazy import of `deepagents` (per §4.8.5 bundle weight mitigation).
- The `convertUIMessagesToLangChain()` adapter (UI shape `parts: [{ type: "text", text }]` → LangChain `HumanMessage` / `AIMessage`).
- The `createUIMessageStream` boilerplate that owns the `writer` lifecycle (and passes it to middleware via closure).

##### Custom data-part types

| Part name | Payload | Rendered as |
|-----------|---------|-------------|
| `survey` | `Survey` (per §8.2.7) | `<SurveyCard>` |
| `diff` | `{ path, before, after }` | `<DiffCard>` |
| `receipt` | `{ tool_name, status, summary }` | `<Receipt>` (also auto-derived from tool-call/tool-result chunks) |
| `critic-round` | `{ round, total, issues, approved }` | `<CriticRoundCard>` (small inline indicator) |
| `pm-update` | `{ kind: 'milestone' | 'issue', payload }` | toast or inline link to plan canvas |
| `sre-alert` | `{ severity, summary, link }` | toast + notification badge |

Client-side, the `useChat` hook surfaces data parts via its `data` callback or custom message-part renderers. Each type has a dedicated React component.

> **See also §4.8.9** G8 (per-request `createDeepAgent` cost — measure, cache by config hash if material) and G9 (lazy-import is per-isolate-boot, not per-call — wording in this section is misleading and needs correction).

#### 4.8.5 Bundle weight mitigation

Server: deepagents + LangGraph + LangChain models is ~1 MB compiled.
Client: `@ai-sdk/react` + `ai` core types is ~30 KB.

Mitigations:

1. **Server: never load LangChain on the client.** Chat UI uses AI SDK's stream parser; LangChain ecosystem stays server-only.
2. **Server: lazy import** — `await import('deepagents')` only when Builder is *first invoked*, not on every server function call. Light tasks (auth, list apps, list files) don't load it.
3. **V8 isolate cache reuse** — the worker keeps the loaded module graph in its isolate's compilation cache; second-invocation cost is much lower than first.
4. **Pin versions** — `@langchain/core@1.x`, `@langchain/langgraph@1.x`, `deepagents@1.x`, `@ai-sdk/react@^1.x` + `ai@^4.x` (matched pair — these track different major lines) pinned to avoid surprise breaks.
5. **Facade layers** — all deepagents/LangChain calls go through `crates/control/src/agents/` modules; the translator (§4.8.4b) is the *only* place that knows about both ecosystems. When upgrading versions, only the facade or translator changes.

#### 4.8.6 Version churn risk (LangChain + AI SDK)

LangChain ships breaking changes frequently. AI SDK is more stable but moves too. Risk mitigations beyond pinning:

- **Snapshot testing**: agent behavior captured as fixtures (server-side); chat UX captured via Playwright snapshots (client-side). An upgrade that breaks either flags itself.
- **Facade layers** (above): isolate upgrade pain to the translator (`stream_translator.ts`) and the `crates/control/src/agents/` modules.
- **CI canary jobs**: weekly tests against `@langchain/*@latest` AND `@ai-sdk/*@latest`, both run independently so a break in one doesn't hide a break in the other.
- **Acceptance**: we treat occasional 1-day upgrade work as the cost. Worth it for the custom-code savings.

#### 4.8.7 Cleanup of current `package.json`

Keep:
- `@langchain/anthropic`, `@langchain/core`, `@langchain/langgraph`, `@langchain/openai`, `deepagents` (these are already there — they were leftover but now properly used)
- `@tanstack/react-query`, `react`, `react-dom`, `react-router-dom`
- `@codemirror/*` *if* CodeMirror wins over Monaco — see §4.5 component decision; revisit during V1
- `clsx`, `tailwind-merge`
- `zod` (used by tool definitions)

Add:
- `@ai-sdk/react` — client chat hook (`useChat`)
- `ai` — core types only (Message, UIMessage, data-part shapes); not used as runtime on server
- `react-markdown`, `remark-gfm`, `shiki`

Remove (unused or replaced by our component library):
- `@radix-ui/*` — replaced by our own primitives in `<Button>`, `<Modal>`, `<Toast>`, etc.
- `class-variance-authority` — our `cva`-style needs are simple; one helper in `lib/utils.ts` covers them
- `lucide-react` — atelier brand uses very few icons; we can ship the ~5 we need as inline SVGs
- `unenv` — Node-compat shim left over from pre-zeroship runtime; not needed
- `@uiw/react-codemirror` — only if Monaco wins; otherwise keep
- `@vitejs/plugin-react` belongs in devDeps not deps (already a misfile)

#### 4.8.8 Decision rationale recap

| Option | Verdict |
|--------|---------|
| Vercel AI SDK + custom orchestration (server) | Rejected — gives up pre-built agent patterns |
| AI SDK + LangGraph, no deepagents | Rejected — less pattern coverage, similar weight |
| deepagents + LangGraph + LangChain, custom client | Rejected — reimplements `useChat`-grade UX; bug surface |
| **deepagents + LangGraph + LangChain on server, AI SDK on client, translator at the seam** | **Chosen.** Best-of-both: pre-built agent patterns + battle-tested chat UX; V8 + node-compat makes server stack viable; translator (~200 LOC) is the only glue |
| LangChain alone (no agent framework) | Rejected — too low-level |

#### 4.8.9 Known gaps and follow-ups (post Plan 02 Phase A review)

After landing Plan 02 Phase A (real OpenAI streaming through the deepagents → translator → useChat path) and re-reading the official deepagents JS docs, these are the gaps and corrections that must land before Phase B. They're organised by severity. Each item names where in this spec the fix belongs, plus the design direction.

##### Critical — block correctness for multi-turn

###### G1 · Conversation memory across turns (`checkpointer`)

**Problem.** Plan 02 Phase A's `buildTranslatedStream` calls `createDeepAgent(...)` *inside* the chat handler. Every turn instantiates a fresh agent. Without a `checkpointer`, the deepagents middleware-managed state — todos (`TodoListMiddleware`), virtual fs entries (`FilesystemMiddleware`), summarisation state — is **discarded after each turn**. Builder forgets what it was doing. `useChat` sends full message history so the LLM sees prior text, but middleware state lives outside `messages`.

**Fix.** Add `checkpointer: BaseCheckpointSaver` to `createDeepAgent`. Persist by `chat_session_id` (provided by useChat as the `id` field on each request body). Initial implementation can use deepagents' in-memory checkpointer for dev; production swaps in a Postgres-backed saver writing to the control plane DB.

**Lives in.** §4.8.3.2 (extend the `createDeepAgent` example), §4.8.3.6 *new* "Conversation memory model".

###### G2 · `AbortSignal` propagation through to the LLM

**Problem.** `useChat`'s Stop button closes the SSE on the client. The translator's `for await` loop sees the writer error and exits — but the underlying LangChain HTTP call to OpenAI isn't aborted. We keep paying for tokens after the user cancelled.

**Fix.** The zeroship runtime exposes a `Request` to server functions with a native `signal: AbortSignal`. Plumb it: `chat(input, ctx)` → `buildTranslatedStream(input, signal)` → `agent.streamEvents(input, { version: "v2", signal })`. LangChain respects `AbortSignal` natively.

**Lives in.** §4.8.4b (translator signature change), Plan 02 Phase B.0 work item.

###### G3 · Resume protocol after `interruptOn`

**Problem.** Spec §8.2.7 says `ask_survey` ⇒ `interruptOn`. Agent halts → UI renders SurveyCard → user submits. But `useChat` doesn't have a "resume" verb — only `sendMessage`. Without a defined resume contract:
- The user's submit becomes a normal new turn
- The agent has no way to know the new turn is "the answer to the interrupt I just emitted"
- The interrupted run's state is lost

The deepagents API for resume is `agent.invoke({}, { configurable: { thread_id }, resume: { value } })`. Wiring this through `useChat` needs explicit protocol design.

**Fix.** New subsection §8.2.7.x "Resume protocol":

```
1. When middleware emits `data-survey`, also include an opaque `resume_token`
   in the part payload (same as `thread_id` for the checkpointer + the
   interrupt's run id).

2. SurveyCard's submit handler calls `sendMessage` with a custom body shape:
     {
       json: {
         resume: {
           token: <resume_token>,
           value: { answers, skipped }
         }
       }
     }
   instead of the normal { messages } body.

3. Server-side chat handler detects `body.resume`. If present, calls
   `agent.invoke({}, { configurable: { thread_id }, resume: ... })` instead
   of `agent.streamEvents({ messages })`.

4. Agent resumes from the interrupt; middleware emits new chunks; client
   renders them as the assistant message continues.
```

This is real protocol work. Plan B.0 includes it.

**Lives in.** §8.2.7 + §4.8.4b (handler shape).

##### Important — bad architecture without a fix

###### G4 · Tool-call rendering: data parts ≠ everything

**Problem.** §4.8.3.3 collapses all tool I/O onto custom `data-*` parts. Overcorrection. AI SDK v6 has typed `tool-call` / `tool-result` message parts with built-in renderer affordances. They're the right shape when the args / result *are* what the user sees.

**Fix.** Two-track rule:

| Tool | UI shape | Wire as |
|------|----------|---------|
| `write_file` (creator cares about before/after) | DiffCard | **`data-diff`** custom part |
| `write_todos` (creator cares about issue tracking) | IssueCard | **`data-issue`** custom part |
| `task("critic", ...)` (multi-dim feedback) | CriticRoundCard | **`data-critic-round`** custom part |
| `execute` (shell — npm install / running tests) | streaming text/lines | **native `tool-call` + `tool-result`** chunks |
| `read_file` (creator wants to see the request) | "Read X" line + content | **native `tool-call` + `tool-result`** chunks |
| Future raw HTTP / SQL / etc. | generic Receipt | **native `tool-call` + `tool-result`** chunks |

Rule of thumb: **if the UI's natural rendering shape differs from the tool's I/O shape, custom data part. Otherwise native.** The translator (§4.8.4b) handles native chunks; middleware handles data parts.

**Lives in.** §4.8.3.3 (revise), §10 (Receipt + DiffCard component spec).

###### G5 · Critic needs `responseFormat` (structured output)

**Problem.** §11.1 says Critic returns `[{dimension, severity, issue, suggested_fix, line?}]`. Without `responseFormat: z.object(...)` on the Critic SubAgent, deepagents returns prose; we'd parse free-form text. Fragile.

**Fix.** Critic SubAgent config:

```ts
const critic: SubAgent = {
  name: "critic",
  description: "Reviews Builder output across quality dimensions.",
  systemPrompt: CRITIC_PROMPT,
  model: "openai:gpt-5.4-mini",                  // cheap; see G6
  responseFormat: z.object({
    approved: z.boolean(),
    issues: z.array(z.object({
      dimension: z.enum(["correctness","security","performance",
                          "accessibility","ux_completeness","responsive",
                          "code_health"]),
      severity: z.enum(["low","medium","high","critical"]),
      issue: z.string(),
      suggested_fix: z.string(),
      line: z.number().int().optional(),
    })),
  }),
};
```

**Lives in.** §4.8.3.2 (extend the example), §11.1 (note).

###### G6 · Per-SubAgent model selection

**Decision (2026-05-01).** Standardise all agents on **`openai:gpt-5.4-mini`** for V1. The model is `gpt-5.4`-family — fast, cheap-enough, and the same quality bar across Builder + SubAgents avoids regressions where a smaller-model Critic mis-grades a larger-model Builder's output. Per-SubAgent model differentiation deferred until cost data justifies it.

| SubAgent | Model | Why |
|----------|-------|-----|
| Builder (top-level) | `openai:gpt-5.4-mini` | Default; user sees its output |
| Critic | `openai:gpt-5.4-mini` | Structured via `responseFormat`; same family as Builder for grading parity |
| Reviewer (CI gate) | `openai:gpt-5.4-mini` | Same |
| PM (chat-mode) | `openai:gpt-5.4-mini` | Same |
| SRE (chat-mode) | `openai:gpt-5.4-mini` | Same |

**Future tuning (deferred).** Once we have token-cost telemetry per agent, revisit:
- Critic running 3× per turn is the heaviest spend → may benefit from a smaller model **iff** parity stays acceptable. Keep `responseFormat` Zod schema as the constraint that lets us drop quality without losing structure.
- PM / SRE on smaller models is plausible; they're mostly summarisation.

The "fast / balanced / thorough" project setting (§11) controls *iteration count* of the Critic loop, not the model.

**Lives in.** §4.8.3.2 (table), §11 (cost-tier note).

###### G7 · UI-message → LangChain conversion drops tool history

**Problem.** Plan 02 Phase A's `convertUIMessagesToLangChain` filters non-text parts. Once we emit native `tool-call` / `tool-result` (per G4), the next turn's converter loses them — Builder's history of "what tools did I just call and what did they return" disappears. Builder will repeat tool calls or get confused.

**Fix.** Extend the converter:
- v6 `{type: "tool-call", toolCallId, toolName, args}` part on assistant message → LangChain `AIMessage({ tool_calls: [{id, name, args}] })`
- v6 `{type: "tool-result", toolCallId, result}` part → LangChain `ToolMessage({ tool_call_id, content })`
- Custom `data-*` parts: not sent back to the model (they're UI-only)

**Lives in.** §4.8.4b ("convertUIMessagesToLangChain" subsection).

##### Concerns — worth noting in the spec

###### G8 · Per-request `createDeepAgent` cost

Compiling a LangGraph + registering middleware happens per chat request. Cost not yet measured. If material, cache the compiled graph by `(model, tools-hash, subagents-hash, middleware-hash)` and clone state per request. Note in §4.8.5 as a deferred optimisation.

###### G9 · "Lazy import" mitigation is per-isolate, not per-call

§4.8.5 implies lazy import keeps non-chat fns off the deepagents bundle cost. True only on cold V8 isolates. After the first chat request, the isolate's module cache holds deepagents forever — `apps.ts`'s `listApps` running on the same isolate after a chat call still pays the loaded cost. The win is per-isolate-boot, not per-call. **Wording fix in §4.8.5.**

###### G10 · `AnyBackendProtocol` shape unverified

§4.8.3.4 references `backend: AnyBackendProtocol` as the wire from built-in fs tools to `crates/sandbox`. Method signatures (`readFile`, `writeFile`, `ls`, `execute`) and shape were inferred from the docs, not from `node_modules/deepagents/dist/*.d.ts`. **Phase B preflight: verify the actual exported type before implementing the sandbox adapter.**

###### G11 · AI SDK v6 stream-protocol choice unjustified

We use `createUIMessageStreamResponse` (v6 UI Message Stream / SSE flavour). Right for `useChat` — but spec doesn't justify it over `createDataStreamResponse` or `streamText().toTextStreamResponse()`. **Add a 1-paragraph justification in §4.8.4.**

##### Minor — polish

###### G12 · System prompts inline

Plan 02 Phase A has `BUILDER_SYSTEM` inline in `_translator.ts`. With Critic / Reviewer / PM / SRE SubAgents coming, prompts move to `apps/zeroship-builder/src/server/_prompts.ts` (or per-agent files under `_agents/`).

###### G13 · LangSmith / observability not wired

§4.8.6 mentions LangSmith as a benefit. Phase B should set the env vars (`LANGSMITH_API_KEY`, `LANGSMITH_TRACING=true`) and route traces. Debugging the Critic loop without LangSmith will be miserable.

###### G14 · `zod` vs `z` from `@zeroship/server`

Examples use `z` from `@zeroship/server`. Our `chat.ts` uses raw `zod`. Standardise on `@zeroship/server`'s `z` (it's a re-export of the same lib but consistent across the workspace).

##### Phase B preflight (B.0) work order

Before adding tools or surveys, land these in order:

1. **G2** plumbing — `AbortSignal` from request → translator → agent. Easy, immediate win.
2. **G1** checkpointer — pick in-memory for dev, design Postgres-backed shape for prod. Without this, multi-turn is broken.
3. **G3** resume protocol — design + implement client / server contract for surveys. Required for Phase B's first feature.
4. **G7** converter extension — handle tool-call / tool-result history. Required as soon as we emit native tool-call chunks.
5. **G10** verify `AnyBackendProtocol` shape — read the deepagents source, document the actual interface in §4.8.3.4.
6. **G12 / G14** code hygiene — extract prompts, standardise zod import. Two-line change.

Then Phase B.1 adds tools (write_file via backend + ask_survey via interruptOn), Phase B.2 adds Critic SubAgent with `responseFormat` and cheap model.

---

## 5 · Pre-auth surfaces

### 5.1 Marketing landing (`/`)

Goal: convince a stranger to sign up. Currently *the entire thing is missing*.

Sections (top → bottom):

1. **Hero** — Fraunces display headline; one-sentence value prop; primary CTA "Start a project →"; secondary CTA "See an example".
2. **Demo loop** — autoplaying 25-second screen recording of prompt → preview → ship. Captioned, mute-by-default, replayable.
3. **Skill catalog teaser** — 21 domain pack tiles with "+ 18 capability skills". CTA to `/skills`.
4. **What you get** — 4 column cards: AI agents (Builder + Critic + PM + SRE), zeroship runtime, monetization (Stripe payouts), quality scorecard.
5. **Pricing summary** — three plans, "+ 15 % platform fee on what your apps earn". CTA to `/pricing`.
6. **Showcase strip** — 6 templates / public projects (V2+: real apps; V1: 6 curated templates). CTA to `/templates`.
7. **Trust band** — "SOC2 in progress · GDPR aware · 99.95 % SLA". (Honest copy — not "SOC2 certified" until we are.)
8. **Footer** — links to /docs, /changelog, /legal/*, /status, /about, social.

### 5.2 Pricing (`/pricing`)

Three plans:

| Plan | Price | Includes |
|------|-------|----------|
| Free | $0 | 3 projects · 50 k req / month · zeroship subdomain · platform branding · 15 % share applies if monetized |
| Pro | $20/mo | Unlimited projects · 1 M req / month · custom domain · no platform branding · 15 % share |
| Enterprise | contact | SLAs, SSO, audit log export, dedicated support |

Below the plan grid: a clear "**The 15 % platform fee**" section with worked example ($100 in revenue → Stripe ~$3.20 → platform $14.52 → creator $82.28). This number is also stated *at first deploy* and *at first earning*, so users see it three times before any money moves.

FAQ: 8 entries, including "what happens if I cancel?", "do I own my code?" (yes, exportable), "can I leave the platform?" (yes, `.zsapp` bundle download).

### 5.3 Skill catalog (`/skills`)

Single page with the full BB inventory, organized by domain pack and capability skill. Each item:
- Title
- One-line description
- "Used in N projects" stat (after launch)
- Example apps that use it

This page is the marketing page that does the most work. Searchable, filterable.

### 5.4 Public templates (`/templates`)

Grid of TemplateCards. Filter by category. Each template clicks through to `/templates/:slug` with screenshots, the prompt that generated it, the live demo URL, the skills + theme + feature sets used, and a "Use this template →" CTA (opens `/signup?template=…` if logged out).

### 5.5 Other public pages

- `/docs` — searchable docs hub. V1: 12–20 essential pages.
- `/changelog` — chronological updates.
- `/showcase` — V2+. Public projects opted in.
- `/legal/*` — Terms, Privacy, AUP. Standard SaaS legal.
- `/status` — embedded status page.
- `/about` — company, mission, contact.

---

## 6 · Auth flows

### 6.1 Sign-up (`/signup`)

Atelier card, 440 px max width. Three fields: email, password (8+ chars, zxcvbn meter), display name. Or "Continue with Google" / "Continue with GitHub".

After submit:
1. Account created.
2. Email verification dispatched (link valid 24 h).
3. **No verification block** for first-project creation (verification is required before deploy).
4. Redirect to `/?return=…` or `/onboarding/intent`.

ToS/Privacy checkbox required, with link to `/legal/*`. No "we'll only show your name on projects you publish" copy unless we actually have that toggle wired (we will).

### 6.2 Sign-in (`/login`)

Same card. Email + password or OAuth. "Forgot password" link. Magic-link toggle (V1.5).

Errors (inline, in `font-serif italic text-blood`, *not* tomato):
- Wrong credentials: "Email or password is incorrect."
- Unverified: "Check your email — we sent a verification link." with resend.
- Rate limited: "Too many attempts. Try again in 5 minutes."

### 6.3 Forgot password (`/forgot-password`)

Single field, email. After submit: always says "If that email is in our system, we sent a link." (no email-existence leak).

### 6.4 OAuth flow

Google + GitHub via standard OAuth. Returns to `/oauth/callback?return=…`. On success, sets session cookie, redirects.

OAuth errors render in the same `<ErrorState>` on `/login` with a dedicated "google sign-in: …" copy.

### 6.5 2FA / sessions / account deletion

- 2FA: V1.5. TOTP via authenticator app + recovery codes.
- Active sessions list: V1.5. Per-device with last-seen, IP-coarse, revoke.
- Account deletion: V1. Hard delete after 30-day grace; data export available.

---

## 7 · Onboarding (first run)

Goal: brand-new user → first ship in under 60 seconds.

### 7.1 Intent question (one screen)

After signup, before the empty home: a single question with chip answers.

> *"What are you trying to build?"*
> [a website] [a tool for me] [an internal tool] [a SaaS product] [a marketplace] [I don't know yet]

Skippable. Answer used to seed prompt examples on the home page and to suggest templates.

### 7.2 Empty home

Hero: *"What will you make?"* + NotebookPrompt. Three example chips below seeded by the intent answer. Below that, "Or start from a template →" linking to `/templates`.

### 7.3 First prompt → ship

User submits prompt → navigate to `/p/:appId/preview` while the workspace mounts. The chat rail receives the prompt automatically and starts streaming. Status line on top bar: "thinking… → writing files → reviewing → deploying → live".

### 7.4 First deploy celebration

`<LiveBanner>` shown above the canvas (full width, ink black, tomato accent corner, ivy "live" pulse). Includes:
- "*{appName}* is live."
- Copy URL pill
- Three follow-on CTAs: open in tab, custom domain, share

The *15 % share* is mentioned discreetly in the celebratory copy: "*Free for now — when this earns money, we take 15 %.*"

### 7.5 Optional product tour

A skippable 4-step tour after first ship, triggered by a small "Take the tour" pill in the top bar:
1. The chat (right rail)
2. The canvas pills (preview / logs / plan / health / settings)
3. The mode toggle (+ data / + code)
4. The plan canvas (your AI PM)

Dismissible forever.

---

## 8 · Project lifecycle

### 8.1 Project gallery (home, authed)

```
TopBar
─────────────────────────────────────
What will you make?
{NotebookPrompt — full width, 18 px serif}
[chip: a recipe site] [chip: tip calculator] [chip: notes app]

─── Recent work ─────────────────  N projects
[ProjectCard] [ProjectCard] [ProjectCard] …
```

Each ProjectCard:
- № in tomato italic
- Project name (Fraunces 22 px)
- Tagline (italic, 1–2 lines clamped)
- Footer: status pulse (ivy "Live" *or* "Draft") + relative timestamp + scorecard mini badge (`A · B+ · A` for security · code · perf, opt-in)
- Hover: lifts 3 px, paper-fold corner expands

Above the project grid (when projects exist): a single search field + sort dropdown (recent / name / status) + filter chips (all / live / draft / archived).

Pin / favorite is an inline action on the card (top-right small star icon, 1-click toggle).

### 8.2 App creation wizard

**Big design choice**: project creation has *three* flows that all lead to the same place. The default is the simplest possible (no wizard at all — the chat is the wizard). The other two exist for users who want more control upfront.

#### 8.2.1 Default flow — *chat is the wizard*

90 % of project creation should follow this path. Aligned with the product's "AI does the work" pitch.

```
Home page
─────────────────
What will you make?
[NotebookPrompt — full width]
[chip] [chip] [chip]                                ← examples (intent-aware)

Submit (⌘+↵)
   │
   ▼
Behind the scenes (≤ 800 ms):
  1. Reserve auto-generated slug (3 first-prompt words → kebab-case, suffix on collision)
  2. Create project record + prod/dev branches + default theme/plan
  3. Insert PM milestone "v0.1 — first ship"
  4. Insert PM issue "set up project per prompt"
  5. Stash prompt in sessionStorage
   │
   ▼
Navigate to /p/:appId/preview
   │
   ▼
Workspace mounts:
  - Chat rail auto-sends the user's prompt as turn 1
  - Top bar shows project name = autogenerated; URL pill = {slug}.zeroship.app
  - Status pulse: "thinking…"
  - Preview canvas shows pre-deploy empty state with progress
   │
   ▼
Builder ⇄ Critic loop runs (3 iterations default)
   │
   ▼
Reviewer + hard gates → first deploy succeeds
   │
   ▼
LiveBanner: "{appName} is live."
   │
   ▼
PM digest fires: "v0.1 shipped. Want to add Auth, Comments, or Search next?"
```

**Why this works**: zero friction. The user types a sentence and sees their app being built in seconds. The chat IS where they will live anyway, so dropping them into it immediately is the right framing.

**What if the prompt is too thin?** Builder's *first* response includes 1–3 inline clarifying questions before writing code:

> *"Got it — a recipe app for your supper club. A few quick questions:*
> *  · Sign-in for guests, or anonymous? `[sign-in]` `[anonymous]`*
> *  · Photos uploaded by guests? `[yes]` `[no]`*
> *  · Public to anyone, or invite-only? `[public]` `[invite-only]`"*

Click-to-answer chips. User picks → Builder writes code. No separate wizard step needed; the chat is doing wizardy work.

#### 8.2.2 Advanced flow — `/new` for upfront customization

For users who care about brand (custom name, custom URL, particular theme) before any code is written. Reached by:

- Clicking "Customize before begin →" link beneath the home prompt
- Direct nav to `/new`
- Clicking "+ New Project" from any header (which now opens this page rather than the default flow, since the user clearly wanted to customize)

```
─── Begin a project ──────────────────────────────────────────────

Describe what you want to make
┌──────────────────────────────────────────────────────────────┐
│ A recipe sharing app for my supper club where guests         │
│ can sign in, post photos, and vote on who hosts next…        │
└──────────────────────────────────────────────────────────────┘
⌘+↵ to begin · drop images for inspiration

Builder reads it as ↓     (refreshes 600 ms after user stops typing)

┌─ I see this as ────────────────────────────────────────────┐
│  Name:    Supper Club                          [edit]      │
│  URL:     supper-club.zeroship.app             [edit]      │
│                                                            │
│  Domain:  Social / community                               │
│  Skills:  Auth · File upload · Comments · Voting [+ more]  │
│  Theme:   Editorial newspaper                  [change]    │
│                                                            │
│  Anything else?                                            │
│  ┌────────────────────────────────────────────────────┐    │
│  │ (optional — refinements, examples, references)    │    │
│  └────────────────────────────────────────────────────┘    │
└────────────────────────────────────────────────────────────┘

Plan:  Free (3 projects · 50k req/mo)            [change]

                                            [   ⏵ Begin   ]
```

**Behavior**:

- The "I see this as" card is generated client-side via a lightweight inference call (separate from Builder's full code-gen). Updates as the user edits the prompt.
- All inferred values are **inline-editable**:
  - **Name** click → inline input. Updates URL slug live unless user has touched the slug.
  - **URL** click → inline input with `.zeroship.app` suffix locked. Validates uniqueness in real-time, suggests `-2` / `-3` suffix on collision.
  - **Skills** click `[+ more]` → drawer with the full skill catalog filtered by relevance. User can add or remove skills. Each removal shows what feature it provides ("removing Voting means no voting feature").
  - **Theme** click `[change]` → drawer with theme thumbnails. Click to pick.
  - **Plan** click `[change]` → drawer with plan comparison.
- "Anything else?" is a second textarea for refinements (becomes part of the prompt context Builder receives).
- "Begin" button: all settings committed, project created with the chosen config, redirects to workspace where Builder kicks off with the full prompt + skill / theme / plan context already attached.

**No separate confirm step**. Begin is the commit.

#### 8.2.3 Template-driven flow — `/new?template=<slug>`

Reached from `/templates` (public or authed). The template's defaults pre-fill the wizard:

```
─── Begin from template: Newsletter ──────────────────────────────

What you'll get out of the box (preview the template →)
  · Subscriber list with Stripe-integrated paid tiers
  · Email composer with markdown
  · Sent-issues archive
  · Editorial newspaper theme

Anything different about yours?
┌──────────────────────────────────────────────────────────────┐
│ I want it to feel more playful, with a yellow accent…        │
└──────────────────────────────────────────────────────────────┘

Name:  My Newsletter        [edit]
URL:   my-newsletter.zeroship.app  [edit]
Theme: Editorial newspaper  [change]
Plan:  Free                 [change]

[← Back to templates]                    [   ⏵ Begin   ]
```

**Behavior**:
- Skills + feature sets pre-loaded from the template's manifest, not displayed individually (the template is the unit).
- "What you'll get" lists the template's headline features.
- The textarea captures *deltas* from the template defaults ("more playful, with a yellow accent" → Builder applies these on top of the template's first build).
- Begin commits and navigates to workspace.

#### 8.2.4 Behind the scenes — what Begin actually does

For all three flows, "Begin" runs:

```
1. Validate slug (uniqueness, format, no reserved names)
2. Create project record:
     INSERT INTO projects (id, owner_id, slug, name, plan, ...)
3. Create branches:
     INSERT INTO branches (project_id, name='prod', kind='long_lived', ...)
     INSERT INTO branches (project_id, name='dev',  kind='long_lived', parent='prod')
     CREATE SCHEMA app_<id>__prod;
     CREATE SCHEMA app_<id>__dev;
4. Apply theme tokens to project (writes initial CSS / Tailwind config)
5. For each selected skill / feature set: load into project's active list, run skill's installer step
6. Create initial PM milestone: "v0.1 — first ship", target = today + 1d
7. Create initial PM issue: "Set up the project per the creator's prompt"
8. Stash full prompt + skill/theme context for Builder's first turn
9. Insert audit_log entry: { actor: human, action: project.created, payload: {...} }
10. Navigate to /p/:appId/preview
11. Workspace auto-sends the prompt as the first chat turn
12. Builder picks up issue #1, starts work
```

Steps 2–9 happen in one DB transaction. Step 10 is a client-side navigation. Step 11–12 happen as soon as the workspace mounts and the chat hook reads the stashed prompt.

#### 8.2.5 Mobile / tablet wizard

Single-column on phone (< 600 px):

```
─── Begin a project ──────────
Describe what you want to make
[full-width textarea]
[example chip] [example chip]

Inferred ↓
[Name]    Supper Club        ✎
[URL]     supper-club.…      ✎
[Skills]  Auth · Photos · Voting +
[Theme]   Editorial          change

[      Begin      ]   ← full-width
```

The "Inferred" card collapses to a one-tap row on phone:

```
[Inferred · 4 settings        ▾]
```

Tap to expand; tap again to collapse. The Begin button stays at the bottom, full-width, accessible without scrolling.

On tablet portrait, both the prompt and inference card fit on one screen without scrolling.

#### 8.2.6 Edge cases

| Case | Behavior |
|------|----------|
| Empty prompt + Begin | Button is disabled. Hint: "Describe your idea — even one sentence is enough." |
| Slug collision (default flow) | Auto-append `-2`, `-3`, etc., until free. Toast on workspace mount: "URL `supper-club-2.zeroship.app` was taken — yours is `supper-club-3.zeroship.app`. Change in Settings if you'd like." |
| Slug collision (advanced flow) | Inline error on the URL field: "Taken. Try `supper-club-2`." with click-to-accept suggestion. |
| Network error during Begin | Toast: "Couldn't reach the server. Retry?" Button reverts to Begin. Form state preserved. |
| Free-tier user has 3 projects already | Begin button replaced with "You've reached the free-tier limit." + Upgrade CTA + "Or archive a project to free a slot." |
| Inappropriate prompt (CSAM, etc.) | Pre-creation moderation check; Begin returns "This prompt was flagged. Reach out if you think this is a mistake." |
| Prompt requests something zeroship can't build (e.g. "build me a kernel driver") | Builder's first response is "I can't build that — here's what I can do. Want to revise?" with link back to skill catalog. Project is still created (Builder can be redirected). |
| User abandons mid-creation | Project shell + branches not yet created (default flow) or saved as draft (advanced flow auto-saves prompt to localStorage). On return, prompt is restored. |

#### 8.2.7 Agent-generated surveys (the dynamic clarification mechanism)

The wizard does **not** carry a hardcoded question registry. Builder *generates* the questions per prompt. The platform supplies the **contract** (so the agent's output is renderable) and the **renderer** (so the chat surfaces structured questions natively).

This decision applies wherever the system asks the user something: first-turn clarifications in the wizard, mid-build "which approach?" forks, pre-deploy destructive-confirms, SRE post-incident reviews, feature-set configuration. **One mechanism, many uses.**

##### Type contract

```typescript
type Survey = {
  preamble?: string;        // "Got it — a few quick things:"
  questions: Question[];    // 0–3 questions; renderer truncates beyond 3
  skip_label?: string;      // default: "skip — just build"
}

type Question = {
  id: string;               // stable client key — used in SurveyResponse
  prompt: string;           // user-facing question text
  kind: QuestionKind;
  default?: unknown;        // applied if user skips
  required?: boolean;       // default false; true blocks skip for this Q
}

type QuestionKind =
  | { type: "single_choice"; options: Option[] }    // ≤ 6 options enforced
  | { type: "multi_choice";  options: Option[]; min?: number; max?: number }
  | { type: "short_text";    placeholder?: string; max_length?: number }
  | { type: "long_text";     placeholder?: string; max_length?: number }
  | { type: "yes_no" }
  | { type: "scale";         min: number; max: number; labels?: [string, string] }
  | { type: "image_upload";  max_count?: number; hint?: string }

type Option = {
  value: string;
  label: string;
  hint?: string;            // optional 1-line description shown beneath the chip
}

type SurveyResponse = {
  survey_id: string;
  answers: Record<QuestionId, unknown>;   // typed per kind
  skipped: boolean;                        // true if user pressed skip
}
```

##### Builder native tool: `ask_survey`

Exposed as `zeroship.builder.ask_survey(survey)` returning `Promise<SurveyResponse>`. Tool-call mechanic: pauses Builder mid-stream, surfaces the survey to the chat rail, resumes when the user submits.

```typescript
// Builder-facing signature
zeroship.builder.ask_survey(survey: Survey): Promise<SurveyResponse>
```

Operationally identical to existing `sandbox_*` and `deploy_*` tools — same receipt mechanic, same streaming pause, same audit trail. Tool calls are persisted in `chat_messages.tools_jsonb`, so survey + response live in conversation history naturally.

> **Implementation note (post-deepagents review).** `ask_survey` is best implemented as a deepagents `interruptOn` configuration, not a generic tool. When the agent emits a survey payload it halts (interrupt state carries the `Survey` shape); custom middleware writes the `data-survey` UI part; the user submits → the conversation re-runs with answers injected into the agent's state. This is the canonical deepagents pattern for human-in-the-loop and matches our shape exactly. Other tools' outputs (Diff, CriticRound, Issue) flow through `wrapToolCall` middleware as data parts. See §4.8.3.3 + §4.8.3.5.
>
> **Open gap — G3 in §4.8.9**: how the user's `SurveyResponse` actually gets back into the agent's interrupted state via `useChat`. The current handwave ("user submits → agent resumes") needs a concrete client/server protocol (custom request body shape: `{json: {resume: {token, value}}}`; server detects `body.resume` and calls `agent.invoke({}, {configurable: {thread_id}, resume: ...})`). Plan 02 Phase B.0 lands this before any survey emission.

##### Three-layer constraint enforcement

The platform doesn't enumerate questions; it constrains *shape*:

| Layer | Enforces |
|-------|----------|
| **Builder system prompt** | "Ask at most 3 questions. Each must have a clear default. Use chip-based kinds when possible. Don't ask anything the prompt already implies. Always offer skip." |
| **Critic** (new sub-dimension under correctness) | Reviews the emitted `Survey` shape before it surfaces. Rejects if: > 3 questions / no skip path on a non-required survey / wording duplicates information already in prompt / option count > 6 on a `single_choice`. Sends back for revise. |
| **Renderer** (`<SurveyCard>`) | Defensive: silently truncates > 3 questions to top 3, collapses single_choice with > 6 options to a `<Select>` dropdown, falls back unknown `kind` to `short_text`. Never throws on malformed input. |

Result: the form factor is platform-controlled; the content is fully agent-driven. New domains, new prompts, new clarification needs — no spec changes required.

##### Render in chat rail

A `<SurveyCard>` primitive (added to §4.5 component library) renders inside the assistant message. Layout:

```
─── Builder ─────────────────────────────────────────
 Got it — a recipe app for your supper club. A few
 quick things:

 ┌──────────────────────────────────────────────────┐
 │ Who can use it?                                  │
 │ [ just my supper club ]  [ anyone with a link ]  │
 │ [ public ]                                       │
 │                                                  │
 │ Vibe?                                            │
 │ [ cozy / warm ]  [ minimal ]  [ playful ]        │
 │                                                  │
 │ Anything else?                                   │
 │ ┌────────────────────────────────────────────┐   │
 │ │ optional…                                  │   │
 │ └────────────────────────────────────────────┘   │
 │                                                  │
 │           [ skip — just build ]   [ send → ]     │
 └──────────────────────────────────────────────────┘
```

Behavior:

- One single_choice question per row of chips (wraps on narrow widths).
- multi_choice chips have a checkmark indicator, with `min` / `max` enforced client-side.
- `short_text` / `long_text` render as inline inputs with character counts when `max_length` is set.
- `yes_no` is a tomato/ivy pair of chips.
- `scale` is a 1-N row of chips with optional end labels.
- `image_upload` is a drag-zone that accepts `max_count` images.
- Submit button is disabled until all `required: true` questions are answered.
- Skip is always available unless every question is `required`.
- After submit: the survey card collapses to a one-line summary ("Answered: just my supper club · cozy/warm · *(no extras)*") that remains in the chat history.

##### Response written to conversation history

When the user submits, two things happen:

1. The structured `SurveyResponse` is returned to Builder via the tool-call mechanic.
2. A user-facing **prose summary** is appended to the chat as a normal user message ("just my supper club, cozy/warm, no extras"). Future Builder turns see this prose summary in conversation; the structured form is also retrievable from `tools_jsonb`.

This dual representation means: the chat reads naturally to a human, the agent has structured access to answers, and surveys are first-class conversation content rather than out-of-band metadata.

##### Reuse beyond the wizard

The same `ask_survey` is invoked across the product:

| Where | Example |
|-------|---------|
| Wizard / first turn | "Got it — who can use it? cozy/warm/playful?" |
| Mid-build clarification | "I see two valid auth approaches — magic-link or password? `[magic link]` `[password]`" |
| Pre-deploy destructive confirm | "This migration drops a column from prod. Confirm?  `[yes, drop it]` `[no, cancel]`" (single_choice with `required: true`) |
| Feature-set configuration | When the creator says "add Stripe", Builder surveys for tier shape: subs / one-time / both, free trial yes/no, currency |
| SRE post-incident review | "I rolled back v0.12 because errors spiked. Was the rollback right? `[yes — keep it rolled back]` `[no — re-deploy and I'll watch]`" |

Same renderer, same contract, same constraints. Different content per moment.

##### Constraints recap

- **Cap** = 3 questions per survey, enforced by Critic + truncated by renderer.
- **Skip** is always possible unless every question is `required: true`.
- **Defaults** must be specified by Builder; renderer applies them on skip.
- **Question kinds** limited to the 7 listed; renderer is defensive on unknowns.
- **Option count** for `single_choice` is ≤ 6; renderer collapses to dropdown above.

##### What this replaces

The earlier proposal ("category-specific question packs", "fixed factor list of 3") is dropped entirely. No question registry. No category-question mapping. The agent decides; the platform renders.

#### 8.2.8 Wizard analytics

Events emitted (per §28):

- `project.creation_started` (with flow: `default | advanced | template`, prompt_length, has_image)
- `project.inference_shown` (advanced flow only — when the inference card renders)
- `project.inference_edited` (which fields the user changed: name, slug, skills, theme, plan)
- `survey.shown` (with question count, kinds, source: wizard / mid-build / pre-deploy / config / sre)
- `survey.answered` (with answered count, skipped flag, time-to-respond)
- `survey.skipped` (full skip)
- `project.creation_completed` (latency from prompt → workspace mount)
- `project.creation_abandoned` (advanced flow — user navigated away without Begin)

These let us measure: default vs advanced flow conversion, which inferred field is most-edited (poor inference signal), how often Builder asks surveys (Builder over-asking is a red flag), survey skip rate per source.

### 8.3 Project archive / delete / transfer

In Settings canvas, danger zone. Always double-confirm with the project name typed back ("type 'supper-club' to delete").

---

## 9 · Workspace canvases

Each canvas occupies the same shell slot. Internal layout differs.

### 9.1 Preview canvas

```
┌─ printed plate frame ─────────────────────────────────────┐
│ ⦿ ⦿ ⦿   https://supper-club.zeroship.app    ↻ reload  ↗ open │
│─ plate bar ────────────────────────────────────────────────│
│                                                            │
│           [iframe of the live app]                          │
│                                                            │
└────────────────────────────────────────────────────────────┘
   device: [💻] [🖥️] [📱]    last shipped: 4 minutes ago
```

- Iframe sandbox: `allow-scripts allow-same-origin allow-forms allow-popups allow-modals`.
- `?_v=${deployVersion}` cache-busts on every successful deploy.
- Device frame switcher: desktop (1280) / tablet (768) / phone (375). Iframe size shrinks; canvas centers it.
- Click-to-edit overlay (V1.5): toggle in plate bar. With it on, hovering an iframe element outlines it; clicking opens a chat composer prefilled with "Change [element description]: ".
- Pre-deploy state: empty-state copy "Nothing's been built yet. Tell the agent what to make in the chat." with a link to focus the chat composer.
- Failed-deploy state: red-amber bar with message + link to /logs.

### 9.2 Files canvas (+Code only)

```
┌── files ──────┬────────── editor ───────────────┬── meta ─┐
│ search…       │ [tab1] [tab2] [+]                │ Path    │
│ ▸ src/        │                                  │ Size    │
│   ▾ server/   │                                  │ Modified│
│     auth.ts   │  Monaco editor with TS LSP        │ Author  │
│     db.ts     │                                  │ ─────── │
│   client/     │                                  │ Files: N│
│ package.json  │                                  │         │
└───────────────┴──────────────────────────────────┴─────────┘
```

- File tree: 240 px, collapsible, search at top.
- Editor: full Monaco. TypeScript LSP. Autocomplete from project + zeroship type definitions.
- Tabs: open files; cmd-click to split.
- Save: Cmd+S manual; auto-save 2 s after edit.
- Diff view: toggle button in editor toolbar; shows diff vs last deploy.
- Find / replace: Cmd+F (file), Cmd+Shift+F (project).
- Context menu: right-click on tree → New file, New folder, Rename, Delete, Move, Download, Upload.
- Read-only mode: when viewing an old deploy.
- Right meta panel: 200 px. Path / size / modified / author of currently open file.

### 9.3 Data canvas (+Data and +Code)

The Data canvas is the largest non-chat surface in the workspace. It has its own sub-tabs and its own branch switcher in the top of the canvas.

```
┌─ Data ────────────────────────────────────────────────────────────┐
│ Branch: [▾ dev]   |   Tables · Schema · Indexes · Migrations · Backups │
├───────────────────────────────────────────────────────────────────┤
│ … sub-tab content …                                               │
└───────────────────────────────────────────────────────────────────┘
```

**Branch switcher** at the top of the canvas changes which branch the rows / schema / migrations are reading from. It's the *most prominent* control on this canvas because operating on the wrong branch is the most-common foot-gun in this kind of tool. The current branch's name appears in the workspace top-bar URL pill too: `dev--supper-club.zeroship.app` vs `supper-club.zeroship.app`.

#### 9.3.1 Tables sub-tab (default)

**Notion-style row view (Ali-friendly, default in +Data):**
```
Tables: [contacts] [deals] [pipeline_stages]  + new

[All ▾]  [+ Filter]  [Sort]  [☐ Bulk]  [Search]                215 rows
────────────────────────────────────────────────────────────────────
| ☐ | Name           | Email          | Stage     | Updated |
|───|────────────────|────────────────|───────────|─────────|
| ☐ | Ada Lovelace   | ada@…          | ⮕ Lead    | 2h ago  |
| ☐ | Alan Turing    | alan@…         | ⮕ Customer| 1d ago  |
| ☐ | …              |                |           |         |
────────────────────────────────────────────────────────────────────
+ Add row
```

- Inline edit (click cell).
- Pagination: 50 rows; virtualized.
- Sort: click column header.
- Filter: filter button → expression builder (no SQL).
- Foreign keys: column type rendered as a pill `⮕ Lead`, click jumps to that row.
- Bulk: checkbox column. Header checkbox = select all visible. Bulk bar appears at bottom: *Edit · Duplicate · Delete · Export*.
- Saved views (button right of `[All ▾]`): named filter+sort+column-order, scoped to project. Switch view by clicking; "Save current as view…".
- Soft-delete: deleted rows hold for 24 h in a "deleted" filter; can be restored.

**Schema view (+Code only — toggle button at top right):**
```
contacts
├── id          uuid PRIMARY KEY
├── name        text NOT NULL
├── email       text UNIQUE NOT NULL
├── stage_id    uuid REFERENCES pipeline_stages(id)
└── updated_at  timestamptz DEFAULT now()

[ Run query ▾ ]   [SQL editor]
```

SQL editor: Monaco with SQL LSP. Saved queries. Read-only by default; "enable writes" toggle (with confirmation). `EXPLAIN ANALYZE` rendered as a tree.

#### 9.3.2 Schema sub-tab

Visual ER diagram of all tables in the active branch:

```
┌─────────────┐         ┌──────────────┐
│  contacts   │   *──1  │  pipeline_   │
│             │◀────────┤   stages     │
│ id, name,   │         │ id, name,    │
│ email,      │         │ order        │
│ stage_id    │         └──────────────┘
└─────────────┘
       1
       │ *
       ▼
┌─────────────┐
│   deals     │
│ contact_id, │
│ amount, ... │
└─────────────┘
```

- Tables shown as cards; foreign keys as arrows.
- Click a table → side panel with full column list, indexes, constraints, statistics.
- Drag to rearrange (layout persisted per project).
- "Add table", "Edit table" (opens AI prompt: "describe the table you want") buttons.
- Right side panel for selected table:
  - **Columns**: name, type, nullable, default, indexed
  - **Indexes**: name, type, columns, size, last-used
  - **Constraints**: unique, NOT NULL, defaults, CHECK
  - **Statistics**: row count, size, distinct/null per column
  - **Migration history**: timeline of schema changes with author + branch + brief

#### 9.3.3 Indexes sub-tab

Flat list of all indexes across the active branch:

```
table         index               type    columns           size    last used
contacts      contacts_pkey       btree   (id)              16 kB   2m ago
contacts      contacts_email_uk   unique  (email)           48 kB   2m ago
deals         deals_contact_idx   btree   (contact_id)      32 kB   2h ago
…
```

- Sort by table / size / last-used.
- "Suggest indexes" button → SRE proposes indexes based on slow-query analysis.
- "Drop unused" button → flags indexes never used in the last N days.

#### 9.3.4 Migrations sub-tab

```
Branch: dev                                                           [+ New migration]

✓  20260430.142512  add password reset            Builder · 4m ago
✓  20260430.140003  add stage_id index            Builder · 8m ago
○  20260429.180000  drop legacy_field             Builder · pending  [Apply]   [Discard]
✓  20260428.090000  initial schema                Builder · 2d ago
…
```

- Status icons: `✓` applied, `○` pending, `✕` failed/rolled-back.
- Click a migration → detail with up/down SQL, author, branch, deploy linkage, safety report (Builder + Critic notes).
- "Apply" runs against active branch with safety checks. Pre-apply preview shows expected row impact and lock duration.
- "Rollback" available where down SQL exists.
- "New migration" — opens prompt to Builder ("describe the schema change you want").
- **Migration linter** (Critic dimension): flags breaking changes (drop column, NOT NULL without default, type narrow, FK without index) before apply. On `prod` branch, breaking changes are hard-blocked unless explicit creator opt-in.

#### 9.3.5 Backups sub-tab

```
Branch: prod                                                                [Snapshot now]

Daily auto · 2026-04-30 03:00      4.2 GB   17 h ago     [Restore →]
Daily auto · 2026-04-29 03:00      4.1 GB   1 d ago      [Restore →]
Manual    · before-payment-mig    4.1 GB   2 d ago      [Restore →]
…
```

- "Snapshot now" → manual snapshot with optional name.
- Restore behavior: applies to a *new branch* (`restore-{snapshot-name}-{date}`); creator promotes after verification. Never overwrites the active branch directly.
- Retention shown per plan (24 h free / 7 d Pro / 30 d Enterprise).
- Cross-region replication (V2+).

### 9.4 Media canvas (+Data and +Code)

Grid of uploaded files. Filter by type (image / video / audio / other). Drag to upload. Click to preview. Copy public/signed URL. Right-click for delete / download / rename.

Storage usage shown at top (e.g., "2.4 GB of 10 GB used").

### 9.5 Logs canvas (Maker tier)

```
[all] [info] [warn] [error]    [last 1h ▾] [⏸ pause] [↓ tail]      214 events

09:42:11.234  INFO   request  GET /api/contacts → 200 (12ms)
09:42:11.501  INFO   request  POST /api/votes  → 201 (43ms)
09:42:14.998  WARN   build    bundle size 480kB → 510kB (budget 500kB)
09:42:18.342  ERROR  request  POST /api/auth/login → 500 (db connection)
                              ↳ pq: connection refused
                              [show full payload]
```

- Real-time stream via SSE.
- Filter by level / source / time range.
- Search box: full-text.
- Pause/resume tail; auto-scroll while at bottom.
- Click a row → expand to full payload (request headers, query, response, stack trace).
- Color-coded levels: ivy (info), amber (warn), blood (error).
- Real timestamps; tooltip shows absolute + timezone.
- Export CSV/JSON button.

### 9.6 Env canvas (+Code only)

Same as current Env tab but **fully wired** (the current is read-only-pretending-to-save). Two sections: Variables, Secrets.

- Variables: key/value/last-updated rows with edit + delete.
- Secrets: key/(masked value)/last-updated. Add new (with confirmation since values are write-once-readable). Rotate (replaces value). Delete.
- Audit log link in section header: "see env change history" → opens audit log filtered to env actions.
- Per-environment vars (V2): tabs for dev / preview / prod.

### 9.7 Settings canvas (Maker tier)

```
General
───────
Project name      [{name}]                    [Save]
Tagline           [{tagline}]                 [Save]
Project icon      [{drag image}]              [Save]
Visibility        ( ) private  ( ) unlisted  (•) private

Domain
──────
Studio URL        supper-club.zeroship.app           (read-only)
Custom domain     [{domain input}]   [Verify DNS →]

Plan
────
{plan card with usage + upgrade button}
Spending limit   $[___]/month with alert at [___]%

Danger zone
───────────
[Transfer ownership]  [Archive]  [Delete project]
```

**Every input has a Save button.** Dirty state shows orange dot in tab title.

Custom domain flow:
1. Enter domain → instructions appear: "Add this CNAME: `cname.zeroship.app`"
2. Click "Verify DNS" → check + report status
3. On success: TLS auto-provisioned (Let's Encrypt via gateway), domain becomes active.
4. Multi-domain (V2): list of domains with primary + redirects.

### 9.8 Plan canvas (PM agent's home)

Three sub-tabs: **Issues · Roadmap · Deployments**.

#### Issues

```
Open (12)  Closed (47)            [filter ▾]  [+ New issue]
───────────────────────────────────────────────────────────
🔴  Login form rejects valid emails        SRE · 2h ago
●   Add password reset                     PM · 1d ago
○   Tagline edit doesn't save              you · 3d ago
…
```

- Status icon: red filled (critical), red ring (high), amber (medium), grey (low).
- Source attribution (SRE / PM / you / Builder).
- Click → detail with comments, linked deploys, status timeline.
- Status change inline: open → in-progress → closed.

#### Roadmap

- Kanban: Proposed | Planned | Building | Shipped (option to switch to list)
- Cards: feature title + description + linked issues + ETA + agent assigned
- "Suggest features" — PM proposes 3 based on usage / gaps; creator promotes / dismisses

#### Deployments

```
v0.12   2 minutes ago   ✅ live        🟢 A-     PM: "added password reset, fixed auth bug"
v0.11   3 hours ago     ✅ live        🟢 A      PM: "first ship"
…
```

- Click a deploy → detail (diff vs prior, scorecard, linked issues, changelog).
- Rollback button on detail page.

PM digest at top of plan canvas: "Here's what's in flight, here's what shipped today, here's what I'd suggest next."

### 9.9 Health canvas (SRE agent's home)

Four sub-tabs: **Status · Quality · Incidents · Performance**.

#### Status

```
🟢 All systems normal · last check 12 s ago

Uptime  99.97 %  (7 d)
Errors  0.4 %    (1 h)
p95     124 ms   (1 h)

Watching:
  · Uptime probe (every 60 s)
  · Error rate threshold > 2 %
  · Latency p99 > 500 ms
  · DB query p95 > 200 ms
```

#### Quality (the scorecard)

```
Overall  A-   (85)
─────────────────────────────────────────────────
  Correctness    A   (90)   compile / typecheck / smoke
  Security       A+  (96)   no secrets · all CVEs clean
  Performance    B+  (84)   bundle 480 kB · LCP 1.8s
  Accessibility  B   (78)   3 axe violations
  UX completeness A-  (88)   all forms validated · 1 missing empty state
  Code health    A   (90)   lint clean · type cov 92 %
  Reliability    A   (92)   uptime 99.97 % · 0 incidents 7d

[Run quality check now]
```

Click any dimension → drill-down with flagged issues, severity, fix suggestion. "Fix it for me" button asks Builder to address them.

#### Incidents

Timeline of past incidents: started → mitigated → resolved. Each shows: detection time, root cause (SRE's analysis), fix applied, downtime, scorecard delta.

#### Performance

Charts: requests/sec, p50/p95/p99 latency, error rate, over 24 h / 7 d / 30 d. Filter by route.

---

## 10 · Chat surface (deep)

### 10.1 Layout

```
┌─ Notes & thoughts          12 turns      clear ─┐
│                                                  │
│ scrollable msg list                              │
│   user msg  → tomato left rule, serif 15px       │
│   assistant → eyebrow "BUILDER", serif 14.5px    │
│      ↳ Receipt cards (one per tool call)         │
│      ↳ Diff card (if code changed)               │
│   …                                              │
│                                                  │
├──────────────────────────────────────────────────┤
│ [composer with red ruler]                        │
│ describe what to change…                         │
│ [📎 attach] [🖼 image]                            │
│ ↵ for newline · ⌘↵ to send       [   SEND   ]   │
└──────────────────────────────────────────────────┘
```

- Width: 320 default; resizable 280–640 via gutter.
- Collapse to 36 px right strip with chevron icon; click to expand.
- Sticky-to-bottom while at bottom; pause auto-scroll if user scrolls up; "↓ N new" pill appears when paused.

### 10.2 Composer

- Textarea, auto-resize 2–10 rows, Source Serif 14.5 px.
- **Enter = newline, ⌘+Enter = send.** This is the opposite of the current code and prevents accidental sends.
- Drag-and-drop / paste images directly into composer; thumbnails appear above textarea with × to remove.
- File upload via 📎 button (PDF, JSON, code, etc., max 5 files / 25 MB total).
- Slash commands: `/deploy`, `/explain`, `/test`, `/undo`, `/help`, `/sre`, `/pm`. Autocomplete on `/` typed.
- @mentions: `@server.ts`, `@route:/api/login`, `@env:STRIPE_KEY`, `@pm`, `@sre`, `@critic`. Autocomplete on `@`.
- Cost meter (bottom-right of composer, optional): "≈ 4 k tokens · ≈ 12 s".

### 10.3 Send / streaming / stop

- On submit:
  1. User msg appears with tomato left rule.
  2. Assistant placeholder appears with "thinking…" italic.
  3. Stream tokens into placeholder.
  4. Tool calls appear as Receipt cards inside the assistant turn.
  5. Status line in top bar updates: thinking → calling tool → reviewing (Critic loop) → deploying → live.
- **Stop button**: 36 px tall, blood-colored, anchored bottom-right of chat (replaces SEND while busy). Cannot be missed.
- On stop: partial content kept; "Cancelled by you" italic line appended.

### 10.4 Receipt cards

```
✓  Wrote src/server/votes.ts                      details
   Critic approved (round 2/3)
```

- Status icon: `…` spinner (running), ivy `✓` (done), blood `✕` (error).
- Plain-language sentence (`humanize()` rewritten to be exhaustive across all built-in tools).
- Sub-line: Critic round info ("approved round 2/3"), or warning ("Critic flagged: missing error handler", inline blood text), or empty.
- "details" button → expands inline (fixed from current bug); shows input/output JSON.
- For deploy receipts: "✓ Built and shipped — *it's live in a moment*" plus an inline pulse-dot animation.

### 10.4.1 Survey cards (clarification questions)

When Builder calls `ask_survey` (see §8.2.7), the chat receives a tool call rendered as a `<SurveyCard>` instead of a normal receipt. The card pauses the streaming generation visually (spinner + "waiting for you…" line beneath Builder's message), accepts user input, and on submit collapses to a one-line summary that stays in the chat history. The structured response unblocks Builder's next turn.

Survey cards differ from regular receipts in three ways:
- They block forward progress (Builder is waiting on the user).
- They render as interactive UI, not a static fact.
- They're skippable; receipts are not.

The same card primitive renders surveys regardless of source — wizard first turn, mid-build forks, pre-deploy confirms, SRE post-incident reviews, feature-set configuration. One UX pattern, many invocation points.

### 10.5 Diff cards

When Builder modifies a code file, the receipt is followed by a diff card:

```
diff  src/server/votes.ts                       view full
- export function vote(req: Request) {
+ export async function vote(req: Request) {
+   const { user } = await requireUser(req);
    …
```

- Inline 3-line context above/below.
- Click "view full" → modal with full diff viewer (Monaco diff editor).

### 10.6 Regenerate / edit

- Hover an assistant turn → "↻ regenerate · ✎ edit prior" appears.
- Regenerate: replaces current assistant turn with a new one (preserves the user prompt that produced it).
- Edit prior: lets user edit any prior user message; on save, conversation is truncated after that message and the new turn fires.
- Undo / redo at conversation level (V1.5).

### 10.7 History

- "12 turns" counter top of chat.
- Click counter → opens history modal with all past sessions for this project (grouped by date).
- Each session can be re-opened (read-only) or "fork" (start a new conversation with this session's context).
- Conversation always persisted per project (current behavior kept).

### 10.8 Errors and retry

When agent errors, the assistant turn ends with a blood-colored error block:

```
**Error: control plane returned 502 (timeout)**
[ Retry ]   [ Dismiss ]
```

Retry resends the last user turn. Dismiss removes the error block.

---

## 11 · Quality control system

(Architecture detail, complementing GG in the inventory.)

### 11.1 Critic ⇄ Builder loop

```
User chat turn
      │
      ▼
Builder writes code  ──────┐
      │                    │
      ▼                    │
   Critic reviews          │
      │                    │
      ├─ approved? ─yes──▶ Reviewer (CI gate) ──▶ Deploy
      │                    │
      └─ no, feedback ─────┘
                  ↑
                  │
                  └─ revise (loop, max N iterations)
```

- Critic produces structured JSON feedback: `[{dimension, severity, issue, suggested_fix, line?}]`.
- Builder receives feedback as additional context; revises in same turn.
- Max iterations per project: 1 (fast) / 3 (balanced, default) / 5+ (thorough).
- If max reached without approval: change ships (unless creator opted into hard-block on Critic), with remaining concerns surfaced as soft-warning issues filed by Critic.

> **Implementation (post-deepagents review).** Critic is a `SubAgent` config on `createDeepAgent`, not a separate runtime. Builder calls `task("critic", { changes })` after each commit; the SubAgent runs with its own system prompt + (smaller) model and returns `{ approved: bool, issues: [...] }`. Builder's planning loop checks the result; if not approved and iteration count < N, Builder revises and calls `task("critic")` again. The "loop" is a plain JS `while` inside Builder's planning — *not* a custom LangGraph cycle. A custom middleware wraps the `task("critic", ...)` invocation to emit `data-critic-round` UI parts (round / total / approved / issues). See §4.8.3.2.
>
> **Required gaps to close** (per §4.8.9): **G5** Critic SubAgent must declare `responseFormat: z.object(...)` for structured output (not free-form text parsing); **G6** all SubAgents (Critic / Reviewer / PM / SRE) standardised on `openai:gpt-5.4-mini` for V1 — same family as Builder; per-agent model tuning deferred until cost telemetry justifies divergence.

### 11.2 Pre-deploy gate matrix

| Check | Type | Tier | Override |
|-------|------|------|----------|
| Build succeeds | hard | always | NO |
| Typecheck passes | hard | always | NO |
| No secrets in client bundle | hard | always | NO |
| No critical CVEs in deps | hard | always | NO |
| Smoke tests pass | hard | always | NO |
| **Migration safety** (no destructive ops on `prod` w/o opt-in) | **hard** | **always** | **creator opt-in flag only** |
| **Migration + code coupling** (schema change ships with the code that uses it) | **hard** | **always** | NO |
| Critic score ≥ project min | soft | balanced+ | yes (creator confirm) |
| a11y baseline (no critical axe) | soft | balanced+ | yes |
| Performance budget | soft | thorough | yes |
| Visual regression | soft | thorough | yes |

For monetized projects: security score ≥ 80 becomes a hard gate. No payment-handling code can deploy below this threshold.

### 11.3 Post-deploy verification

- T+0 to T+60 s: SRE smoke tests against live URL (predefined per skill — Auth skill smoke-tests login, Payments skill smoke-tests checkout).
- T+0 to T+5 min: error rate compared to pre-deploy baseline. If > 2× baseline → auto-rollback.
- T+0 to T+1 h: Lighthouse run; scorecard updated.
- T+0 to T+24 h: real-user-monitoring aggregate; PM-digestable.

### 11.4 Auto-rollback

Triggered by:
- Post-deploy error rate > 2× baseline within 5 minutes of deploy
- Critical health-check failure (uptime probe fails 3 consecutive)
- Memory / CPU runaway detection

On rollback:
- Previous deploy restored (atomic via gateway route flip).
- Notification fired: "Auto-rolled back v0.12 → v0.11 (error rate 12% → 0.4% on rollback). SRE filed an issue."
- SRE files an incident in plan canvas with full timeline.

### 11.5 Scorecard storage and history

Per-deploy, the scorecard is computed and stored in `quality_scores`:
```
deploy_id, timestamp, dim, score, weight, details_jsonb
```

Aggregated views:
- Per-project current (latest deploy)
- Per-project trend (last 30 deploys)
- Public badge URL: `https://app.zeroship.app/badge.svg` (opt-in, server-rendered)

### 11.6 Skill quality budgets

Each skill's spec includes:
```yaml
skill: auth
quality_budgets:
  security:
    - rate_limit_login: required
    - csrf_protection_state_changes: required
    - secure_cookie_defaults: required
    - password_hash_argon2_or_bcrypt: required
  reliability:
    - failed_login_logs: required
```

Critic enforces these budgets when reviewing code that uses skill X. A change that uses Auth without rate limiting is flagged hard.

---

## 11.5 · Branching architecture (data, env, deploy)

Branching is the most architecturally significant addition after the agent fleet. It cuts across:

- **Data canvas** — branch switcher; every Data sub-tab reads from the active branch
- **Env canvas** — per-branch env-var overlays
- **Logs canvas** — per-branch log streams
- **Deploy pipeline** — preview branches, branch-specific URLs, atomic prod promotion
- **Critic** — migration safety as a quality dimension; destructive ops on `prod` blocked
- **SRE** — monitors `prod` by default; can be configured for any branch
- **Platform layer** — `compio-postgres` needs branch-aware schema isolation (V1) and CoW backend (V2)

### 11.5.1 Branch model

```
project
└── branches
    ├── prod  ← default deploy target (the live URL points here)
    ├── dev   ← default working copy (forked from prod)
    └── preview-{deploy_hash}*  ← short-lived, auto-created on PR-style deploys, TTL 7d
```

Each branch is an isolated bundle of:

- **Schema** — its own tables, columns, indexes, constraints
- **Data** — its own rows
- **Env vars** — overlay on top of project-level vars (branch wins on conflict)
- **Migrations history** — linear per-branch; merge-able to other branches
- **Deploy attachment** — most recent build associated with this branch
- **URL** — `{branch}--{slug}.zeroship.app`; the special `prod` branch maps to bare `{slug}.zeroship.app`

Branches are addressable in the workspace via:
- The Data canvas branch switcher (most prominent)
- The Settings → Branches list (full CRUD)
- The chat: `@builder on prod`, `@builder on dev`

### 11.5.2 Branch lifecycle

**Create**:
- Default `dev` and `prod` are created on first deploy.
- New branches: `[+ Branch] from {parent_branch}` in Settings → Branches.
- Forks are logical (cheap) in V1: same database, separate schema namespace.
- Default fork direction: dev forks from prod ("snapshot of prod for safe testing").

**Use**:
- Builder commits land on the branch the creator was last viewing in Data canvas (or explicitly addressed via chat).
- Schema migrations run *only* on the active branch unless explicitly merged.
- Deploys can target any branch; production deploy means promoting a branch's most recent build to `prod`.

**Merge**:
- Schema merge: applies migrations from source to target. Conflicts surface a resolution UI (rename / drop / keep both).
- Data merge: not supported in V1 (you can't reasonably merge two divergent rowsets). V2: small-table data merges with conflict UI. Most workflows don't need this — they want schema flow without data flow.
- Merge dry-run: Builder simulates against a shadow branch, reports impact (row count affected, lock duration).

**Promote** (a preview branch → prod):
- The same as merging schema + flipping the gateway URL pointer.
- Triggers full safety pipeline: Critic, Reviewer, hard gates, post-deploy verification.
- Atomic — no half-promoted state.

**Delete**:
- `prod` cannot be deleted.
- Long-lived branches: confirm with row count + size.
- Preview branches: auto-delete after TTL (default 7 days).

### 11.5.3 Per-branch env vars

Env canvas shows variables in three tiers:

```
┌─ Variables (active branch: dev) ────────────────────────────────┐
│  STRIPE_KEY        sk_test_dev_…       branch · dev    edit del │
│  DATABASE_URL      (from project)      project         edit del │
│  ENABLE_FEATURE_X  true                branch · dev    edit del │
└─────────────────────────────────────────────────────────────────┘
```

Resolution: branch overlay wins over project-level. Secrets get the same overlay model. "Copy env from prod to dev" available with a checkbox to mask secrets.

### 11.5.4 Per-branch deploys

```
project: supper-club

Deploys (most recent first)
─────────────────────────────────────────────────────────────────
v0.14   prod     2 min ago    A-     "added password reset"
v0.13   dev      5 min ago    B+     "experimenting with voting"
v0.12   prod     2 h ago      A      "first ship"
v0.11   preview-a1b2c3   1d   B      (merged into prod as v0.12)
…
```

The Plan canvas → Deployments view shows branch column. Filter by branch.

Production URL: `supper-club.zeroship.app` (always points to prod's most recent successful deploy).
Branch URLs: `dev--supper-club.zeroship.app`, `preview-a1b2c3--supper-club.zeroship.app`.

### 11.5.5 Critic + Reviewer in a branched world

Critic gains an additional dimension: **migration safety**.

- Detects destructive changes (DROP COLUMN/TABLE, type narrowing).
- Checks for safe migration patterns (NOT NULL adds with defaults; multi-step migrations for breaking changes; FK adds with covering indexes).
- On `prod` branch, hard-blocks destructive changes unless creator explicitly opts in (separate from soft-block on dev).

Reviewer enforces:
- Migrations and code changes ship in the *same deploy* (no schema-only deploys whose code can't read the new schema yet).
- Production-targeting changes have passed `dev` first (configurable per project; default ON for projects > 7 days old).
- Pre-deploy gate: hard-block if a migration is destructive AND target is `prod` AND creator opt-in not flagged.

### 11.5.6 SRE in a branched world

- SRE monitors the production branch by default.
- For non-prod branches: SRE skips uptime probes and error-rate alerts (these are noisy on dev) but tracks schema changes and missing indexes.
- SRE-proposed fixes target the same branch where the bug was detected.
- A creator can subscribe SRE to a non-prod branch (e.g. a long-lived staging branch) via Settings.

### 11.5.7 Platform support (V1 vs V2)

- **V1**: Postgres branching = separate schemas inside the per-app database. `compio-postgres` plugin learns to address `project_<id>__<branch>` schemas. Logical clones; new branches have empty data unless explicitly seeded from parent.
- **V1.5**: parent-branch data seeding on fork (manual snapshot copy on fork creation; cheap for projects under N MB).
- **V2**: integration with a CoW Postgres backend (Neon-equivalent) — true at-rest copy-on-write branching. Storage / blob branching also supported.

This is a **platform-level dependency** for the builder UX. The Data canvas branch switcher cannot ship before `compio-postgres` supports per-branch schemas. Tracked as a release-coupled dependency in the implementation plan.

---

## 12 · Builder agent + skills catalog

### 12.1 Builder loop (per turn)

Implemented as a deepagents *planner agent* whose graph contains the Critic loop as an internal cycle (see §4.8.3).

1. Parse user request + tool / file context.
2. Determine which skills to load (LLM-driven retrieval over skill registry).
3. **Decide whether to clarify**: if confidence on prompt intent / scope / vibe is low, emit a `Survey` via `ask_survey` (see §8.2.7). Wait for `SurveyResponse`. Skip step if confidence is high.
4. Plan: high-level steps. (deepagents' planner produces the plan; visible to user as PM-tracked items if substantive.)
5. Execute: call tools (sandbox file ops, env reads, package installs, etc.).
6. Submit to Critic *(sub-agent invocation in a LangGraph cycle node)*.
7. Iterate per Critic feedback (max N) — the cycle either approves or returns feedback for revision.
8. Submit final commit to Reviewer *(sub-agent, one-shot)*.
9. On Reviewer pass → deploy.

### 12.1.1 Tools available to Builder (native)

In addition to sandbox / db / kv / storage primitives, Builder has these *agent-coordination* tools:

| Tool | Purpose | Surfaces in chat as |
|------|---------|---------------------|
| `ask_survey(survey: Survey)` | Ask the user 0–3 structured questions. See §8.2.7. | `<SurveyCard>` |
| `propose_diff(diff)` | Show a code diff card before applying. | Diff card receipt |
| `request_review(scope)` | Hand off to Reviewer for the CI gate. | Implicit |
| `consult_critic()` | Force a Critic round (rare; usually automatic). | Implicit |
| `file_issue(title, body, severity)` | Open an issue in plan canvas mid-conversation. | Receipt |
| `tag_milestone(title, items[])` | Group features into a milestone. | Receipt |

`ask_survey` is the most-used coordination tool. Its constraints (cap of 3, skip always offered, kinds limited) are enforced as described in §8.2.7.

### 12.2 Skill registry shape

Each skill is a directory under `crates/control/skills/<skill-name>/`:
```
skill.yaml          # metadata, quality budgets, dependencies, tags
SKILL.md            # human/LLM-facing instructions
reference/          # docs the LLM may search
templates/          # boilerplate code snippets
tests/              # fixtures the skill knows how to write
```

Skill loading strategy:
- Builder receives the user prompt + project state.
- Retrieval picks top-K skills by domain + capability match.
- Each skill's SKILL.md is loaded into context.

### 12.3 Skill catalog UI (`/skills` page)

- Two columns: Domain Packs (21) | Capability Skills (18).
- Each tile: name, one-line, "used in N projects" stat.
- Click tile → detail page: full description, examples, sample prompts, what's included.
- Search + filter.

### 12.4 Active skills per project

In **Settings → Project → Active skills** (read-only): which skills are currently in use, when they were added, current version.

---

## 13 · PM agent + plan canvas

(Detailed in §9.8 above.)

PM's responsibilities and triggers:

- **On project state change** (deploy, issue created, milestone hit) → update digest, update progress %.
- **Daily** → generate stand-up: "Yesterday: shipped v0.12. Today: 3 open issues. Blocked: nothing."
- **On `@pm` in chat** → respond with a relevant answer (based on project state).
- **Proactive feature suggestions** (max 3, refreshed weekly): based on missing functionality common in apps of this domain.

PM does not write code. PM creates issues, organizes them, links them to deploys, and recommends what to work on. Builder executes when creator approves.

### 13.1 Issue lifecycle

```
open → in-progress → closed
       (Builder picks up)  (deploy resolves it)
```

Issues link to deploys (`closed_by_deploy_id`), to commits, and to Critic-filed concerns.

### 13.2 Milestone lifecycle

Creator defines milestones manually or accepts PM-suggested ones. Each milestone has a target date (optional), a list of features + issues, and progress %. Auto-completed when all linked items are closed.

---

## 14 · SRE agent + health canvas

(Detailed in §9.9 above.)

### 14.1 Continuous monitoring

- **Uptime**: probe `/` every 60 s from gateway. Failure = file incident after 2 consecutive failures.
- **Latency**: `request` log entries → percentile aggregation per route, every 60 s.
- **Errors**: 5xx and 4xx counters per route per minute.
- **Resource**: CPU / memory per V8 isolate, polled every 30 s.

### 14.2 Anomaly detection

- Error rate spike: > 2× rolling-1-h baseline for 5 minutes.
- Latency regression: p95 > 1.5× rolling baseline for 10 minutes.
- Traffic crash: 3-min average < 0.3× rolling baseline.

On detection: SRE files an incident, creates a linked issue in plan canvas, starts diagnosing.

### 14.3 Diagnose + propose fix

SRE:
1. Pulls recent log entries around the anomaly start.
2. Identifies error fingerprints (stack trace dedup).
3. Hypothesizes a fix.
4. Asks Builder to implement the fix (passes context).
5. Critic + Reviewer pipeline runs as normal.
6. Surface to creator: "SRE found a fix. Review?"

### 14.4 Autonomy levels (V1 default = manual)

- **Manual** (V1 default): creator approves every SRE proposal before deploy.
- **Low-risk auto** (V1.5): SRE auto-deploys fixes that touch only logs/comments/null-checks/typos. Creator notified after.
- **Full auto** (V2): SRE auto-deploys with auto-rollback safety net. Notification only.

---

## 15 · Themes & feature sets

### 15.1 Themes

Theme = (Tailwind config + design tokens + small CSS overrides + curated copy strings). Stored in `crates/control/themes/<name>/`.

Apply flow:
1. Creator clicks "Apply" on a theme card.
2. Builder loads the "theme migration" skill (V1.5+).
3. Builder generates a diff that swaps tokens, rewrites custom CSS, regenerates atelier flourishes for new tokens.
4. Critic + Reviewer pipeline.
5. **Preview** the result (sandboxed deploy at preview-themed-{appName}.zeroship.app).
6. Creator approves → deploy or discard.

V1 themes:
- Refined Atelier (the platform's own, available to apps)
- Minimal mono
- Playful pastel
- Dark-tech
- Editorial newspaper
- Brutalist
- Glass / blur
- Retro 80s

### 15.2 Feature sets

Feature set = a Builder-runnable recipe that adds a coherent feature to a project. Stored in `crates/control/feature-sets/<name>/`.

Apply flow:
1. Creator clicks "Add" on a feature-set card.
2. Builder receives a structured task: "Integrate {feature-set} into this project per its recipe".
3. Recipe is a sequence of code-gen steps with dependencies.
4. If dependencies (e.g., Comments needs Auth) are missing, Builder installs them first with a heads-up line in the receipt.
5. Critic + Reviewer pipeline.
6. Deploy.

V1 feature sets:
- Login + accounts
- Stripe payments + subscriptions
- Comments / reactions
- Search bar with results
- Email signup / newsletter
- File upload + media gallery
- Social share / OG cards
- Real-time updates
- Notifications (in-app + email)
- Multi-language i18n
- Admin panel for the built app
- Analytics + visitor tracking
- SEO / sitemap / structured data
- Calendar / scheduling
- Forms / surveys

Removal: a feature set can be removed; Builder runs the recipe in reverse where possible. Otherwise, Critic warns about residual code.

---

## 16 · Templates

Templates = (skills + theme + feature sets + initial prompt + sample data). Stored similar to themes / feature sets.

Browse `/templates` (public + authed). Detail page shows full skill / theme / feature set composition. "Use this template →" creates a project pre-populated.

Templates are **curated only in V1**. Creator-publishing is V2+.

---

## 17 · Account / profile

`/account` is one page with sections (saved per section):

1. **Profile**: avatar (upload), display name, bio, social links, public portfolio toggle.
2. **Email**: verified email, change email flow (re-verify).
3. **Password**: change password, sign-out-everywhere.
4. **Connected accounts**: Google / GitHub linked status.
5. **Billing** → link to `/account/billing`.
6. **Payouts** → link to `/account/payouts`.
7. **Sign out**.
8. **Delete account** (danger zone, double-confirm).

---

## 18 · Billing — creator pays zeroship

`/account/billing`:

- **Current plan card**: plan name + usage bars (projects, requests, storage). Upgrade / downgrade buttons.
- **Payment methods**: card list, default toggle, add / remove.
- **Invoices**: list of past invoices with download (PDF). Auto-emailed too.
- **Spending limits**: monthly cap + alert thresholds.
- **Cancel subscription**: with retention prompt (free tier fallback).

---

## 19 · Payouts — creator earns from end users

`/account/payouts`:

- **Stripe Connect status**: not connected / pending verification / connected.
- **Connect onboarding**: kicks off Stripe Connect Express flow.
- **Bank account**: managed by Stripe, displayed read-only.
- **Payout schedule**: configurable (weekly / monthly / threshold-based).
- **Earnings dashboard**:
  - Lifetime earned, this month, last month
  - Per-project breakdown
  - 15 % platform fee broken out clearly
  - Stripe fee broken out
  - Net to bank
- **Tax forms**: 1099 (US), W-9 collection. Stripe handles this.

---

## 20 · Sharing / social

V1:

- Share project URL (copy button on workspace top-bar URL pill).
- Auto-generated OG card per built app (screenshot + title).
- Social meta tags injected by Builder by default.

V2:

- Public showcase opt-in.
- Star / follow.
- Activity feed.

---

## 21 · Notifications

- **In-app notification center**: bell icon top right, opens dropdown.
- Categories:
  - Deploy success / failure
  - Auto-rollback
  - SRE incident
  - PM digest
  - Critic blocked deploy
  - Custom-domain DNS verified
  - First earning, payout sent
- **Email notifications**: configurable per category.
- **Mark all read**, individual dismiss.

---

## 22 · Search

V1:

- Global search ⌘K-triggered: projects, files, env vars, issues, recent chats, commands.
- Per-canvas search: file tree, logs, data rows, issues.

V2:

- Saved searches.
- Recent searches surface.

---

## 23 · Admin (real, role-gated)

Strict role check on every page (admin role on user record). Shows in top bar as a tomato `ADMIN` badge.

Pages:

1. **Apps** — searchable list (creator, plan, status, scorecard avg, last deploy, MRR).
2. **App detail** — full creator + project + scorecard history + recent deploys + audit log.
3. **Users** — searchable list (signups, plan, projects, MRR contribution).
4. **User detail** — full user record + impersonate-view.
5. **Revenue** — MRR / ARR / churn / per-plan / platform fee tracking.
6. **System health** — workers, control plane, gateway, DB pool, request rate, error rate, p50/p99 platform-wide.
7. **Audit log** — searchable; filter by actor (user / agent / admin) + action.
8. **Moderation queue** — V2: reported public projects.
9. **Feature flags** — V2: toggle features per user / per cohort.

---

## 24 · Help & support

- **In-app docs**: `/docs` linked from top bar `?` button. Search-first.
- **Contextual help**: small `?` icon on confusing surfaces (custom domain, payouts, secrets) opens a side drawer with relevant docs.
- **Submit feedback**: in-app form, attaches current page + browser context.
- **Status banner**: when zeroship is degraded, persistent banner top of page with link to status page.

---

## 25 · Cross-platform UX (web · tablet · mobile)

zeroship-builder runs on three form factors. Each gets its own layout (not a shrunken desktop). Plus: every app it *generates* is responsive by default.

### 25.1 Form factors and breakpoints

| Form factor | Breakpoint | Primary input | Default layout |
|-------------|-----------|---------------|----------------|
| **Desktop** | ≥ 1024 px | mouse + keyboard | Two-column shell: canvas + chat rail. Full canvases. |
| **Tablet (landscape)** | 768–1023 px | touch + optional keyboard | Same two-column shell, narrower chat rail (280 px). Long-press = right-click. |
| **Tablet (portrait)** | 600–767 px | touch | Single-column. Chat docks to bottom 50 % of screen, swipe-to-resize. |
| **Mobile (phone)** | < 600 px | touch | Single-column. Chat is the home view; canvas opens via top-bar pill in a full-screen view. |

These thresholds are deliberate: tablet-landscape is desktop-equivalent (most iPads in landscape exceed 1024 px CSS). Tablet-portrait is the awkward middle and gets its own layout.

### 25.2 Mobile-first information architecture

On phone:

```
┌────────────────────────────────────┐
│ ≡  supper-club          ⚙ [+code]  │  topbar slim
├────────────────────────────────────┤
│ [preview] [logs] [plan] [health]…→  │  pill strip, h-scrollable
├────────────────────────────────────┤
│                                    │
│   chat is the home view            │
│                                    │
│   user msg…                        │
│   builder msg…                     │
│                                    │
├────────────────────────────────────┤
│  [composer]                        │
│  ↵ to send · 📎  🖼  ⏹             │
└────────────────────────────────────┘
```

- **Chat = home**. Open the app on a phone, you land on chat. Canvases are accessed via the pill strip (h-scroll) which slides up a full-screen canvas view.
- **Canvas full-screen overlay** with a back button returns to chat. The pill that's active turns tomato.
- **Preview canvas on phone** = the iframe at full bleed. Device-frame switcher in the plate bar still works (you can preview your app's mobile vs desktop appearance from your phone).
- **Files canvas on phone** = tree-only by default (file list with sizes). Tap a file → viewer (read-only). Editing prompts: "Editing files from a phone is rough — want to ask Builder to change this instead?" with chat composer prefilled with file context. Real edit on phone is allowed but warned.
- **Data canvas on phone** = list view per row tap → detail sheet. No spreadsheet-grid; a rendered card per row.
- **Plan canvas on phone** = sub-tabs become a top dropdown; lists are full-width.
- **Health canvas on phone** = scorecard at top, status second, recent incidents below. Charts stack vertically.

### 25.3 Tablet (portrait) layout

Distinct from both desktop and phone:

```
┌────────────────────────────────────┐
│ topbar with full pills + tier      │
├────────────────────────────────────┤
│                                    │
│        canvas (top 50%)            │
│                                    │
├──── drag handle ───────────────────┤
│                                    │
│        chat (bottom 50%)           │
│                                    │
└────────────────────────────────────┘
```

- Canvas + chat split vertically, drag handle to resize.
- Both visible simultaneously — you can write code, see preview update, talk to Builder.
- Canvas pills row stays at the top.
- Chat composer pinned to the bottom of the chat half.

### 25.4 Touch interactions

Standard mobile gestures only — nothing exotic to learn:

- **Long-press** = context menu (right-click equivalent in files / data / chat receipts)
- **Pinch-zoom** in preview canvas (zooms the iframe)
- **Pull-to-refresh** on logs canvas
- **Tap-and-hold-to-drag** for the canvas/chat divider on tablet portrait

Power-user swipe gestures (swipe-regenerate, etc.) deferred — only add if usage data shows users want them.

### 25.5 Performance on mobile

Mobile networks and slower CPUs raise the bar:

- **Initial bundle ≤ 200 KB gzipped** for the auth + workspace shell. Code-split everything else.
- **Monaco lazy-loaded**: 1.2 MB compressed; only loaded when the files canvas is opened on +Code tier. Mobile shows "open file in Builder" alternative until Monaco is loaded.
- **Charts in health/plan canvases**: lazy-loaded chart library (`@unovis/charts` or similar, ~40 KB) loaded only when those canvases mount.
- **Image compression**: project icon and avatar uploads auto-resize to ≤ 512 px before upload.
- **Preview iframe deferred**: doesn't load until preview canvas is in viewport.

### 25.6 Apps built on zeroship — responsive by default

Every app Builder ships is responsive by default. Critic enforces.

#### Builder defaults (skill behavior)

- All generated layouts use **mobile-first Tailwind classes** (`flex-col md:flex-row`, etc.).
- Default project template includes a `<viewport>` meta tag, touch icons, and an `<picture>` element pattern for hero images.
- Forms, tables, modals all generate mobile-friendly variants (e.g. tables become cards under 600 px).
- Skills declare their target form factors (`form_factors: [mobile, tablet, desktop]`); a skill that doesn't ship a mobile pattern fails Critic's responsive-design dimension.

#### Critic — responsive-design dimension

Adds to the GG.1 quality dimensions list (features.md, dim 418):

- **Responsive design**: every page renders without horizontal scroll at 375 / 768 / 1280 px. No fixed widths > 100 vw. Touch targets ≥ 44 × 44 px. Forms work on mobile keyboards (`type="email"` etc.). Critic runs Playwright in three viewport sizes during the loop. **Hard-blocked horizontal scroll at 375 px** is escalated to a deploy gate.

#### Preview canvas — device-frame testing

Already in §9.1: device frame switcher (desktop / tablet / phone). Now mandatory in the Critic post-deploy verification: a screenshot per device frame. Visual regression detection across all three.

### 25.7 Built-in templates declare form-factor support

Each template's metadata declares `tested_on: [desktop, tablet, mobile]`. V1 templates all ship with all three tested.

### 25.8 Implementation phasing

- **V1.0**: responsive web — desktop + tablet-landscape + tablet-portrait + phone layouts. Builder generates mobile-first apps. Critic responsive-design dimension live with the 375-px hard gate.
- **V1.5**: refinement based on usage data — touch-gesture polish, mobile bundle-size cuts, per-canvas mobile UX iterations.

**Out of scope**: PWA installation, push notifications, native mobile apps, offline mode. Cross-platform here means *responsive web*. Revisit installable / native if real demand surfaces post-launch.

---

## 26 · Empty / loading / error states (catalog)

Standardized, reusable patterns:

| Surface | Empty | Loading | Error |
|---------|-------|---------|-------|
| Project gallery | "No projects yet — start one above." with hero CTA | Skeleton cards | Banner + retry |
| Files canvas | "No files yet — describe an app in chat." | Tree skeleton | "Couldn't load files. {Retry}" |
| Logs canvas | "No events yet. Once your app runs, they appear here." | Stream connecting indicator | "Lost log connection. {Reconnect}" |
| Data canvas | "No tables yet. Ask the agent to add one." | Row skeletons | inline error + retry |
| Plan / Issues | "No issues yet. Press + to file one." | Skeleton rows | "Couldn't load issues. {Retry}" |
| Health | "Building first deploy — health checks start in a moment." | "Checking…" | "Couldn't reach app. {Retry}" |
| Chat | "Tell {projectName} what to make." with 3 example prompts | typing cursor | inline error block + retry button |
| Preview | "Nothing's been built yet." | progress bar | "Preview failed. View logs →" |

Every error state ships with a *retry* affordance. Every empty state ships with a *next action*.

---

## 27 · Voice & copy guide

### 27.1 Tone

- **Direct, helpful, slightly literary, never cute.**
- Active voice. Short sentences. Avoid jargon.
- "I" / "we" used sparingly; default to imperative ("describe what to make").
- Italics: emphasis only, never decoration.
- Emojis: never in operational copy; only ✓/✕/… as glyphs in receipts.

### 27.2 Replacements (de-atelier)

| Old | New |
|-----|-----|
| "the studio" (when meaning the agent) | "Builder" or "the agent" |
| "the studio" (when meaning the platform) | "zeroship" |
| "the studio is working…" | "thinking…" |
| "manuscript" | "files" |
| "ledger" | "logs" |
| "key cabinet" | "env" / "secrets" |
| "letterhead" | (just card / project) |
| "marginalia" | (kept only as a marketing flourish, never operational) |
| "if you're curious:" | (removed; pills replace) |
| "Studio open Mon–Fri 09–18" | (deleted entirely) |
| "Vol. I · Issue 04" | (kept as marketing-only flourish on landing) |
| "est. Twenty-twenty-six" | (kept as marketing-only stamp) |

### 27.3 Error language

Use exact templates:
- "Email or password is incorrect."
- "Network problem. Try again, or check the status page."
- "Couldn't reach {service}. {Retry}"
- "Not allowed. (You're signed in as {email}.)"

Never:
- "Oops!"
- "Something went wrong."
- "Error: {raw exception}"

### 27.4 Action verbs

| Action | Button |
|--------|--------|
| Create project | "Begin" (atelier surfaces) / "Create project" (operational) |
| Save edit | "Save" |
| Confirm destructive | "Delete" / "Archive" / "Cancel subscription" |
| Cancel modal | "Cancel" or "Never mind" |
| Apply theme | "Preview" → then "Apply" |

---

## 28 · Telemetry / analytics events

For product-iteration insight, every meaningful action emits an event:

- `auth.signup`, `auth.login`, `auth.oauth_start`, `auth.oauth_callback`
- `project.created`, `project.deleted`, `project.archived`
- `chat.turn_sent` (with `tier`, `image_count`, `file_count`, `tokens_in`)
- `chat.turn_complete` (with `critic_iterations`, `tokens_out`, `latency_ms`)
- `deploy.started`, `deploy.complete`, `deploy.failed`, `deploy.rolled_back`
- `quality.scorecard_computed` (with per-dim scores)
- `sre.incident_filed`, `sre.fix_proposed`, `sre.fix_approved`
- `pm.feature_suggested`, `pm.feature_promoted`, `pm.feature_dismissed`
- `template.applied`, `theme.applied`, `feature_set.applied`
- `tier.upgraded`, `tier.downgraded`, `payment_method.added`
- `payout.connect_started`, `payout.connect_complete`, `payout.sent`

Events are stored in the audit log (with attribution) AND emitted to a metrics pipeline for aggregate analytics.

---

## 29 · Data model touchpoints

(High level — full schemas in implementation plan.)

```
users               id, email, name, avatar, role, created_at …
projects            id, owner_id, slug, name, tagline, visibility, plan, prod_branch_id, created_at …
branches            id, project_id, name, parent_branch_id, kind (long_lived | preview | snapshot_restore),
                    schema_namespace (postgres schema name), ttl_at?, created_by, created_at, last_activity_at
deployments         id, project_id, branch_id, hash, status, started_at, finished_at, scorecard_summary
migrations          id, project_id, branch_id, applied_to_deploy_id?, sql_up, sql_down, status, author,
                    safety_report_jsonb, applied_at?, rolled_back_at?
snapshots           id, project_id, branch_id, kind (auto_daily | auto_hourly | manual), name?, size_bytes,
                    created_at, retain_until
saved_views         id, project_id, table_name, name, filter_jsonb, sort_jsonb, columns_jsonb, owner_id
quality_scores      deploy_id, dim, score, weight, details
issues              id, project_id, branch_id?, title, body, severity, status, source, …
features            id, project_id, title, body, status, milestone_id, …
milestones          id, project_id, title, target_date, …
incidents           id, project_id, branch_id, started_at, mitigated_at, resolved_at, root_cause, …
audit_log           id, project_id?, actor, action, target, branch_id?, payload, created_at
chat_sessions       id, project_id, branch_id?, started_at, …
chat_messages       id, session_id, role, content, tools_jsonb, created_at
skills_active       project_id, skill_name, version, added_at
themes_active       project_id, theme_name, version, applied_at
feature_sets_active project_id, feature_set_name, version, applied_at
env_vars            id, project_id, branch_id?, key, value (encrypted for secrets), updated_at
                    -- branch_id=NULL means project-level; branch overlay wins on resolve
notifications       id, user_id, type, payload, read_at, created_at
```

Notes:
- `branches.schema_namespace` is the actual Postgres schema name (e.g. `app_<id>__<branch>`). The `compio-postgres` plugin resolves all per-app queries against this schema for the active branch.
- `migrations` is per-branch; merging branches re-applies migrations on the target.
- `env_vars.branch_id NULL` = project-level (default); non-null = branch-specific override.
- `quality_scores.deploy_id` ties scorecard to a specific branch via `deployments.branch_id`.

---

## 30 · Implementation phasing

Map of priority → release vehicle → spec sections:

### Release 0 — Critical Path (V1.0)

Goal: a working zero-to-ship loop better than Lovable / v0 / Bolt for our target persona.

- All [0] features
- Subset of [1] needed to make [0] viable:
  - Custom domain (1 in V1 per project)
  - Real plan canvas + scorecard (so the AI PM and Critic story is honest)
  - Real logs (with timestamps, not "—")
  - Mobile-responsive baseline
  - WCAG-AA pass on critical surfaces
  - **Two-branch model: `prod` and `dev` (no preview branches yet, no merge UI)**
  - **Migration as first-class object** (so Builder/Critic can enforce safety from day 1)
  - **Migration safety hard-gate on `prod`**
- Surface skeleton for everything in [1] (placeholder copy, "coming soon" patterns where unfilled)

Rough scope:
- Auth: full sign-up / login / OAuth / verify / forgot-password
- Workspace shell + Maker tier surfaces (preview / logs / plan / health / settings)
- Chat surface complete (composer, streaming, receipts, regenerate, edit, slash, @, image input)
- Builder + Critic + Reviewer agents
- 3 themes (Atelier / Minimal mono / Dark-tech), 5 feature sets (Login, Stripe, Comments, Search, Email)
- 6 domain templates (todo, blog, landing, e-commerce, CRM, dashboard)
- Marketing: landing + pricing + skills + templates pages
- Account / billing entry (no payouts yet)
- 100 % of pre-deploy hard gates; soft gates baseline
- **Data canvas: tables + schema + migrations sub-tabs; per-branch reads via `compio-postgres` schema namespacing**
- **Branch switcher in Data canvas (limited to `prod` / `dev` in V1)**
- **Cross-platform (responsive web): distinct layouts for desktop / tablet-landscape / tablet-portrait / phone; touch gestures (long-press = context menu, pull-to-refresh, pinch-zoom in preview); bundle budget ≤ 200 KB initial; lazy-load Monaco**
- **Builder default: mobile-first generation; Critic responsive-design dimension live; horizontal-scroll-at-375px hard gate**

### Release 1 — Production-grade (V1.1 → V1.5)

- Remaining [1] features
- +Data tier (data + media canvases — full)
- +Code tier (files + env canvases — full)
- PM agent fully proactive
- SRE agent low-risk-auto
- Notifications center
- 2FA, GDPR export
- Stripe Connect payouts (full)
- **Branching: arbitrary named branches; preview branches auto-created on PR-style deploys; per-branch env overlays**
- **Backups + restore** (point-in-time, restore-to-new-branch only)
- **Bulk row operations + saved views**
- **Schema diagram view + index inspector**

### Release 2 — Trust & Growth (V2.0)

- All [2] features
- Public showcase
- Templates marketplace (creator-publishing curated)
- Multi-domain
- Continuous evaluation of zeroship itself (GG.8)
- SRE full autonomy mode
- **CoW Postgres branching backend** (Neon-equivalent) — true at-rest data CoW; cheap forks of large prod data
- **Schema + (small-table) data merge UI**
- **Branch protection rules** on `prod`

### Release 3+ — Network / Collaboration / Polish

- [3] network features (public profiles, follows, stars)
- [4] collaboration (teams, comments, roles)
- [5] dark mode, i18n, voice input

---

## 31 · Open issues / decisions deferred

These don't block V1 design lock but should be tracked:

1. **End-user auth UX hybrid surface** — needs its own mini-spec when the auth feature-set is implemented (does the creator-facing config UI live in Settings, or in the Auth feature-set's detail page?).
2. **SRE telemetry pipeline** — control-plane Postgres + materialized views for V1; revisit if we exceed ~10k events/min.
3. **Skill update propagation UX** — opt-in, but the prompt design and the migration-skill behavior need a follow-on spec.
4. **Theme migration skill** — must be designed and implemented before V1.5 ships theme switching for existing apps.
5. **Cost meter UI** — needs design when token costs become user-facing (should it whisper or shout?).
6. **Click-to-edit overlay** — V1.5; needs an overlay-injection contract with the iframe.
7. **`compio-postgres` branch-namespace support** — V1 needs schema-namespaced queries with per-app + per-branch resolution. Implementation touches the runtime DB plugin (`zeroship.db.*`) — concrete spec lives there, not here.
8. **Data branching strategy (V1 logical vs V2 CoW)** — V1 uses separate Postgres schemas inside the per-app DB; V2 swaps to a CoW backend. The migration path between V1 and V2 needs a follow-on spec when V2 is on the horizon.
9. **Cross-branch chat history** — does a chat session belong to a branch (so dev-mode experiments don't pollute the prod-Builder context) or to a project? V1 default: project-level (single thread); V1.5+ adds optional per-branch threads.
10. **Migration "down" SQL** — Builder must always write a reversible migration where possible. Some changes are inherently non-reversible (DROP TABLE with data). Critic should flag and require explicit "no rollback possible" acknowledgement from creator.

---

## Appendix A · Postgres branching implementation

This appendix is the implementation contract for the branching feature defined in §11.5. It exists at the design-doc level (not the implementation plan) because branching cuts across multiple crates (`compio-postgres`, `control`, `gateway`, `worker`) and the platform-vs-builder boundary needs to be designed coherently.

### A.1 Strategy comparison

Five candidate strategies were evaluated. Summary:

| Strategy | V1? | Cost of fork | Schema isolation | Data isolation | Ops complexity | PG vendor lock |
|----------|-----|--------------|------------------|----------------|----------------|----------------|
| **1. Schema namespacing** | ✅ ship | O(DDL) free / O(data) optional | strong | strong | low | none |
| 2. Database-per-branch | ✗ | O(N) | strong | strong | high (pool fragmentation) | none |
| 3. Logical replication | ✗ | high | strong | with lag | high | none |
| **4a. Neon (CoW)** | V2 target | O(1) | strong | true CoW | medium-high | open source |
| 4b. ZFS / Btrfs snapshots | ✗ | O(1) | strong | true CoW | high (per-branch PG) | hardware |
| 4c. Aurora cloning | ✗ | O(1) | strong | true CoW | low (managed) | AWS |
| 5. Application-layer (`_branch` col) | ✗ | O(rows) | none | logical only | low | none |

**V1 chosen: Strategy 1 (schema namespacing).** Ships fast; vanilla PG works (RDS / Aurora / Cloud SQL / our own); the per-app schema pattern is already in zeroship; cost-of-data-fork is acceptable while typical apps are < 100 MB.

**V2 target: Strategy 4a (Neon).** Adopts true CoW once branch counts and storage costs exceed Strategy 1's tolerable range.

### A.2 V1 architecture: schema namespacing

#### A.2.1 Naming convention

```
app_<app_id>__<branch_name>
```

- `<app_id>`: the typed-id base62 form, lowercase, with the `app_` prefix preserved (Postgres-safe identifier).
- `<branch_name>`: lowercase ASCII `[a-z0-9_-]{1,32}`, with `-` rewritten to `_` for the schema name (PG identifier rules).
- Reserved branch names: `prod`, `dev`. Auto-generated previews: `preview_<deploy_hash[:8]>`.

Examples:
```
app_01h3m9q2t__prod
app_01h3m9q2t__dev
app_01h3m9q2t__preview_a1b2c3d4
```

#### A.2.2 Per-request resolution flow

```
HTTP request
    │
    ▼
Gateway parses host:
  "supper-club.zeroship.app"            → project=supper-club  branch=prod (default)
  "dev--supper-club.zeroship.app"       → project=supper-club  branch=dev
  "preview-a1b2c3--supper-club..."      → project=supper-club  branch=preview-a1b2c3
    │
    ▼
Gateway looks up project → resolves to (app_id, branch_name)
Gateway adds headers:  X-ZS-App: app_01h3m9q2t   X-ZS-Branch: dev
    │
    ▼
Worker boots/reuses V8 isolate
Worker constructs BranchContext { app_id, branch_name } from headers
    │
    ▼
JS code calls zeroship.db.find('contacts', {…})
    │
    ▼
plugin-db op acquires conn from pool, sets BranchContext
    │
    ▼
compio-postgres on connection acquire (if branch differs from cached):
  SET search_path TO app_01h3m9q2t__dev, public;
    │
    ▼
Query executes against the dev schema
```

#### A.2.3 `compio-postgres` extensions

New module `crates/compio-postgres/src/branch.rs`:

```rust
/// Identifies which branch's schema queries on a given connection are bound to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BranchContext {
    pub app_id: AppId,
    pub branch: String,         // already validated; ASCII safe
}

impl BranchContext {
    pub fn schema(&self) -> String {
        format!("app_{}__{}", self.app_id.as_str(), self.branch)
    }

    pub fn search_path_sql(&self) -> String {
        format!("SET search_path TO \"{}\", public", self.schema())
    }
}
```

Connection wrapper learns `current_branch_ctx: Option<BranchContext>`:

```rust
impl Connection {
    pub async fn ensure_branch(&mut self, ctx: &BranchContext) -> Result<()> {
        if self.current_branch_ctx.as_ref() == Some(ctx) { return Ok(()) }
        self.simple_query(&ctx.search_path_sql()).await?;
        self.current_branch_ctx = Some(ctx.clone());
        Ok(())
    }
}
```

Pool acquisition: callers pass a `BranchContext`; pool prefers connections already bound to that context (cheap reuse), falling back to setting the `search_path` on a fresh connection.

```rust
pub async fn acquire_for(&self, ctx: &BranchContext) -> Result<PoolConn> {
    let mut conn = self.acquire_with_affinity(ctx).await?;
    conn.ensure_branch(ctx).await?;
    Ok(conn)
}
```

Affinity hint reduces redundant `SET search_path` calls for hot branches.

#### A.2.4 Schema creation on branch fork

`crates/control/src/branch.rs`:

```rust
pub async fn create_branch(
    project_id: ProjectId,
    name: &str,
    parent: &str,
    copy_data: bool,
    actor: Actor,
) -> Result<Branch, BranchError> {
    validate_branch_name(name)?;
    let project = projects::get(project_id).await?;
    let parent_schema = format!("app_{}__{}", project_id, parent);
    let new_schema    = format!("app_{}__{}", project_id, name);

    pg::transaction(async |tx| {
        // 1. New schema
        tx.exec(&format!("CREATE SCHEMA {} AUTHORIZATION {}",
            quote_ident(&new_schema), project.pg_role)).await?;

        // 2. Copy DDL
        let ddl = extract_ddl(&parent_schema).await?;
        let rewritten = rewrite_schema_refs(&ddl, &parent_schema, &new_schema);
        tx.exec(&rewritten).await?;

        // 3. (Optional) Copy data
        if copy_data {
            for table in list_tables(tx, &parent_schema).await? {
                let q = format!(
                    "INSERT INTO {}.{} SELECT * FROM {}.{}",
                    quote_ident(&new_schema), quote_ident(&table),
                    quote_ident(&parent_schema), quote_ident(&table),
                );
                tx.exec(&q).await?;
            }
        }

        // 4. Record branch
        branches::insert(tx, project_id, name, parent, copy_data, actor).await?;
        audit::log(tx, AuditAction::BranchCreated { project_id, name: name.into(), parent: parent.into() }, actor).await?;
        Ok(())
    }).await
}
```

**`extract_ddl`** options:

- **V1**: shell out to `pg_dump --schema-only --schema=<parent>` via `compio_process::Command`. Reliable, well-tested, captures all object kinds.
- **V1.5+**: native extractor that queries `information_schema.tables` + `pg_indexes` + `pg_constraint` + `pg_trigger` + `pg_proc` and reconstructs DDL. No subprocess dependency, controllable.

**`rewrite_schema_refs`** is a literal-string replacement of the parent schema name with the new one in the dumped DDL. Edge cases: schema-qualified types in function bodies (`RETURNS app_X__prod.foo`) — handled by the same replacement since pg_dump emits qualified names verbatim.

#### A.2.5 Migrations

Migrations are stored in the control plane DB:

```sql
CREATE TABLE migrations (
    id            UUID PRIMARY KEY,
    project_id    UUID NOT NULL REFERENCES projects(id),
    branch_id     UUID NOT NULL REFERENCES branches(id),
    sql_up        TEXT NOT NULL,
    sql_down      TEXT,                         -- NULL allowed for irreversible migrations
    status        migration_status NOT NULL,    -- pending | applied | failed | rolled_back
    applied_at    TIMESTAMPTZ,
    rolled_back_at TIMESTAMPTZ,
    safety_report JSONB,                        -- Critic + linter output
    author_kind   TEXT NOT NULL,                -- 'human' | 'builder'
    author_id     UUID,
    deploy_id     UUID REFERENCES deployments(id), -- which deploy applied this
    created_at    TIMESTAMPTZ DEFAULT now()
);

CREATE INDEX migrations_branch_status ON migrations(branch_id, status);
```

Applying a migration:

```rust
pub async fn apply_migration(
    migration_id: MigrationId,
    actor: Actor,
) -> Result<(), MigrationError> {
    let m = migrations::get(migration_id).await?;
    let branch = branches::get(m.branch_id).await?;
    let schema = format!("app_{}__{}", branch.project_id, branch.name);

    // Critic safety check (idempotent — may run twice if creator re-applies)
    let report = critic::check_migration_safety(&m.sql_up, &branch).await?;
    if report.has_blocking_issues() && !actor.has_destructive_opt_in() {
        return Err(MigrationError::BlockedByCritic(report));
    }

    pg::transaction(async |tx| {
        tx.exec(&format!("SET search_path TO \"{}\", public", schema)).await?;
        tx.exec(&m.sql_up).await?;
        migrations::mark_applied(tx, migration_id, actor).await?;
        audit::log(tx, AuditAction::MigrationApplied { migration_id, branch_id: branch.id }, actor).await?;
        Ok(())
    }).await
}
```

Rollback:
- Requires `sql_down IS NOT NULL` on the migration row.
- Runs `sql_down` in a transaction with the same search_path.
- Marks migration `rolled_back`; later re-apply creates a new migration row.

#### A.2.6 Branch promotion (preview → prod)

Promotion = merge migrations from source to target + flip gateway pointer.

```rust
pub async fn promote_to_prod(
    source_branch: BranchId,
    actor: Actor,
) -> Result<DeploymentId, PromoteError> {
    let project = projects::get_for_branch(source_branch).await?;
    let prod = branches::get(project.prod_branch_id).await?;
    let unapplied = migrations::list_unapplied_on(source_branch, prod.id).await?;

    // Reviewer + Critic safety pass on the merge
    let safety = reviewer::check_promotion(source_branch, prod.id, &unapplied).await?;
    if safety.has_hard_blocks() { return Err(PromoteError::Blocked(safety)); }

    let new_deploy = pg::transaction(async |tx| {
        // Apply all migrations to prod schema, in order
        let prod_schema = format!("app_{}__prod", project.id);
        tx.exec(&format!("SET search_path TO \"{}\", public", prod_schema)).await?;
        for m in &unapplied {
            tx.exec(&m.sql_up).await?;
            migrations::mark_applied(tx, m.id, actor).await?;
        }

        // Promote the source branch's most-recent build to prod
        let source_deploy = deployments::latest_on_branch(tx, source_branch).await?;
        let new = deployments::create_promotion(
            tx, source_deploy.id, project.id, prod.id, actor
        ).await?;
        deployments::set_prod_pointer(tx, project.id, new.id).await?;

        audit::log(tx, AuditAction::BranchPromoted {
            source: source_branch, target: prod.id, deploy_id: new.id
        }, actor).await?;
        Ok(new.id)
    }).await?;

    // Atomic: gateway picks up the new prod pointer on next route refresh
    gateway::invalidate_routes(project.id).await?;
    Ok(new_deploy)
}
```

#### A.2.7 Gateway routing

Gateway parses the host header. Pseudo-Rust:

```rust
fn parse_host(host: &str, base_domain: &str) -> Option<RouteTarget> {
    // base_domain: "zeroship.app"
    let prefix = host.strip_suffix(&format!(".{}", base_domain))?;
    // "supper-club" or "dev--supper-club" or "preview-a1b2c3--supper-club"

    if let Some((branch, slug)) = prefix.split_once("--") {
        Some(RouteTarget { slug: slug.to_string(), branch: branch.to_string() })
    } else {
        Some(RouteTarget { slug: prefix.to_string(), branch: "prod".into() })
    }
}
```

The route registry maps `(slug, branch)` → most-recent deploy hash for that branch. Forwards to a worker with `X-ZS-App` and `X-ZS-Branch` headers.

#### A.2.8 Env-var resolution

Workers receive env vars on isolate boot. Resolution:

```rust
pub async fn resolve_env(project_id: ProjectId, branch_id: BranchId) -> EnvMap {
    let project_level = env_vars::list_where(project_id, None).await?;
    let branch_level  = env_vars::list_where(project_id, Some(branch_id)).await?;
    
    let mut out = EnvMap::default();
    for v in project_level { out.insert(v.key, v.resolved_value()); }
    for v in branch_level  { out.insert(v.key, v.resolved_value()); }  // branch wins
    out
}
```

Secrets are decrypted at the control plane using the project's KMS key before being shipped to workers.

#### A.2.9 Backups + restore

V1 backup:

- Daily `pg_basebackup` to per-app object-store bucket, full cluster snapshot (one cluster, not per-app).
- WAL archiving enabled (continuous archive to object store).
- Retention per plan (24 h free / 7 d Pro / 30 d Enterprise).

Restore (always to new branch, never overwrites active branch):

```rust
pub async fn restore_to_new_branch(
    project_id: ProjectId,
    snapshot: SnapshotId,
    new_branch_name: &str,
) -> Result<Branch, RestoreError> {
    // 1. Spin up an ephemeral PG instance from the snapshot via pg_basebackup restore + WAL replay to target time.
    // 2. pg_dump the project's schema(s) from the ephemeral instance.
    // 3. Apply that dump to a new schema in the live cluster: app_<id>__<new_branch_name>.
    // 4. Tear down ephemeral instance.
    // 5. Insert branch row (kind = snapshot_restore).
    // 6. Notify creator: "Restore complete on branch <name>. Promote when ready."
}
```

This is implemented as a **background job** (the restore can take minutes for big DBs).

### A.3 Capacity and sharding

PG starts to struggle around **10–20k schemas per database** due to:
- `pg_namespace` linear scans during catalog operations.
- Lock-table sizing (`max_locks_per_transaction`).
- Statistics-gathering (`autovacuum`) bottlenecks.

**Strategy**: shard apps across PG clusters by `app_id` range. Each cluster targets ≤ 5k apps × 5 branches average = 25k schemas.

Routing:

- Control plane stores `cluster_id` per app.
- `compio-postgres` connection pool keyed by `(cluster_id)` not by app.
- Migrations routing also goes through cluster lookup.

New cluster provisioning is operator action (Terraform / Ansible). Apps onboard to the next-with-capacity cluster.

### A.4 V2 migration to Neon

When V1 strategy hits its capacity wall (or a tail of large-data apps blow up the storage bill), migrate to Neon.

Migration plan (per app, lazy):

1. Create a Neon project for the app (one project per app on Neon's side).
2. For each existing branch (Postgres schema), create a Neon timeline.
3. `pg_dump` the schema; `pg_restore` into the corresponding Neon timeline's DB.
4. Update control plane to point this app to Neon (URL of the Neon endpoint).
5. `compio-postgres` driver detects Neon target via project metadata, drops the `SET search_path` step, talks to Neon directly. (Neon branches are real DBs; no schema namespacing needed.)
6. Verify (smoke tests against the new endpoint), then atomically flip routes.
7. Delete the old schemas in the V1 cluster.

This lets us migrate per app, not big-bang, and keeps V1 and V2 apps coexisting during the transition.

The `compio-postgres` connection layer needs:
- A `BackendKind` enum: `SchemaNamespaced { cluster_id }` | `Neon { project, branch }`.
- A unified `acquire_for(BranchContext)` that dispatches.

Driving `Branch → BackendKind` lives in the control plane; workers receive the right connection target as part of their env.

### A.5 Operational considerations

#### Monitoring

SRE-relevant metrics for V1 schema-namespacing:

- Schema count per cluster (alert at 80 % of soft limit).
- `pg_stat_user_tables.n_live_tup` per schema (track largest tables for fork-cost estimation).
- `pg_stat_database.xact_rollback` rates (catch frequent failed migrations).
- Connection-pool hit-rate by `BranchContext` (cache effectiveness).
- Migration apply duration p50/p95/p99 per branch.

#### Failure modes

| Failure | Detection | Mitigation |
|---------|-----------|------------|
| Schema creation fails mid-fork | tx rollback | record `branches.status = 'failed'`; surface in plan canvas as error |
| Migration apply fails on prod | tx rollback | migration marked `failed`; deploy held; SRE files incident |
| Cluster reaches schema limit | metric alert | provision new cluster; new apps go to new cluster; existing apps migrate at off-hours |
| `pg_dump` fork is too slow on big data | timeout (5 min default) | offer schema-only fork; surface "data is large; this branch will start empty unless you wait" prompt |

#### Cost model

Schema-namespacing is essentially free per branch *for schema*; costs scale linearly with data forked. Storage cost per branch:

- Schema-only fork: ~50 KB per branch (DDL + pg_catalog rows).
- Full data fork: O(parent's data size). For a 100 MB app, every fork is +100 MB.

This makes free-tier sensible at 2 branches (prod + dev), Pro at 10 long-lived + unlimited preview (TTL bounded), Enterprise unlimited.

### A.6 Branch-aware testing

Implementation testing requires:

- Integration test: create app → create dev branch → verify isolation (write to dev doesn't appear in prod, schema diff returns expected diff).
- Migration safety test: Builder writes a destructive migration → Critic blocks → creator opt-in unblocks → applies cleanly.
- Promotion test: dev branch → promote → prod URL serves the new build, prod schema reflects new migrations, old prod migration history preserved in audit log.
- Cluster failover: simulate cluster restart, verify all branch schemas remain accessible after compio-postgres pool reconnect.

`cargo test -p compio-postgres -- --test-threads=1` (per CLAUDE.md convention) gates the branching driver work.

### A.7 Implementation sequencing

Order the V1 work so each step is shippable in isolation:

1. **`compio-postgres` `BranchContext` plumbing** — affinity-aware connection pool; `SET search_path` per acquire. Tests with hand-crafted schemas. *No user-visible change yet.*
2. **`crates/control` branch CRUD** — `branches` table, `create_branch` / `list_branches` / `delete_branch` ops. Initially admin-only API.
3. **Gateway host parsing** — `dev--{slug}` resolution; new tests in `gateway/dispatch.rs`.
4. **Migrations as first-class** — `migrations` table, `apply_migration` / `rollback_migration` ops; Critic dimension hooked into Builder's commit flow.
5. **Worker plumbing** — `BranchContext` from headers, threaded through `zeroship.db.*` ops.
6. **Builder UX** — Data canvas branch switcher; Migrations sub-tab; "fork dev from prod" UI.
7. **Env-var overlay** — `env_vars.branch_id` column; resolution in worker boot.
8. **Backups** — `pg_basebackup` job + WAL archive + restore-to-new-branch flow.
9. **Promotion flow** — `promote_to_prod` op; LiveBanner change to mention "Promoted from dev".
10. **Cluster sharding readiness** — `projects.cluster_id`; multi-cluster pool registry. Don't actually spin up cluster #2 until needed.

Each step has a clear test surface and is shippable behind a feature flag. Critic dimension and migration safety hard-gate land with step 4; Data canvas branching UI in step 6.

---

## Sign-off checklist (for spec review)

- [ ] All foundation decisions captured
- [ ] Multi-agent fleet documented with roles + lifecycles
- [ ] Information architecture covers every authed and unauthed surface
- [ ] Design system has color tokens, type, motion, components
- [ ] Each canvas (preview / files / data / media / logs / env / settings / plan / health) has a layout and behavior
- [ ] Chat surface specifies composer, streaming, stop, receipts, diff cards, regenerate, history
- [ ] Quality control system covers Critic loop, gates, scorecard, post-deploy verification, auto-rollback, skill budgets
- [ ] PM, SRE, Builder, Critic, Reviewer agents each have a behavior spec
- [ ] Themes, feature sets, templates have clear apply / preview / remove flows
- [ ] Account, billing, payouts have surface + flow
- [ ] Mobile strategy is honest (workspace = desktop-primary)
- [ ] Empty / loading / error states catalog exists
- [ ] Voice & copy guide replaces atelier-overcooked phrases
- [ ] Telemetry events listed
- [ ] Data model sketched
- [ ] Implementation phased to priority tiers

If anything below is missing or unclear, flag it.
