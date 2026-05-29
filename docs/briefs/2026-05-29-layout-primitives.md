# Slice brief: Layout primitives (Wave 1)

**Worktree:** `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`
**Branch:** `builder/ui-layouts-blocks` (NOT the historical `builder/ui-design`).
**Plan:** `docs/superpowers/plans/2026-05-29-ui-layouts-blocks.md` (Tasks 1–6).
**Spec:** `docs/superpowers/specs/2026-05-29-ui-layouts-blocks-design.md`.

This slice ships SIX cohesive layout primitives in one pass. They are tiny,
share the `_layout-primitives.ts` vocabulary (already created in Task 0), and
all paint **nothing** (no background/border) — they only arrange.

> **DO NOT commit, push, or merge.** The orchestrator stages and commits after
> verifying every gate. Implement + self-verify + report only.

---

## Reference patterns to mirror (read these first)

- `sdks/ui/src/components/Separator/{Separator.tsx,Separator.css,index.ts}` —
  simplest recent component; mirror file shape, JSDoc density, `data-slot`,
  forced-colors/RTL handling.
- `sdks/ui/src/components/Card/Card.tsx` — compound parts + `asChild` via
  `Slot`, `data-slot` vocabulary, dev-warn on invalid render-as.
- `sdks/ui/src/components/_slot.ts` — `Slot`, `mergeProps`, `composeRefs`,
  `getElementRef` (React-19-safe). Route ALL `asChild` through this.
- `sdks/ui/src/components/_classnames.ts` — `classnames(...)` composer.
- `sdks/ui/src/stories/Separator.stories.tsx` — story shape, `data-testid`,
  autodocs JSDoc, `play()` placement per `.storybook/CONVENTIONS.md`.
- Shared vocabulary already exists: `sdks/ui/src/layouts/_layout-primitives.ts`
  exports `Gap`, `Pad`, `Align`, `Justify`, `spaceVar`, `alignValue`,
  `justifyValue`. Import from there; do NOT redefine.

Imports from a layout primitive (`src/layouts/<Name>/<Name>.tsx`):
`import { Slot } from "../../components/_slot";`
`import { classnames } from "../../components/_classnames";`
`import { type Gap, spaceVar, ... } from "../_layout-primitives";`

---

## Foundation token additions (do these in `styles.css` foundation block)

The system is currently fluid (only the reduced-motion media query exists).
These primitives need a minimal, governed responsive + width scale. Add to the
`:root` foundation block (theme-invariant), in `rem`, with a short comment:

```css
/* ─── Breakpoints — minimal responsive scale for layout primitives.
   The system is fluid by default; these gate Grid `columns` objects and
   Split `collapseBelow` only. */
--zs-bp-sm: 40rem;   /* 640 */
--zs-bp-md: 48rem;   /* 768 */
--zs-bp-lg: 64rem;   /* 1024 */

/* ─── Container max-widths — Container is the sole width authority. */
--zs-container-sm: 30rem;   /* 480 */
--zs-container-md: 48rem;   /* 768 */
--zs-container-lg: 64rem;   /* 1024 */
--zs-container-xl: 80rem;   /* 1280 */
```

> Note: CSS custom properties can't be used directly inside `@media` query
> conditions. For `Grid` responsive `columns` and `Split` `collapseBelow`,
> write the media queries with the literal rem values **and** a comment
> referencing the token name, e.g. `@media (min-width: 40rem) { /* --zs-bp-sm */ }`.
> This keeps the px-purity gate clean (rem only) while documenting intent.

---

## The six primitives

All: `forwardRef`, accept native `div` props, support `asChild` via `Slot`,
emit `data-slot="<name>"`. Drive layout via inline CSS custom properties set
from props (e.g. `style={{ "--stack-gap": spaceVar(gap) }}`) consumed by the
component CSS — never inline raw values, never arbitrary spacing.

### 1. Stack (`src/layouts/Stack/`)
```ts
interface StackProps extends ComponentPropsWithoutRef<"div"> {
  direction?: "row" | "column";   // default "column"
  gap?: Gap;                        // default 0
  align?: Align;
  justify?: Justify;
  wrap?: boolean;
  asChild?: boolean;
}
```
CSS: `display:flex`; `flex-direction`, `gap`, `align-items`, `justify-content`,
`flex-wrap` from props. No paint.

### 2. Grid (`src/layouts/Grid/`)
```ts
interface GridProps extends ComponentPropsWithoutRef<"div"> {
  columns?: number | { sm?: number; md?: number; lg?: number };  // default 1
  gap?: Gap;
  align?: Align;
  flow?: "row" | "column" | "dense";
  minColWidth?: string;   // when set → repeat(auto-fit, minmax(minColWidth,1fr)); columns ignored
  asChild?: boolean;
}
```
CSS: `display:grid`. `minColWidth` is the breakpoint-free intrinsic path
(lead with it in docs). Responsive `columns` object emits the sm/md/lg values
via the breakpoint media queries above.

### 3. Cluster (`src/layouts/Cluster/`)
```ts
interface ClusterProps extends ComponentPropsWithoutRef<"div"> {
  gap?: Gap;       // default 2
  align?: Align;   // default "center"
  justify?: Justify; // default "start"
  asChild?: boolean;
}
```
CSS: `display:flex; flex-wrap:wrap; gap`.
**JSDoc MUST state:** "Non-interactive visual wrap. For an interactive group
with roving tabindex, use `Toolbar`." (Distinct from the existing `Toolbar`.)

### 4. Container (`src/layouts/Container/`)
```ts
interface ContainerProps extends ComponentPropsWithoutRef<"div"> {
  size?: "sm" | "md" | "lg" | "xl" | "full";  // default "lg"
  padX?: Pad;    // default 4
  center?: boolean;  // default true → margin-inline:auto
  asChild?: boolean;
}
```
CSS: `max-width: var(--zs-container-<size>)` (`full` → `none`),
`margin-inline:auto` when `center`, `padding-inline: spaceVar(padX)`.
**Only** primitive that sets `max-width`.

### 5. Split (`src/layouts/Split/`) — compound
```ts
// <Split side="start" sideWidth="16rem"><Split.Side/><Split.Main/></Split>
interface SplitProps extends ComponentPropsWithoutRef<"div"> {
  side?: "start" | "end";    // default "start"
  sideWidth?: string;         // default "16rem"
  gap?: Gap;
  collapseBelow?: "sm" | "md" | "lg";  // stacks to column below breakpoint
  asChild?: boolean;
}
```
Parts: `Split.Side` (`flex:0 0 var(--split-side-width)`, `data-slot="split-side"`),
`Split.Main` (`flex:1; min-width:0`, `data-slot="split-main"`). Root
`display:flex`; `side="end"` reverses order (use `flex-direction:row-reverse`
or order). `collapseBelow` → `flex-direction:column` under that breakpoint.
Layout-only, no roles.

### 6. Center (`src/layouts/Center/`)
```ts
interface CenterProps extends ComponentPropsWithoutRef<"div"> {
  inline?: boolean;    // center horizontally only (drop vertical centering)
  minHeight?: string;
  asChild?: boolean;
}
```
CSS: `display:flex; align-items:center; justify-content:center`; `inline`
drops vertical centering; `minHeight` applied when set.

---

## Wiring (per primitive)

- `src/layouts/<Name>/index.ts` re-exports component + props type.
- Append `@import "./layouts/<Name>/<Name>.css";` to `src/styles.css` in a new
  "Layout primitives" group after the component imports.
- Export each component + its props type from `src/layouts/index.ts`.
  (`src/index.ts` already does `export * from "./layouts"` — no change there.)

## Stories (`src/stories/<Name>.stories.tsx`)

One file per primitive (matches the existing one-file-per-component
convention). Every variant as a smoke story with a `data-testid`; visualize
arrangement with simple bordered child boxes (use an inline style with a token
`background: var(--zs-fill-secondary)` for demo boxes — demo boxes may use
tokens, never raw hex/px). Add a `play()` ONLY where a prop visibly changes
structure worth asserting (most need none — pure rendering is covered by the
smoke pass per CONVENTIONS.md). Suggested: a Grid story asserting
`display:grid`, a Stack story asserting `flex-direction` flips with
`direction`.

---

## Hard constraints (every file)

- `--zs-*` tokens only. **No raw hex. No raw px** (including comments) — use
  `rem`/`oklch`. Spacing only via `spaceVar(...)` / the `Gap`/`Pad` unions.
- No "HIG"/"Apple" in code, comments, or class names.
- RTL via logical properties (`margin-inline`, `padding-inline`,
  `inset-inline-start`) — never `left`/`right`.
- `@media (forced-colors: active)` + `prefers-reduced-motion` where relevant
  (layout primitives have no motion, so reduced-motion likely N/A; note it).
- `asChild` MUST route through `Slot` from `_slot.ts` (React-19-safe refs),
  never hand-rolled `cloneElement`.
- Pre-launch, no back-compat: no aliases, no shims.

## Verification gates (run and REPORT results; do not commit)

```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build                 # ESM + DTS clean
pnpm --filter @zeroship/ui build-storybook       # static export clean
# Token purity (run in sdks/ui/src):
cd sdks/ui/src
grep -rn '#[0-9a-fA-F]\{3,8\}\b' --include='*.css' --include='*.tsx' --include='*.ts' layouts/   # expect 0
grep -rn '[0-9]\+px' --include='*.css' --include='*.tsx' --include='*.ts' layouts/                # expect 0
```

Report: per-primitive file list, the foundation token diff, build results, the
two grep counts, and any decision you made that the brief didn't cover.
