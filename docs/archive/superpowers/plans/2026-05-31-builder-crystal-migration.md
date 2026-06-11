# zeroship-builder Crystal Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Sub-agents run on **opus**, **in background**, **commit-only — NEVER push**.

**Goal:** Re-skin `apps/zeroship-builder` onto the `@zeroship/ui` crystal design system — deleting Tailwind, the hand-rolled primitives, all control-plane code, and the broken WIP scaffolds — leaving a clean pure-creator-app console that is `tsc -b` + `vite build` green and visually native to the DS.

**Architecture:** Sequential phases against a green baseline. Phase 0 prunes (deletes untracked control + broken files) → baseline goes green. Phases 1–5 re-skin per-file: replace hand-rolled primitives with DS components, rebuild custom markup from DS layout primitives + `--zs-*` crystal tokens, drop Tailwind last. Each file/slice is dual-reviewed (critic + reviser) and gated.

**Tech Stack:** React 18, Vite 8, `@zeroship/ui` (Base UI + crystal tokens, ESM, `import "@zeroship/ui/styles.css"` + `<ThemeProvider defaultTheme="crystal">`), `@tanstack/react-query`, React Router v6, Playwright (system-chromium), Vitest.

**Spec:** `docs/superpowers/specs/2026-05-31-builder-crystal-migration-design.md`

---

## Working context

- **Worktree:** `.worktrees/builder-ui-crystal`, branch `builder/ui-crystal-migration`.
- **Run filter:** `pnpm --filter zeroship-builder <script>` from the worktree root.
- **Git hygiene (critical):** the worktree is seeded with main's untracked WIP. **NEVER `git add -A`** — stage by explicit path only. The Phase-0 removal set is all *untracked*, so deleting is `rm` (no diff). Build-critical untracked files (`sdks/ui/src/placeholders.tsx`, `sdks/auth/src/index.ts`, builder `oauth*`/`session.ts`) must stay present-but-uncommitted.
- **DS prop verification:** before using any `@zeroship/ui` prop, confirm it with the Storybook MCP (`http://127.0.0.1:6006/mcp`, `get-documentation`). Do NOT assume props by name (AGENTS.md mandate). Start Storybook once: `pnpm --filter @zeroship/ui storybook`.

## Standing verification commands (every phase gate)

```bash
# from worktree root
pnpm --filter zeroship-builder exec tsc -b          # expect: 0 errors
pnpm --filter zeroship-builder exec vite build      # expect: built, writes dist/app.zship
pnpm --filter zeroship-builder test                 # vitest, incl. no-control-import.test.ts
```
Visual (phases 2–5): `pnpm --filter zeroship-builder dev`, screenshot each migrated route.
e2e (phase 5): `pnpm --filter zeroship-builder test:e2e` (Playwright, system-chromium).

---

## File-structure decomposition

**Deleted in Phase 0 (all untracked):**
```
src/server/apps.ts                        # orphaned control module
src/server/control-client.ts  (+ .test.ts)
src/client/admin/                         # whole dir: AdminShell + pages/*
src/client/workspace/canvases/DataCanvas.tsx
src/client/workspace/canvases/HealthCanvas.tsx
src/client/workspace/canvases/MediaCanvas.tsx
src/client/workspace/canvases/PlanCanvas.tsx
e2e/data-media.spec.ts
e2e/plan-health.spec.ts
```

**Deleted across phases 1–5 (hand-rolled primitives, `src/client/components/`):**
`Button, GhostButton, StampButton, Pill, FilterPill, Modal, Skeleton, Spinner, Toast, EmptyState, ErrorState, LiveBanner, Kpi, LedgerRow, PageFrame, TopBar, Marginalia, PublicNav` → replaced by DS. `ErrorBoundary` (keep logic), `ProductTour` (keep logic, restyle), `Receipt`/`ProjectCard`/`TemplateCard`/`NotebookPrompt`/`DevEventsBadge` (rebuild app-local over DS).

**New / changed foundation files:**
```
src/client/lib/utils.ts                   # cn(): clsx only (drop tailwind-merge)
src/client/index.css                       # strip editorial @theme + utility classes
src/client/app.css            (new)        # minimal app-scoped CSS over --zs-* (if needed)
src/client/design/tokens.ts                # re-point to --zs-* OR delete if unused
vite.config.ts                             # remove @tailwindcss/vite (phase 5)
package.json                               # drop tailwindcss, @tailwindcss/vite, tailwind-merge (phase 5)
```

**DS contribution:** finish `Badge` in `sdks/ui/src/components/Badge/` (replaces the `placeholders.tsx` stub).

---

## Reusable procedure: PER-FILE MIGRATION

Every re-skin task (phases 1–5) for a file `F` follows this procedure. (Defined once; each task below names `F` + its specific DS mapping + gotchas.)

- [ ] **P1. Read `F` fully.** Inventory its imports of hand-rolled primitives, its `className` Tailwind utilities (editorial tokens: `bg-paper`, `text-ink`, `text-tomato`, `font-serif`, `.hairline`, `.stamp-btn`, etc.), and its layout structure.
- [ ] **P2. Replace primitive imports** with `@zeroship/ui` equivalents per the mapping table. Verify each DS component's props via Storybook MCP `get-documentation` before use.
- [ ] **P3. Rebuild custom markup** using DS layout primitives (`Stack`/`Grid`/`Cluster`/`Container`/`Split`/`Center`) instead of Tailwind flex/grid utilities. Replace editorial color/type/spacing classes with crystal: either a DS component, or a scoped CSS class in a co-located `.css`/`app.css` reading `--zs-*` tokens. No raw hex/px; no Tailwind utilities.
- [ ] **P4. Typecheck the file's package:** `pnpm --filter zeroship-builder exec tsc -b` → 0 errors.
- [ ] **P5. Build:** `pnpm --filter zeroship-builder exec vite build` → green.
- [ ] **P6. Dual review** (critic → reviser) on the diff; apply fixes; re-run P4–P5.
- [ ] **P7. Visual check** (phases 2–5): render the route in `dev`, screenshot, confirm crystal look + no layout breakage.
- [ ] **P8. Commit** (explicit paths): `git add <F and co-located css> && git commit -m "refactor(builder): migrate <F> to @zeroship/ui crystal"`.

**Primitive → DS mapping (used in P2):**

| hand-rolled | DS | notes |
| --- | --- | --- |
| `Button` (primary/secondary/ghost/destructive/link) | `Button` | `primary→variant="filled"`; `secondary→"gray"`/`"tinted"`; `ghost→"plain"`; `destructive→"filled"`+red intent; `link→"plain"` |
| `StampButton` | `Button variant="filled"` | drop the stamp/rotation chrome |
| `GhostButton` | `Button variant="plain"` | |
| `Pill` | `ToggleGroup`/`Tag` | toggle state → `ToggleGroup`; static label → `Tag` |
| `FilterPill` | `FilterBar` or `ToggleGroup` | |
| `Modal` | `Dialog` | focus-trap/Esc handled by DS |
| `Skeleton`/`Spinner`/`Toast` | `Skeleton`/`Spinner`/`Toast` | |
| `EmptyState`/`ErrorState`/`LiveBanner` | `EmptyState`/`ErrorState`/`Banner` | |
| `Kpi`/`LedgerRow` | `StatCard`/`ListView` or `DataTable` | |
| `PageFrame`/`TopBar`/`Marginalia`/`PublicNav` | `AppShell` / `Container`+`PageHeader` / `NavigationMenu` | drop marginalia concept |
| `Receipt`/`ProjectCard`/`TemplateCard`/`NotebookPrompt` | app-local over `Card`+`DescriptionList` | keep as builder components, DS-built |
| `DevEventsBadge` | `Badge` | after Badge lands in DS |

---

## Phase 0 — Prune (→ green baseline)

### Task 0.1: Delete control surface + broken scaffolds

**Files:** delete-only (all untracked).

- [ ] **Step 1: Delete the files**

```bash
cd .worktrees/builder-ui-crystal/apps/zeroship-builder
rm -f src/server/apps.ts src/server/control-client.ts src/server/control-client.test.ts
rm -rf src/client/admin
rm -f src/client/workspace/canvases/DataCanvas.tsx \
      src/client/workspace/canvases/HealthCanvas.tsx \
      src/client/workspace/canvases/MediaCanvas.tsx \
      src/client/workspace/canvases/PlanCanvas.tsx
rm -f e2e/data-media.spec.ts e2e/plan-health.spec.ts
```

- [ ] **Step 2: Find dead references to the deleted UI**

```bash
grep -rnE "DataCanvas|HealthCanvas|MediaCanvas|PlanCanvas|/admin|AdminShell|control-client|getControlClient|@zeroship/control" src App.tsx 2>/dev/null
# Also inspect canvas pills / tier config for dead 'data'|'health'|'media'|'plan' entries:
grep -rnE "\"data\"|\"health\"|\"media\"|\"plan\"|data:|health:|media:|plan:" src/client/workspace/CanvasPills.tsx src/client/workspace/WorkspaceShell.tsx
```
Expected: only matches inside files already deleted, or dead pill/tier entries to remove. Remove any dead pill labels, tier→canvas registry entries, and routes for the deleted canvases/admin.

- [ ] **Step 3: Verify green baseline**

Run the three standing commands. Expected: `tsc -b` **0 errors**; `vite build` writes `dist/app.zship`; `vitest` green including `src/server/no-control-import.test.ts`.

- [ ] **Step 4: Remove the stale baseline snapshot**

```bash
rm -f apps/zeroship-builder/.migration/tsc-baseline-errors.txt
rmdir apps/zeroship-builder/.migration 2>/dev/null || true
```

- [ ] **Step 5: Commit** (explicit paths — most removals are untracked so they won't appear; stage only tracked edits, e.g. CanvasPills/WorkspaceShell/App.tsx cleanups)

```bash
git add apps/zeroship-builder/src/client/workspace/CanvasPills.tsx \
        apps/zeroship-builder/src/client/workspace/WorkspaceShell.tsx \
        apps/zeroship-builder/src/client/App.tsx   # whichever were edited
git commit -m "chore(builder): remove control surface + broken scaffolds (pure-creator-app prune)"
```

> If Step 2 found no tracked edits, there is nothing to commit (deletions were untracked). Record the green baseline in the next foundation commit instead.

---

## Phase 0b — Foundation (non-breaking; Tailwind stays idle)

### Task 0b.1: `cn()` → clsx-only

**Files:** Modify `src/client/lib/utils.ts`; Test `src/client/lib/utils.test.ts`.

- [ ] **Step 1: Write failing test**

```ts
import { describe, it, expect } from "vitest";
import { cn } from "./utils";
describe("cn", () => {
  it("joins truthy class names", () => {
    expect(cn("a", false && "b", "c")).toBe("a c");
  });
});
```

- [ ] **Step 2: Run, expect fail** if `cn` still imports `tailwind-merge` and it's removed; otherwise expect pass. Run: `pnpm --filter zeroship-builder test -- utils.test`.

- [ ] **Step 3: Implement**

```ts
import clsx, { type ClassValue } from "clsx";
export function cn(...inputs: ClassValue[]): string {
  return clsx(inputs);
}
```

- [ ] **Step 4: Run test → PASS.**
- [ ] **Step 5: Commit** `git add src/client/lib/utils.ts src/client/lib/utils.test.ts && git commit -m "refactor(builder): cn() drops tailwind-merge"`

### Task 0b.2: tokens.ts + index.css decision

**Files:** `src/client/design/tokens.ts`, `src/client/index.css`, maybe new `src/client/app.css`.

- [ ] **Step 1: Find tokens.ts consumers**: `grep -rn "design/tokens" src/client`. If the only consumers were the deleted canvases (SVG/canvas drawing), delete `tokens.ts`. Otherwise re-point each constant to a `--zs-*` value (read the value via `getComputedStyle` at runtime, or hard-map to the crystal token's resolved value documented in `sdks/ui/src/styles.css`).
- [ ] **Step 2:** Leave `index.css`'s `@import "tailwindcss"` + `@theme` block IN PLACE for now (idle); they're removed in Phase 5. Confirm `main.tsx` keeps `import "@zeroship/ui/styles.css"` + `<ThemeProvider defaultTheme="crystal" persist applyToDocument>`.
- [ ] **Step 3:** Verify standing commands green. Commit edited files by explicit path.

### Task 0b.3: Finish `Badge` in `@zeroship/ui`

**Files:** `sdks/ui/src/components/Badge/` (Badge.tsx, Badge.css, index.ts), `sdks/ui/src/stories/Badge.stories.tsx`; remove the Badge stub from `sdks/ui/src/placeholders.tsx`; wire export in `sdks/ui/src/index.ts`.

- [ ] Use the **component-slice-flow** skill (brief → implement → dual-review → fix → visual-review → polish). Badge variants/tones per the DS token system (`--zs-system-*`, `--zs-fill-*`). Storybook a11y must pass.
- [ ] Rebuild the DS: `pnpm --filter @zeroship/ui build`. Commit DS files by explicit path.

---

## Phase 1 — Primitives swap

Migrate every surviving file that imports a hand-rolled primitive, then delete the primitives. Apply the **PER-FILE MIGRATION** procedure to each consumer, then:

- [ ] **Task 1.1:** Sweep all imports of `components/{Button,GhostButton,StampButton,Pill,FilterPill,Modal,Skeleton,Spinner,Toast,EmptyState,ErrorState,LiveBanner,Kpi,LedgerRow,PageFrame,TopBar,Marginalia,PublicNav}` across `src/client` and replace with DS per the mapping table. Find them: `grep -rlE "components/(Button|GhostButton|StampButton|Pill|FilterPill|Modal|Skeleton|Spinner|Toast|EmptyState|ErrorState|LiveBanner|Kpi|LedgerRow|PageFrame|TopBar|Marginalia|PublicNav)" src/client`.
- [ ] **Task 1.2:** Rebuild app-local `Receipt`, `ProjectCard`, `TemplateCard`, `NotebookPrompt`, `DevEventsBadge` over DS `Card`/`DescriptionList`/`Badge`. Restyle `ErrorBoundary` fallback with DS `ErrorState`; restyle `ProductTour` over DS `Popover`/`Dialog`.
- [ ] **Task 1.3:** Delete the replaced primitive files: `rm src/client/components/{Button,GhostButton,StampButton,Pill,FilterPill,Modal,Skeleton,Spinner,Toast,EmptyState,ErrorState,LiveBanner,Kpi,LedgerRow,PageFrame,TopBar,Marginalia,PublicNav}.tsx`.
- [ ] **Phase 1 gate:** `tsc -b` 0 · `vite build` green · `vitest` green · DS Badge Storybook a11y green. Commit per-file.

---

## Phase 2 — Pages

Apply PER-FILE MIGRATION to each, with DS mapping:

- [ ] **Marketing.tsx** → `Hero` + `FeatureGrid` + `StatsBand` + `Cta` + `Footer` (DS sections), `Container`.
- [ ] **Pricing.tsx** → `PricingTable`.
- [ ] **Skills.tsx / Templates.tsx** → `FeatureGrid`/`Grid`+`Card`.
- [ ] **About.tsx / Changelog.tsx / Privacy.tsx / Terms.tsx** → `Container`+`PageHeader`+prose in `Card`.
- [ ] **Login.tsx / Signup.tsx / ForgotPassword.tsx** → `AuthForm` block (+ existing `@zeroship/auth` SignInButton where used).
- [ ] **Account.tsx** → `FormSection`+`Card`+`Input`.
- [ ] **OnboardingIntent.tsx** → `Stepper`/`Card` choice tiles.
- [ ] **Home.tsx** → `Grid`+`Card`+`EmptyState`+`FilterBar` (project gallery; uses `listProjects`).
- [ ] **WizardWorkspace.tsx** → `Stepper`+`FormSection`+`Card`.
- [ ] **NotFound.tsx** → `EmptyState`.
- [ ] **Phase 2 gate:** standing commands green + visual screenshots of every page route.

---

## Phase 3 — Workspace shell + surviving canvases

- [ ] **WorkspaceShell.tsx** → `AppShell` (header/sidebar/body); chat rail via `Split` (desktop) / `Drawer` (phone, `useMediaQuery` already present).
- [ ] **workspace/TopBar.tsx** → `AppShell` header + `Breadcrumbs`/`Menu` (project name, tier toggle, tour).
- [ ] **CanvasPills.tsx** → `Tabs` or `ToggleGroup` (surviving canvases only: preview/files/logs/env/settings).
- [ ] **PreviewCanvas.tsx** → iframe in a DS `Card`/`Stack` frame.
- [ ] **canvases/EnvCanvas.tsx** → `FormSection`+`Input`+`Button` (or `DataTable` for the var list); uses `getEnv/setEnv/deleteEnv`.
- [ ] **canvases/LogsCanvas.tsx** → `Card` + app-local log-stream list + `FilterBar`/`ToggleGroup`; uses `getLogs`.
- [ ] **canvases/FilesCanvas.tsx** → `Split` (tree + viewer); app-local CodeMirror viewer (`@uiw/react-codemirror`) themed to crystal; uses `listSandboxFiles/readSandboxFile`.
- [ ] **canvases/SettingsCanvas.tsx** → already on DS (`Button/Card/Input/AlertDialog`); align spacing to DS `FormSection`, remove any residual Tailwind.
- [ ] **Phase 3 gate:** standing commands green + visual screenshots of `/p/:appId/{preview,files,logs,env,settings}`.

---

## Phase 4 — Chat

Apply PER-FILE MIGRATION to the 15 chat components (app-local over DS):

- [ ] **ChatRail.tsx** → `Stack`/`Split` panel; **ChatMessages.tsx** → `Stack` list; **ChatComposer.tsx** → DS `Input`/textarea + `Button`.
- [ ] **MentionDropdown.tsx** → DS `Combobox`/`Popover` (uses `listIssues`).
- [ ] **MessageUser/MessageAssistant/MessageActions** → `Card`/`Avatar`/`Menu`.
- [ ] **BriefCard/DiffCard/SurveyCard/CriticRoundCard/ReviewerRoundCard/PMRecommendationCard/SREFindingCard/Receipt** → `Card`+`DescriptionList`+`Badge`+`Banner`.
- [ ] **Phase 4 gate:** standing commands green + visual screenshot of an active chat session.

---

## Phase 5 — Tailwind removal + final QA

- [ ] **Task 5.1:** Confirm zero Tailwind utility classes remain: `grep -rnE "class(Name)?=\"[^\"]*\b(bg-|text-|flex|grid|p-[0-9]|m-[0-9]|gap-|font-(serif|sans|mono)|hairline|stamp-btn|label-uc)" src/client` → expect no matches (or only DS-driven class names). Fix stragglers.
- [ ] **Task 5.2:** Strip `index.css` to the minimal app-scoped sheet (remove `@import "tailwindcss"` + `@theme` + editorial utilities). Keep only resets/app-shell rules over `--zs-*`.
- [ ] **Task 5.3:** Remove `@tailwindcss/vite` from `vite.config.ts`; drop `tailwindcss`, `@tailwindcss/vite`, `tailwind-merge` from `package.json`. Run `pnpm install` (worktree root).
- [ ] **Task 5.4:** Final gates — `tsc -b` 0 · `vite build` green · `vitest` green · `test:e2e` (Playwright system-chromium) green for surviving flows · axe a11y pass on key routes · full visual pass.
- [ ] **Task 5.5:** Commit; update spec status to "implemented".

---

## Self-review (author checklist — completed)

- **Spec coverage:** prune (Phase 0), Tailwind drop (0b/5), primitives (1), pages (2), workspace+canvases (3), chat (4), Badge gap (0b.3), DS-prop verification (procedure P2), git hygiene (working-context) — all mapped. ✓
- **Placeholder scan:** per-file JSX is intentionally produced at implementation time (read file + Storybook MCP), not fabricated here — this is faithful-work, not a placeholder; deterministic steps (Phase 0 commands, `cn()`) carry complete code. ✓
- **Type/name consistency:** DS component names verified against the catalog; surviving API names (`getEnv/setEnv/deleteEnv/getLogs/listSandboxFiles/readSandboxFile/listProjects/listIssues`) verified against `client/api.ts`. ✓
