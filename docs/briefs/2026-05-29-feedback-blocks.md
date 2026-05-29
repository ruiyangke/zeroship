# Slice brief: Feedback blocks (Wave 2a)

**Worktree:** `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`
**Branch:** `builder/ui-layouts-blocks`.
**Plan:** `docs/superpowers/plans/2026-05-29-ui-layouts-blocks.md` (Tasks 9–12).
**Spec:** `docs/superpowers/specs/2026-05-29-ui-layouts-blocks-design.md` (§4c).

Four cohesive composed BLOCKS for the empty/error/loading state family:
`EmptyState`, `ErrorState`, `Skeleton`, `Spinner`. They live in
`sdks/ui/src/blocks/<Name>/`.

> **DO NOT commit, push, or merge.** Implement + self-verify + report only.

---

## Reference patterns (read first)

- `sdks/ui/src/components/Card/Card.tsx` — compound parts + `asChild` via `Slot`,
  `data-slot`, dev-warn, JSDoc header style. Mirror this house style.
- `sdks/ui/src/components/Button/Button.{tsx,css}` — inline `Spinner` SVG with
  `aria-hidden`, `prefers-reduced-motion` handling, `role`/`aria-busy` pattern.
- `sdks/ui/src/components/Meter/Meter.{tsx,css}` and `Progress/Progress.css` —
  reduced-motion precedent (animation paint preserved, motion disabled).
- `sdks/ui/src/layouts/{Center,Stack}/` — COMPOSE these (dogfood Wave 1):
  EmptyState/ErrorState center their content with `Center` and stack with `Stack`.
- `sdks/ui/src/components/_slot.ts`, `_classnames.ts` — `Slot`, `classnames`.

Import path from a block (`src/blocks/<Name>/<Name>.tsx`):
`import { Slot } from "../../components/_slot";`
`import { classnames } from "../../components/_classnames";`
`import { Center } from "../../layouts/Center";` / `{ Stack }` as needed.

## Token facts (LIVE crystal set — use these exactly)

- Text: `--zs-label` (primary), `--zs-label-secondary` (muted body),
  `--zs-label-tertiary` (faint). Type scale: `--zs-text-title-3-*` /
  `--zs-text-headline-*` for titles, `--zs-text-subheadline-*` /
  `--zs-text-footnote-*` for descriptions.
- Intent colors (no `--zs-state-*` exists; map to these):
  - `error`/`danger` → `--zs-system-red`
  - `warning` → `--zs-system-orange`
  - `success` → `--zs-system-green`
  - `info` → `--zs-accent`
- Skeleton placeholder fill: `--zs-fill-secondary` (base) shimmering to
  `--zs-fill-tertiary` (or `--zs-fill`); pick within the fill family only.
- Spacing/radius: `--zs-space-*`, `--zs-radius-*` only. No raw hex/px.

## New shared foundation utility (add to styles.css foundation block)

No visually-hidden utility exists. Add ONE, used by Spinner's label (and
future blocks). Standard clip pattern, in the `:root`-adjacent base rules
(NOT inside `:root` — it's a class):

```css
/* Visually hidden but available to assistive tech (sr-only). */
.zs-visually-hidden {
  position: absolute;
  width: 1px;
  height: 1px;
  padding: 0;
  margin: -1px;
  overflow: hidden;
  clip: rect(0, 0, 0, 0);
  white-space: nowrap;
  border: 0;
}
```
(`1px` here is the canonical sr-only idiom; it is the single allowed px in the
codebase for this utility — document it in a comment. Do not introduce other px.)

---

## The four blocks

All: `forwardRef`, `data-slot="<name>"`, JSDoc on every prop. Per-component CSS
`@import`ed into `styles.css` under a new "Blocks" group. Export from
`src/blocks/index.ts` (which `src/index.ts` already re-exports).

### 1. EmptyState (`src/blocks/EmptyState/`)
Ergonomic props AND compound parts (mirror Card's dual surface):
```ts
interface EmptyStateProps extends ComponentPropsWithoutRef<"div"> {
  icon?: ReactNode;          // decorative; wrapped aria-hidden
  title?: ReactNode;         // ergonomic; rendered as the heading
  description?: ReactNode;
  action?: ReactNode;        // e.g. a <Button>
  asChild?: boolean;
}
// Compound: EmptyState.Icon / .Title / .Description / .Actions
```
- Layout: `Center` (or centered column `Stack`) — icon, title, description,
  actions stacked, centered, generous `--zs-space-*` padding.
- a11y: Title renders as a heading — `<h2>` default, `asChild` to relevel.
  Container is a plain region (NO `role="alert"` — empty ≠ error). Icon wrapper
  `aria-hidden="true"`. If both `title` prop and `.Title` child are used, prefer
  children (document precedence).
- Story: default (icon+title+desc+action), title-only, compound form. No play().

### 2. ErrorState (`src/blocks/ErrorState/`)
```ts
interface ErrorStateProps extends ComponentPropsWithoutRef<"div"> {
  intent?: "error" | "warning";   // default "error" → red / orange icon accent
  title?: ReactNode;
  description?: ReactNode;
  onRetry?: () => void;           // when set, render a retry Button
  live?: boolean;                 // default false; true → role="alert"
  asChild?: boolean;
}
// Compound: ErrorState.Title / .Description / .Actions
```
- Visual: same centered layout as EmptyState; icon tinted by `intent`
  (`--zs-system-red` / `--zs-system-orange`).
- Retry: when `onRetry` set, render `<Button>Retry</Button>` (import the real
  `Button`) wired to `onRetry`.
- a11y: `role="alert"` ONLY when `live` (a statically-rendered error page must
  not assert alert — document this). Heading semantics like EmptyState.
- Story + **play()**: a story with `onRetry` — click Retry, assert the handler
  fires (use a `fn()` spy from `@storybook/test`). A `live` story. axe each.

### 3. Skeleton (`src/blocks/Skeleton/`)
```ts
interface SkeletonProps extends ComponentPropsWithoutRef<"div"> {
  variant?: "text" | "rect" | "circle";  // default "text"
  lines?: number;                         // text variant only; default 1
  width?: string;                         // CSS length
  height?: string;                        // CSS length
}
```
- CSS: placeholder painted with `--zs-fill-secondary`; a shimmer animation
  (translate a gradient OR pulse opacity) using `--zs-fill-tertiary`.
  **`@media (prefers-reduced-motion: reduce)` disables the shimmer animation**
  (the static fill remains visible — paint preserved). Mirror Progress/Meter.
- `text` with `lines>1` renders N stacked bars (last bar shorter). `circle` is
  `border-radius: var(--zs-radius-full)` and square. `rect` uses `--zs-radius-2`.
- a11y: `aria-hidden="true"` (decorative; the page's status/Spinner conveys
  loading). No role.
- Story: each variant, multi-line text, a **reduced-motion story** asserting the
  animation is disabled (parameter or media emulation — match how Progress's
  reduced-motion story is written). No play() beyond the reduced-motion check.

### 4. Spinner (`src/blocks/Spinner/`)
```ts
interface SpinnerProps extends ComponentPropsWithoutRef<"span"> {
  size?: "sm" | "md" | "lg";   // default "md"
  label?: string;              // default "Loading"; rendered visually-hidden
}
```
- CSS: rotating ring/arc (border or SVG), sized by `size`. Rotation animation;
  `@media (prefers-reduced-motion: reduce)` → stop/slow the spin (document the
  choice; a non-animated busy indicator is acceptable).
- a11y: root `role="status"`; render `label` inside a
  `<span className="zs-visually-hidden">` so SRs announce it. The visual
  spinner element is `aria-hidden="true"`.
- Story: sizes; assert `role="status"` + accessible name (`getByRole("status",
  { name: /loading/i })`). reduced-motion story.

---

## Hard constraints (every file)

- `--zs-*` tokens only. No raw hex. No raw px EXCEPT the single documented
  `.zs-visually-hidden` clip idiom. Use `rem`/`oklch` elsewhere.
- No "HIG"/"Apple" in code/comments/class names.
- RTL via logical properties (`padding-inline`, `margin-inline`,
  `inset-inline-start`). `prefers-reduced-motion` + `@media (forced-colors:
  active)` where relevant (Skeleton/Spinner DEFINITELY; in forced-colors the
  skeleton/spinner must remain visible via system colors).
- `asChild` (EmptyState/ErrorState) routes through `Slot`; dev-warn on invalid
  child (mirror the Wave 1 layout primitives + Card).
- Compose Wave 1 primitives (`Center`/`Stack`) instead of re-rolling flexbox.
- Pre-launch, no back-compat.

## Stories & gate

One story file per block in `src/stories/`. Every variant a smoke story with a
`data-testid`; `play()` where behavior beats markup (ErrorState retry, Spinner
role/name); reduced-motion stories for Skeleton + Spinner; axe-clean (NO raw
`<main>` in demos — use plain content). Stories must pass the Test Runner:
`test-storybook ... <Name>.stories` (positional regex, not -t).

## Verification (run and REPORT; do not commit)

```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build
pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src
grep -rn '#[0-9a-fA-F]\{3,8\}\b' --include='*.css' --include='*.tsx' --include='*.ts' blocks/   # expect 0
grep -rn '[0-9]\+px' --include='*.css' --include='*.tsx' --include='*.ts' blocks/                # expect 0 (the px idiom is in styles.css, not blocks/)
# Then boot http-server on a FREE port and run the 4 suites:
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6131 --silent &) ; sleep 3
for s in EmptyState ErrorState Skeleton Spinner; do npx test-storybook --config-dir .storybook --url http://127.0.0.1:6131 --maxWorkers=1 $s.stories 2>&1 | grep -E 'Tests:|✕'; done
```

Report: per-block files, the `.zs-visually-hidden` + any token additions, build
results, the two grep counts, the 4 suites' pass/fail, the intent→token mapping
you used, and any decision the brief didn't cover.
