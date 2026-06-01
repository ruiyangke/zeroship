# zeroship-builder → @zeroship/ui crystal migration (+ control-surface removal)

**Status:** approved (interactive, 2026-05-31). Branch `builder/ui-crystal-migration`
(worktree `.worktrees/builder-ui-crystal`). Commit-only, **never push**.

Sibling of the approved [`2026-05-31-console-pure-creator-app-design.md`] — this
spec finishes the *control-surface removal* that doc mandates (its locked
choice #1) and re-skins the surviving console onto the design system.

## Decision (locked forks)

1. **Full crystal adoption.** The builder becomes a pure `@zeroship/ui` consumer
   on the `crystal` theme. The editorial "paper & ink / tomato / serif / stamp /
   marginalia" identity is **retired** — replaced, not themed.
2. **Drop Tailwind entirely.** Custom (non-component) markup is rebuilt from DS
   layout primitives (`Stack`/`Grid`/`Cluster`/`Container`/`Split`/`Center`/
   `AppShell`) + component-scoped CSS reading `--zs-*` crystal tokens — exactly how
   the DS itself is authored. No parallel token system.
3. **Remove the broken scaffolds + all control-related code** (owner directive,
   2026-05-31). The non-compiling WIP and the orphaned control surface are
   **deleted**, not migrated. This aligns the console with the approved
   pure-creator-app direction and yields a **green baseline** before any re-skin.

## Goal / end state

A clean **pure-creator-app console**, rendered entirely with `@zeroship/ui`
crystal: no Tailwind, no hand-rolled primitives, no control plane, no dead
scaffolds. `tsc -b` and `vite build` both green; the `no-control-import` guard
test passes; surviving creator flows (craft → preview → env/logs/files/settings,
chat, auth, account, home/projects) work and look native to the DS.

## Current-state findings (evidence)

The working tree is **mid-migration** across two overlapping directions; faithful
diagnosis (in worktree `.worktrees/builder-ui-crystal`, seeded from main's
working tree):

- **`vite build` already succeeds** (2877 modules, writes `dist/app.zship`).
  The bundler never touches the broken files because **they are unreachable** —
  not imported by any router or entry.
- **`tsc -b` has 57 pre-existing errors in exactly 9 untracked WIP files**
  (snapshot: `apps/zeroship-builder/.migration/tsc-baseline-errors.txt`):
  `workspace/canvases/{DataCanvas,HealthCanvas,MediaCanvas,PlanCanvas}.tsx`,
  `admin/pages/{AdminApps,AdminAppDetail,AdminJournal,Library}.tsx`,
  `server/control-client.ts`. They import an API surface (`getQualityScores`,
  `sreMonitor`, `pmDigest`, `listMedia`, `getApp`, `listApps`, …) that the
  current API never exports.
- **The client is already pure-creator-app.** `client/api.ts` re-exports only
  sandbox/KV functions from `server/{projects,sandbox,agents}.ts`
  (`listProjects`, `getEnv`, `getLogs`, `listSandboxFiles`, `listIssues`, …) and
  carries the comment *"There is no control plane — no deploy, no plan."*
- **The surviving canvases already use the new API:** `EnvCanvas→getEnv/setEnv`,
  `LogsCanvas→getLogs`, `FilesCanvas→listSandboxFiles/readSandboxFile`,
  `SettingsCanvas→projects` (and it already imports `@zeroship/ui`).
- **The only live callers of the old control actions** (`listApps`/`getApp`/
  `getAppLogs`) are the files being removed (`admin/` + `HealthCanvas`). Other
  mentions (`Marketing`, `BriefCard`, `ChatMessages`, `MentionDropdown`,
  `WizardWorkspace`) are **comments**, not calls.
- **`server/apps.ts` is fully orphaned old control code** (`getControlClient()`
  throughout). `src/server.ts` re-exports `projects/sandbox/chat/wizard/agents/
  http/pm-worker/sre-worker` — **never `apps.ts`**. `@zeroship/control` is already
  out of `package.json`; `server/config.ts` has zero control refs;
  `server/no-control-import.test.ts` is a guard meant to pass post-removal.

**Implication:** deletion is clean (orphaned files, no rewiring), and removing the
9 broken files + `apps.ts` takes `tsc -b` **green**. The re-skin then proceeds
against a strict zero-error gate.

## Phase 0 — Prune (gets the baseline GREEN)

**Delete (control surface):**
- `src/server/apps.ts`
- `src/server/control-client.ts`
- `src/server/control-client.test.ts`

**Delete (broken / superseded scaffolds):**
- `src/client/admin/` (entire dir — `AdminShell` + all pages)
- `src/client/workspace/canvases/{DataCanvas,HealthCanvas,MediaCanvas,PlanCanvas}.tsx`
- `e2e/data-media.spec.ts`, `e2e/plan-health.spec.ts` (test the removed canvases)

**Cleanup pass:** remove any dead references to the deleted UI — admin routes in
`App.tsx` (none found, verify), canvas pills / tier entries in
`workspace/CanvasPills.tsx` + `WorkspaceShell.tsx` for `data`/`health`/`media`/
`plan` (none imported; verify no string-keyed registry or dead pill labels),
and any `@zeroship/control` / `control-client` residue.

**Gate:** `tsc -b` → **0 errors**; `vite build` → green; `vitest run`
(incl. `no-control-import.test.ts`) → green. Delete the now-stale
`.migration/tsc-baseline-errors.txt`. Commit: *"chore(builder): remove control
surface + broken scaffolds (pure-creator-app prune)"*.

## Component mapping (surviving UI → @zeroship/ui)

Hand-rolled primitives in `src/client/components/` are **deleted** and replaced:

| Builder today | Fate | DS replacement |
| --- | --- | --- |
| `Button`, `StampButton`, `GhostButton` | delete | `Button` (`primary→filled`, `secondary→gray/tinted`, `ghost→plain`, `destructive→filled`+red intent) |
| `Pill`, `FilterPill` | delete | `ToggleGroup` / `Tabs` / `FilterBar` |
| `Modal` | delete | `Dialog` |
| `Skeleton`, `Spinner`, `Toast` | delete | `Skeleton`, `Spinner`, `Toast` |
| `EmptyState`, `ErrorState`, `LiveBanner` | delete | `EmptyState`, `ErrorState`, `Banner` |
| `Kpi`, `LedgerRow` | delete | `StatCard`, `ListView`/`DataTable` |
| `PageFrame`, `TopBar`(components), `Marginalia`, `PublicNav` | delete (editorial) | `AppShell` / `Container`+`PageHeader` / `NavigationMenu` |
| `Receipt`, `ProjectCard`, `TemplateCard`, `NotebookPrompt` | rebuild app-local | over `Card` + `DescriptionList` |
| `ErrorBoundary` | keep (logic) | restyle fallback with `ErrorState` |
| `DevEventsBadge` | rebuild | `Badge` (finish DS placeholder) |
| **Pages** (Marketing/Pricing/About/Changelog/Skills/Templates/legal) | rebuild | `Hero`/`PricingTable`/`FeatureGrid`/`StatsBand`/`Cta`/`Faq`/`Footer` + `Container`/`PageHeader` |
| **Auth pages** (Login/Signup/ForgotPassword) | rebuild | `AuthForm` + `@zeroship/auth` |
| **Account / OnboardingIntent / WizardWorkspace** | rebuild | `FormSection`/`Stepper`/`Card`/`Input` |
| **Home** (projects gallery) | rebuild | `Grid`+`Card`+`EmptyState`+`FilterBar` |
| **WorkspaceShell** + `TopBar` + `CanvasPills` + chat rail | rebuild | `AppShell` + `Tabs`/`ToggleGroup`/`Toolbar` + `Split`/`Drawer` |
| **Surviving canvases**: `Env`, `Logs`, `Files`, `Settings`, `Preview` | rebuild | `DataTable`/`FormSection`/`Input` (Env), `Card`+log stream (Logs), file tree + CodeMirror (Files), `FormSection`+`AlertDialog` (Settings), iframe (Preview) |
| **Chat** (15 components) | rebuild app-local | over `Card`/`Input`/`Button`/`Avatar`/`Combobox`(mentions)/`Popover` |

> Before referencing any `@zeroship/ui` prop, verify it via the Storybook MCP
> (`get-documentation`) — do not assume props by naming convention (per AGENTS.md).

## Foundation changes

- **`index.css`** — delete the editorial `@theme` block + tokens + utility classes
  (`.stamp-btn`, `.hairline`, `.label-uc`, `.pulse-dot`, `.reveal`, …). Keep only a
  minimal app-scoped stylesheet (resets/app shell) authored against `--zs-*`.
- **`main.tsx`** — keep `ThemeProvider` (crystal) + `import "@zeroship/ui/styles.css"`
  (both already present).
- **`tokens.ts`** — re-point any JS-readable token constants to `--zs-*` values, or
  delete if its only consumers (the removed canvases' canvas/SVG drawing) are gone.
- **`lib/utils.ts` `cn()`** — keep `clsx`, drop `tailwind-merge`.
- **`vite.config`** — remove `@tailwindcss/vite`.
- **`package.json`** — drop `tailwindcss`, `@tailwindcss/vite`, `tailwind-merge`.
  Tailwind removal lands in the **final** phase, after no utility classes remain.

## DS gaps & policy

- **`Badge`** is a DS placeholder (`sdks/ui/src/placeholders.tsx`) → **finish it in
  `@zeroship/ui`** (generic, belongs there) via the `component-slice-flow` skill.
- **App-local** (builder-specific, built over DS primitives + crystal tokens):
  code viewer (CodeMirror wrapper, Files), log stream (Logs), chat composer +
  `@`-mention dropdown (over `Combobox`/`Popover`). Not pushed into the DS.
- The SRE sparkline/chart gap **disappears** with `HealthCanvas`'s removal.

## Phasing & verification gates

Tailwind stays installed-but-idle until the last phase, so the app stays green
throughout. After Phase 0 the baseline is **green**, so every gate below is strict.

| Phase | Scope | Gate |
| --- | --- | --- |
| **0 — Prune** | delete control surface + broken scaffolds + dead refs | `tsc -b` 0 · `vite build` · `vitest` (incl. no-control-import) |
| **1 — Primitives** | `components/` → DS; delete hand-rolled primitives; finish `Badge` in DS | `tsc -b` 0 · `vite build` · DS Storybook a11y for `Badge` |
| **2 — Pages** | public/marketing + auth + account + onboarding + home | + visual screenshots |
| **3 — Workspace** | `WorkspaceShell`/`TopBar`/`CanvasPills` + Env/Logs/Files/Settings/Preview | + visual screenshots |
| **4 — Chat** | 15 chat components (app-local over DS) | + visual screenshots |
| **5 — Tailwind removal + QA** | rip Tailwind dep/config/`@theme`; final cleanup | `tsc -b` 0 · `vite build` · Playwright e2e (surviving flows) · axe a11y · full visual pass |

**Standing verification commands (worktree root / builder filter):**
- `pnpm --filter zeroship-builder exec tsc -b` — zero errors
- `pnpm --filter zeroship-builder exec vite build` — green (fast reachable-code gate)
- `pnpm --filter zeroship-builder test` (vitest) — green
- `pnpm --filter zeroship-builder test:e2e` (Playwright, **system-chromium** per the
  NixOS setup — `reference_playwright_nixos`) — surviving flows green
- Visual: run `pnpm --filter zeroship-builder dev` and screenshot each migrated route.

**Regression discipline:** every gap component (e.g. `Badge`) ships with a test;
removed-canvas e2e specs are deleted, not left dangling; each fix that closes a
defect adds a test that would have failed pre-fix.

## Execution model

Ultracode → **Workflow-orchestrated phases**. Each slice runs
implement → **two-agent review (critic + reviser)** → fix → visual review →
verify, sub-agents on **opus**, **run in background**, **commit-only — never
push**. Phases are gated: a phase's commit lands only when its gate is green.

### Git hygiene (the seeded-worktree caveat)

The worktree was branched from clean `HEAD` and seeded with main's untracked
working-tree files so it builds. Two consequences:

- **The entire Phase-0 removal set is untracked** (`apps.ts`, `control-client.ts`
  +test, `admin/`, the 4 broken canvases, the 2 e2e specs). "Removal" is just
  `rm` from the worktree — no commit/diff, since they were never in `HEAD`. The
  HEAD-committed builder is **already pure-creator** (`projects.ts`/`sandbox.ts`/
  `api.ts`/`server.ts` are tracked).
- **Build-critical files outside this work are untracked** (`sdks/ui/src/
  placeholders.tsx`, `sdks/auth/src/index.ts`, the builder's `oauth*`/`session.ts`).
  They must stay **present but uncommitted**. Therefore: **never `git add -A`** —
  stage migration changes by explicit path only, so unrelated seeded WIP
  (`sdks/auth`, `crates/*`) never leaks into this branch. Worktree `rm`s do not
  touch main's working tree.

## Out of scope / risks

- **Not** implementing the rest of the pure-creator-app spec beyond control
  removal (seed PAT changes, `sandbox-backend` dev.log redirect) — the client and
  the new server modules are already pure-creator; this spec only deletes the
  orphaned control remnants + re-skins. If a server `apps.ts` action turns out to
  be referenced after all, stop and reassess (none found).
- **Risk:** DS `crystal` is a single light-glass theme; some editorial pages
  (Marketing) lose distinctive character. Accepted per the full-crystal decision.
- **Risk:** prop drift between assumed and real DS APIs → mitigated by the
  Storybook MCP `get-documentation` check before use.
- **Pre-launch, no back-compat:** rename/delete freely; no shims, no aliases.
