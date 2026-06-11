# @zeroship/ui Layouts + Blocks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: drive each per-piece task through the project's **`component-slice-flow`** skill (brief → implement → dual-review → fix → visual-review → polish → commit). That skill is the execution loop; this plan is the slice manifest + per-piece briefs. Use `superpowers:subagent-driven-development` to dispatch one fresh subagent per slice and review between slices. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the composition layer to `@zeroship/ui` — 6 layout primitives, 2 app-shell compositions, and 10 composed blocks — so the builder's UI composes governed pieces instead of app-code duplicates.

**Architecture:** A3 layered model. Generic layout primitives at the base (`src/layouts/`), 2 compositions built from them, and 10 blocks (`src/blocks/`). Every piece follows the existing `Card` contract (`forwardRef` / `asChild` via local `Slot` / compound `data-slot` parts / semantic `--zs-*` tokens only / per-component CSS `@import`ed into `styles.css`) and the existing quality gate (Storybook story + `play()` where behavior beats markup + axe-clean via the Test Runner). Catalog-only this round; the builder migration is a separate follow-up.

**Tech Stack:** React 18/19, Base UI (only where headless behavior is needed — most of these are presentational), TypeScript, tsup, Storybook 8 + Test Runner + addon-a11y, CSS custom properties (crystal/HIG token system).

**Reference spec:** `docs/superpowers/specs/2026-05-29-ui-layouts-blocks-design.md`.
**Worktree/branch:** `.claude/worktrees/ui-layouts-blocks` on `builder/ui-layouts-blocks`. **Commit-only — never push.**

---

## How to read this plan

- **Task 0** is concrete scaffolding (real code below) — the shared type vocabulary + subtree wiring every later task imports. Do it first, in this session.
- **Tasks 1–18** are slices. Each carries a complete, self-contained brief (purpose, public API + compound parts, tokens, required states, a11y contract, story/`play()`/axe test focus, commit message). Hand the brief to `component-slice-flow`; do not invent API beyond the brief without recording the decision.
- **Wave gates:** finish + green a wave before starting the next. "Green" = `pnpm --filter @zeroship/ui build` clean **and** `pnpm --filter @zeroship/ui test-storybook:ci` passing for the new stories.

### Per-slice Definition of Done (applies to every Task 1–18)
- [ ] `src/<layer>/<Name>/<Name>.tsx` — `forwardRef`, `asChild` via `../components/_slot` where a render-as target is sensible, compound parts expose `data-slot="<kebab-name>"`, JSDoc header documenting decisions/anti-patterns (match `Card.tsx` house style).
- [ ] `src/<layer>/<Name>/<Name>.css` — reads `--zs-*` tokens only; **no raw hex, no raw px** (rem/oklch only); spacing via the `--zs-space-*` scale mapped from the closed `Gap`/`Pad` unions.
- [ ] `src/<layer>/<Name>/index.ts` — re-exports component + types.
- [ ] `@import` added to `src/styles.css` (keep the existing grouping order).
- [ ] Public exports added to `src/<layer>/index.ts` and surfaced in `src/index.ts`.
- [ ] `src/stories/<Name>.stories.tsx` — every variant as a smoke story; a `play()` only where behavior beats markup (per `.storybook/CONVENTIONS.md`); axe-clean (any skip carries a justifying comment).
- [ ] Dual-review (code-critic + a11y/visual) clean; fixes applied; one commit `feat(ui/<name>): …`.

---

## Task 0: Foundation — subtrees, shared unions, index wiring

**Files:**
- Create: `sdks/ui/src/layouts/_layout-primitives.ts` (shared types + token maps)
- Create: `sdks/ui/src/layouts/index.ts` (empty barrel, filled per slice)
- Create: `sdks/ui/src/blocks/index.ts` (empty barrel, filled per slice)
- Modify: `sdks/ui/src/index.ts` (re-export the two new barrels)
- Create: `sdks/ui/src/stories/Foundations.Layout.stories.tsx` (token/spacing swatch doc — optional but recommended)

- [ ] **Step 1: Create the shared layout vocabulary.** This is the single source of the closed unions every layout primitive imports — prevents arbitrary spacing and keeps gaps token-mapped.

```ts
// sdks/ui/src/layouts/_layout-primitives.ts
/**
 * Shared layout vocabulary. Layout primitives map these closed unions to
 * `--zs-space-*` / flex / grid values. Keeping them closed is the governance
 * lever: consumers cannot pass arbitrary px/rem spacing into a layout.
 */
export type Gap = 0 | "half" | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10;
export type Pad = Gap;
export type Align = "start" | "center" | "end" | "stretch" | "baseline";
export type Justify =
  | "start" | "center" | "end" | "between" | "around" | "evenly";

/** Map a Gap/Pad token to its `--zs-space-*` custom property reference. */
export function spaceVar(token: Gap): string {
  return `var(--zs-space-${token === "half" ? "half" : token})`;
}

const ALIGN: Record<Align, string> = {
  start: "flex-start", center: "center", end: "flex-end",
  stretch: "stretch", baseline: "baseline",
};
const JUSTIFY: Record<Justify, string> = {
  start: "flex-start", center: "center", end: "flex-end",
  between: "space-between", around: "space-around", evenly: "space-evenly",
};
export const alignValue = (a: Align): string => ALIGN[a];
export const justifyValue = (j: Justify): string => JUSTIFY[j];
```

- [ ] **Step 2: Create empty barrels.**

```ts
// sdks/ui/src/layouts/index.ts
// Layout primitives + compositions. Filled per slice.
export type { Gap, Pad, Align, Justify } from "./_layout-primitives";
```
```ts
// sdks/ui/src/blocks/index.ts
// Composed blocks. Filled per slice.
export {};
```

- [ ] **Step 3: Wire into the public surface.** Add to `sdks/ui/src/index.ts`, after the `./components` re-export block and before the placeholder block:

```ts
export * from "./layouts";
export * from "./blocks";
```

- [ ] **Step 4: Verify build is still clean.**

Run: `pnpm --filter @zeroship/ui build`
Expected: ESM + DTS build success (barrels are empty, so no behavior change).

- [ ] **Step 5: Commit.**

```bash
git add sdks/ui/src/layouts sdks/ui/src/blocks sdks/ui/src/index.ts
git commit -m "feat(ui): scaffold layouts/ + blocks/ subtrees + shared layout unions"
```

---

## WAVE 1 — Layout primitives (Tasks 1–6)

> Highest leverage: everything in waves 2–3 composes these. Gate wave 2 on all six green. Layout primitives paint **nothing** (no bg/border) — they only arrange. Their "test" asserts the produced CSS contract (display/flex/grid + token-mapped gap via computed style), not pixel geometry — see `.storybook/CONVENTIONS.md`. Most need no `play()` (pure rendering); add one only where a prop visibly changes structure worth asserting.

### Task 1: `Stack`
**Layer:** `layouts`. **Generalizes:** the ubiquitous flex-row/column.
**API:** `direction?: "row" | "column"` (default `"column"`), `gap?: Gap` (default `0`), `align?: Align`, `justify?: Justify`, `wrap?: boolean`, `asChild?: boolean`, plus `div` props. No compound parts.
**CSS:** `display:flex`; `flex-direction`, `gap: spaceVar(gap)`, `align-items`, `justify-content`, `flex-wrap` driven by inline custom props or data-attrs. No surface paint.
**a11y:** none added (transparent container; role inherited).
**Test focus:** smoke render row & column; assert computed `display:flex` + `flex-direction` + gap resolves to the `--zs-space` value. No `play()`.
**Commit:** `feat(ui/stack): 1-D flex layout primitive (direction/gap/align/justify/wrap)`

### Task 2: `Grid`
**Layer:** `layouts`. **API:** `columns?: number | { sm?: number; md?: number; lg?: number }` (default `1`), `gap?: Gap`, `align?: Align`, `flow?: "row" | "column" | "dense"`, `minColWidth?: string` (when set, uses `repeat(auto-fit, minmax(minColWidth, 1fr))` and `columns` is ignored), `asChild?`, `div` props.
**CSS:** `display:grid`; responsive `columns` object emits the three breakpoint values via media queries keyed off the existing breakpoint tokens (confirm breakpoint vars exist in `styles.css`; if none, define `--zs-bp-sm/md/lg` in the foundation block as part of this slice and note it in the brief). `minColWidth` path is breakpoint-free intrinsic responsive.
**Test focus:** smoke fixed-columns + `minColWidth` auto-fit; assert `display:grid`. No `play()`.
**Commit:** `feat(ui/grid): 2-D grid layout primitive (columns/responsive/minColWidth/gap)`

### Task 3: `Cluster`
**Layer:** `layouts`. **Purpose:** wrap-flow row for pill/tag/action groups.
**API:** `gap?: Gap` (default `2`), `align?: Align` (default `"center"`), `justify?: Justify` (default `"start"`), `asChild?`, `div` props.
**CSS:** `display:flex; flex-wrap:wrap; gap`. **JSDoc MUST state:** "non-interactive visual wrap; use `Toolbar` for a roving-tabindex interactive group." (Distinct from the existing `Toolbar` primitive.)
**Test focus:** smoke with many children wrapping. No `play()`.
**Commit:** `feat(ui/cluster): wrap-flow row layout primitive`

### Task 4: `Container`
**Layer:** `layouts`. **Purpose:** the page width authority; replaces builder `PageFrame` width logic.
**API:** `size?: "sm" | "md" | "lg" | "xl" | "full"` (default `"lg"`), `padX?: Pad` (default a sensible gutter, e.g. `4`), `center?: boolean` (default `true`; `margin-inline:auto`), `asChild?`, `div` props.
**CSS:** `max-width` per size (define `--zs-container-{sm..xl}` in the foundation block as rem; `full` = `none`), `margin-inline:auto`, `padding-inline: spaceVar(padX)`. The ONLY width authority — no other primitive sets `max-width`.
**Test focus:** smoke each size; assert `margin-inline:auto` present. No `play()`.
**Commit:** `feat(ui/container): page-width + centering layout primitive`

### Task 5: `Split` (a.k.a. Sidebar)
**Layer:** `layouts`. **Purpose:** two-pane fixed-side + fluid-main; underlies `AppShell`.
**API (compound):** `<Split>` root + `<Split.Side>` + `<Split.Main>`. Root props: `side?: "start" | "end"` (default `"start"`), `sideWidth?: string` (default e.g. `"16rem"`), `gap?: Gap`, `collapseBelow?: "sm" | "md" | "lg"` (stacks to column below the breakpoint), `asChild?`.
**CSS:** `display:flex`; `Split.Side` is `flex: 0 0 sideWidth`; `Split.Main` is `flex:1; min-width:0`. `side="end"` flips order. `collapseBelow` → `flex-direction:column` under the breakpoint.
**a11y:** layout-only; no roles. Parts emit `data-slot="split-side"` / `"split-main"`.
**Test focus:** smoke start/end + collapse; assert side has fixed basis, main grows. No `play()` (unless collapse is asserted via resize — skip; out of runner scope).
**Commit:** `feat(ui/split): two-pane sidebar/main layout primitive`

### Task 6: `Center`
**Layer:** `layouts`. **API:** `inline?: boolean` (center horizontally only vs both axes), `minHeight?: string`, `asChild?`, `div` props.
**CSS:** `display:flex; align-items:center; justify-content:center`; `inline` drops vertical centering; `minHeight` applied when set. Used by EmptyState/loading screens.
**Test focus:** smoke both modes. No `play()`.
**Commit:** `feat(ui/center): centering layout primitive`

**WAVE 1 GATE:** `pnpm --filter @zeroship/ui build` clean + new layout stories pass the Test Runner. Then proceed.

---

## WAVE 2 — Compositions + small blocks (Tasks 7–17)

### Task 7: `AppShell`
**Layer:** `layouts` (composition). **Generalizes:** builder `WorkspaceShell`.
**API (compound):** `<AppShell>` + `.Header` + `.Sidebar` + `.Main` + `.Footer` (Footer optional). Root: `sidebarOpen?: boolean` + `onSidebarOpenChange?: (open: boolean) => void` (controlled) with uncontrolled `defaultSidebarOpen?`, `sidebarWidth?: string`, `sidebarSide?: "start" | "end"`.
**Built from:** `Split` (sidebar/main) under a fixed `Header`; full-height grid (`header` row / `body` row). Parts emit `data-slot="appshell-*"`.
**a11y:** Header is `<header>`, Sidebar `<aside>` (or `<nav>` if consumer passes nav), Main `<main>`, Footer `<footer>`. **Bake in a skip-to-content link** targeting Main. **Do NOT re-scope `data-theme`** — theme host stays `<html>`; verify overlays opened from inside inherit theme.
**States:** sidebar open/collapsed.
**Test focus:** `play()` — toggle sidebar via `onSidebarOpenChange`, assert `aria`/visibility flips; assert single `<main>` + skip link focuses Main.
**Commit:** `feat(ui/appshell): header/sidebar/main/footer app frame`

### Task 8: `PageHeader`
**Layer:** `layouts` (composition). **Generalizes:** builder `PageFrame`/`TopBar` header band.
**API (compound):** `<PageHeader>` + `.Breadcrumbs` + `.Title` + `.Description` + `.Actions`. `Title` is `<h1>` by default; `asChild` to relevel to match document outline. `.Actions` lays out as a right-aligned `Cluster`.
**a11y:** Breadcrumbs is `<nav aria-label="Breadcrumb">` with an ordered list; Title carries the heading.
**Test focus:** smoke full + minimal (title only); assert heading present, actions cluster right. No `play()` unless an action interaction is shown.
**Commit:** `feat(ui/pageheader): page title band (breadcrumbs/title/description/actions)`

### Task 9: `EmptyState`
**Layer:** `blocks`. **Generalizes:** builder `EmptyState`.
**API:** ergonomic `icon?`, `title`, `description?`, `action?` props **and** compound `.Icon`/`.Title`/`.Description`/`.Actions` for advanced layout. Centered via `Center`.
**a11y:** Title is a heading (`<h2>` default, `asChild` to relevel); container is a plain region (no `role="alert"` — empty is not an error). Icon `aria-hidden`.
**Test focus:** smoke prop form + compound form; axe. No `play()`.
**Commit:** `feat(ui/emptystate): zero-data block`

### Task 10: `ErrorState`
**Layer:** `blocks`. **Generalizes:** builder `ErrorState`.
**API:** `intent?: "error" | "warning"` (default `"error"`), ergonomic `title`/`description`/`onRetry?` + compound `.Title`/`.Description`/`.Actions`. When `onRetry` set, render a retry `Button`.
**a11y:** Use `role="alert"` ONLY when the error appears live (prop `live?: boolean`, default `false`); a statically-rendered error page should not assert alert. Document this in JSDoc.
**Test focus:** `play()` — click retry, assert `onRetry` called. axe.
**Commit:** `feat(ui/errorstate): failure block with optional retry`

### Task 11: `Skeleton`
**Layer:** `blocks`. **Generalizes:** builder `Skeleton`.
**API:** `variant?: "text" | "rect" | "circle"` (default `"text"`), `lines?: number` (text only), `width?: string`, `height?: string`.
**CSS:** shimmer animation; **`@media (prefers-reduced-motion: reduce)` disables the shimmer** (paint preserved) — match the Progress/Separator wave-10 precedent.
**a11y:** `aria-hidden="true"` (decorative; the live region/spinner conveys loading). 
**Test focus:** smoke each variant; reduced-motion story asserting animation disabled. No `play()`.
**Commit:** `feat(ui/skeleton): loading placeholder block (reduced-motion aware)`

### Task 12: `Spinner`
**Layer:** `blocks`. **Generalizes:** builder `Spinner`.
**API:** `size?: "sm" | "md" | "lg"`, `label?: string` (default `"Loading"`, rendered visually-hidden).
**CSS:** rotation animation; reduced-motion → slower/stepped or static per a11y norms (document choice).
**a11y:** `role="status"` + visually-hidden label so SRs announce loading.
**Test focus:** smoke sizes; assert `role="status"` + accessible name. No `play()`.
**Commit:** `feat(ui/spinner): indeterminate busy indicator block`

### Task 13: `Badge` (promote placeholder → real)
**Layer:** `blocks`. **Replaces:** `src/placeholders.tsx` `Badge`.
**API:** `intent?: "neutral" | "info" | "success" | "warning" | "danger"` (default `"neutral"`), `variant?: "solid" | "soft" | "outline"` (default `"soft"`), `size?: "sm" | "md"`, `asChild?`, `span` props.
**CSS:** map intent→state tokens (`--zs-state-*` if present, else label/fill families — confirm against `styles.css`), per variant. `--zs-radius-full` pill.
**Wiring:** remove the `Badge` re-export from `src/index.ts`'s placeholder block and delete it from `placeholders.tsx`; export the real one from `blocks`. (Pre-launch, no alias.)
**Test focus:** smoke all intents × variants; axe contrast on every combo. No `play()`.
**Commit:** `feat(ui/badge): real status/count badge (replaces placeholder)`

### Task 14: `Tag` (a.k.a. Chip)
**Layer:** `blocks`. **Generalizes:** builder `Pill`/`FilterPill`.
**API:** `removable?: boolean`, `onRemove?: () => void`, `selected?: boolean` (filter-chip state), `size?: "sm" | "md"`, `leadingIcon?`, `span`/`button` props. When `selected` is controllable, support `onSelectedChange?`.
**a11y:** when `removable`, render a real `<button aria-label="Remove {label}">`; when `selected` is a filter toggle, the tag root is a `<button aria-pressed>`. Keyboard: ⌫/Delete on the tag triggers `onRemove`.
**Test focus:** `play()` — click remove → `onRemove`; keyboard Delete → `onRemove`; toggle selected → `aria-pressed` flip. axe.
**Commit:** `feat(ui/tag): removable/selectable tag (generalizes Pill/FilterPill)`

### Task 15: `StatCard`
**Layer:** `blocks`. **Generalizes:** builder `Kpi`. **Built on:** `Card`.
**API:** `label: string`, `value: ReactNode`, `delta?: { value: string; direction: "up" | "down" | "flat" }`, `icon?`, plus pass-through to `Card`.
**CSS:** delta direction → color via state tokens + an `aria-hidden` directional glyph; value uses a title type token.
**a11y:** label + value read as a coherent unit; delta direction conveyed textually (not color alone — e.g. "up 12%").
**Test focus:** smoke up/down/flat; axe contrast on delta colors. No `play()`.
**Commit:** `feat(ui/statcard): KPI/metric card block`

### Task 16: `Banner` (a.k.a. Callout)
**Layer:** `blocks`. **Generalizes:** builder `LiveBanner`.
**API:** `intent?: "info" | "success" | "warning" | "danger"` (default `"info"`), `dismissible?: boolean`, `onDismiss?: () => void`, compound `.Title`/`.Description`/`.Actions`, leading icon per intent.
**a11y:** `role` per intent — `"status"` for info/success, `"alert"` for warning/danger ONLY when rendered live (document the live caveat as in ErrorState). Dismiss is a real `<button aria-label>`.
**Test focus:** `play()` — dismiss → `onDismiss`, banner removed. axe each intent.
**Commit:** `feat(ui/banner): inline page-level message block`

### Task 17: `DescriptionList`
**Layer:** `blocks`. **Generalizes:** builder `LedgerRow`/`Receipt`.
**API (compound):** `<DescriptionList>` + `.Item` + `.Term` + `.Detail`. Root: `orientation?: "horizontal" | "vertical"` (default `"horizontal"`), `divider?: boolean`.
**a11y:** semantic `<dl>` / `<dt>` / `<dd>` (Item wraps a `<dt>`+`<dd>` pair — use `<div>` inside `<dl>` which is valid grouping).
**Test focus:** smoke both orientations + divider; assert `dl/dt/dd` structure. No `play()`.
**Commit:** `feat(ui/descriptionlist): key-value list block`

**WAVE 2 GATE:** build clean + all wave-2 stories pass the Test Runner. Then proceed.

---

## WAVE 3 — DataTable (Task 18)

### Task 18: `DataTable`
**Layer:** `blocks`. **Generalizes:** the long-promised `Table`. **Presentational only — the consumer owns data fetching/sorting logic; the component renders state + emits intent.**

> This slice gets its **own brief expansion** before implementation — settle these decisions in the slice brief and record them in the component JSDoc:
> 1. **Sortable headers:** header is a `<button>` inside `<th aria-sort>`; clicking emits `onSortChange`. Keyboard = native button.
> 2. **Selection:** opt-in `selection?: "none" | "single" | "multiple"`; checkbox column uses the existing `Checkbox`; rows carry `aria-selected`; header checkbox = select-all (indeterminate state).
> 3. **Sticky header:** `stickyHeader?: boolean` via `position:sticky` on `<thead>`.
> 4. **Density:** `density?: "comfortable" | "compact"`.
> 5. **Empty / loading / error slots:** `renderEmpty?`, `loading?` (→ Skeleton rows), `renderError?` — wire to the wave-2 blocks.
> 6. **Virtualization:** **DEFERRED** by default (document as a known limitation; revisit only if a real builder table needs it). Do not add a windowing dep this round.

**API:**
```ts
interface Column<T> {
  key: string;
  header: ReactNode;
  cell: (row: T) => ReactNode;
  sortable?: boolean;
  align?: "start" | "center" | "end";
  width?: string;
}
interface DataTableProps<T> {
  columns: Column<T>[];
  data: T[];
  rowKey: (row: T) => string;
  sort?: { key: string; direction: "asc" | "desc" } | null;
  onSortChange?: (sort: { key: string; direction: "asc" | "desc" }) => void;
  selection?: "none" | "single" | "multiple";
  selectedKeys?: string[];
  onSelectionChange?: (keys: string[]) => void;
  stickyHeader?: boolean;
  density?: "comfortable" | "compact";
  loading?: boolean;
  renderEmpty?: () => ReactNode;   // defaults to <EmptyState/>
  renderError?: () => ReactNode;
}
```
**a11y:** real `<table>`/`<thead>`/`<tbody>`/`<th scope>`/`<td>`; `aria-sort` on the sorted column header; `aria-selected` on rows; caption or `aria-label` required (dev-warn if neither).
**States:** populated, empty (→ `EmptyState`), loading (→ `Skeleton` rows), error (→ `renderError`).
**Test focus:** `play()` — click sortable header → `onSortChange` with toggled direction + `aria-sort` flips; select-all checkbox → `onSelectionChange` with all keys + header indeterminate when partial; empty data renders `EmptyState`. axe on populated + empty + loading.
**Commit:** `feat(ui/datatable): presentational data table (sort/selection/sticky/states)`

**WAVE 3 GATE:** build clean + DataTable stories pass the Test Runner.

---

## Final verification (after Task 18)

- [ ] `pnpm --filter @zeroship/ui build` — ESM + DTS clean.
- [ ] `pnpm --filter @zeroship/ui test-storybook:ci` — full runner green (all old + 18 new pieces, smoke + `play()` + axe).
- [ ] `src/index.ts` exports every new piece + its public types; `placeholders.tsx` no longer exports `Badge`.
- [ ] `grep` the new CSS files for raw hex / raw px — none.
- [ ] Spec deferred items still deferred (no builder migration crept in).

---

## Self-review (against the spec)

**Spec coverage:** §4a primitives → Tasks 1–6 ✓. §4b compositions → Tasks 7–8 ✓. §4c blocks (10) → Tasks 9–18 ✓ (EmptyState/ErrorState/Skeleton/Spinner/Badge/Tag/StatCard/Banner/DescriptionList/DataTable). §3 conventions → per-slice DoD ✓. §5 waves → Wave 1/2/3 ✓. §6 testing → test-focus per task + final runner ✓. §8 deferred → final-verification guard + DataTable virtualization note ✓.

**Placeholder scan:** DataTable's "own brief expansion" is a deliberate, enumerated decision list (not a TODO) — the six decisions are stated. Grid/Container note that breakpoint/container vars may need defining — flagged as in-slice work, not vague. No "TBD"/"handle edge cases"/"similar to Task N".

**Type consistency:** `Gap`/`Pad`/`Align`/`Justify` defined in Task 0, imported by Tasks 1–8. `spaceVar` used consistently. `Column<T>`/`DataTableProps<T>` self-contained in Task 18. Compound-part `data-slot` naming is `<name>-<part>` throughout.
