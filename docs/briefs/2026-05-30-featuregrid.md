# Slice brief: FeatureGrid section

Third piece of the `sections/` layer (after Hero, PricingTable). A marketing
"features" band: an optional header lead-in (eyebrow + title + description) above a
responsive grid of feature items, each = an Icon (Lucide) in a tinted badge + a
title + a short description. Composes Container/Grid/Stack/Icon (+ Card optional).
Lives in `sdks/ui/src/sections/FeatureGrid/`. Visible marketing section → visual
review (codex screenshot) + self-verify (lighter than a full dual code review;
mirror the now-established sections/ patterns from Hero/PricingTable).

WORKTREE: `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.
Run ALL commands with cwd INSIDE it (nested under main repo; wrong cwd builds the
WRONG checkout — `cd` in first).
> **DO NOT commit, push, or merge.** Implement + self-verify + report.

## Reference / compose (READ first — mirror exactly)
- `sections/Hero/Hero.tsx` and `sections/PricingTable/PricingTable.tsx` — the
  sections/-layer template: dual surface (props + compound parts), the recursive
  `flattenChildren` Fragment-descending walk, root data-slot override, the
  title-gated `aria-labelledby` (NO dangling label when no header), the JSDoc
  density, the `--zs-bp-md` (47.999rem) collapse. Mirror these.
- `components/Icon/Icon.tsx` — features use `<Icon as={LucideGlyph} size="lg">`
  (consumer passes the glyph); decorative (no label) — the title carries meaning.
- `layouts/{Container,Grid,Stack,Cluster}` — Grid for the item row (responsive,
  collapses below `--zs-bp-md`); Stack for header + each item's vertical rhythm.
- `_classnames`, `_slot`. House: forwardRef, data-slot, per-prop JSDoc.

## API (dual surface — props is the ergonomic 80%, compound for control)
```ts
export interface FeatureItem {
  id?: string;                 // stable key (else index)
  icon?: ReactNode;            // an <Icon> (consumer passes the Lucide glyph) — decorative
  title: ReactNode;            // feature name — renders as a heading (h3 default)
  description?: ReactNode;     // short supporting line
}
export interface FeatureGridProps
  extends Omit<ComponentPropsWithoutRef<"section">, "title"> {
  /** Optional small label above the section title (eyebrow). */
  eyebrow?: ReactNode;
  /** Optional section title lead-in (renders as <h2>; the section is
   *  aria-labelledby it ONLY when it renders — no dangling label otherwise). */
  title?: ReactNode;
  /** Optional supporting paragraph under the title. */
  description?: ReactNode;
  /** Ergonomic mode: the feature items. */
  features?: FeatureItem[];
  /** Column count at wide widths (collapses to 1 below --zs-bp-md). Default 3. */
  columns?: 2 | 3 | 4;
  /** Header + item text alignment. "center" (default) | "start". */
  align?: "center" | "start";
  /** Container width. Default "lg". */
  size?: ContainerSize;
  /** Compound FeatureGrid.Item parts (additive with `features`). */
  children?: ReactNode;
  "data-slot"?: string;
}
// Compound: FeatureGrid.Item (props: icon/title/description) — additive marker
//   parts read by the root via the recursive flattenChildren walk (like
//   PricingTable.Tier). Dev-warn if rendered standalone.
```
**ADDITIVE** (the sections/ rule): `features` prop items render first, then any
compound `<FeatureGrid.Item>` children fall through after — no suppression; use the
recursive Fragment-descending `flattenChildren` walk.

## Layout / structure
- Root `<section data-slot="feature-grid">` → `Container` → optional header
  (`Stack`: eyebrow → `<h2>` title → description, aligned per `align`, on a
  readable measure) → a `Grid` of feature items.
- Grid: `columns={{ md: columns }}` collapsing to 1 column below `47.999rem`
  (match Hero/Split/PricingTable). Equal-height items via `align="stretch"` if you
  wrap items in Cards; otherwise simple cells.
- Each item (`data-slot="feature-grid-item"`): the `icon` in a tinted square/circle
  badge (accent-tint surface, token-pure) → title `<h3>` → description (muted).
  `align="center"` centers icon+text; `align="start"` start-aligns.
- The band paints no heavy background by default (composable); generous vertical
  padding via `--zs-space-*` large steps.

## a11y
- Section header title is an `<h2>` (the band sits under a page `<h1>`); the
  `<section>` is `aria-labelledby` it ONLY when the title renders (gate the attr on
  a real heading — the Hero R6 / PricingTable lesson; NO dangling label when the
  band is used headerless). Feature titles are real `<h3>`s. Icons are decorative
  (aria-hidden via Icon's no-label path). forced-colors: text + icon badges stay
  legible; reduced-motion parity block present.

## Constraints
`--zs-*` only; no raw hex/px (rem/%/dvh/oklch); logical properties; forced-colors +
prefers-reduced-motion blocks (BOTH required); no "HIG"/"Apple"; pre-launch
no-back-compat (no shims/aliases). forwardRef + data-slot vocab (feature-grid /
feature-grid-header / feature-grid-title / feature-grid-items / feature-grid-item /
feature-grid-item-icon / feature-grid-item-title / feature-grid-item-description —
document the set). Export from `sections/index.ts`; `@import` CSS into styles.css
under the existing "Page sections" group; story `title: "Sections/FeatureGrid"`,
`layout: "fullscreen"`.

## Stories (`src/stories/FeatureGrid.stories.tsx`)
- **ThreeUp** (header eyebrow+title+description + 3 features, each Lucide icon
  +title+description, centered).
- **FourColumns** (columns=4, 4–8 features).
- **StartAligned** (align="start", no eyebrow).
- **Compound** (the same via `<FeatureGrid.Item>` parts + additive fall-through —
  one prop feature then compound items; assert heading order).
- **Headerless** (no eyebrow/title/description — assert the `<section>` has NO
  aria-labelledby; just the item grid).
- **play()**: assert feature titles are `<h3>` headings; the icon is decorative
  (aria-hidden, no accessible name leaking); Headerless has no aria-labelledby;
  Compound renders prop + compound items in order. axe clean on all.

## Verify (run, REPORT; do not commit)
```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build && pnpm --filter @zeroship/ui build-storybook
node -e "const x=require('./sdks/ui/dist/index.js'); console.log('FeatureGrid export:', !!x.FeatureGrid)"
cd sdks/ui/src && echo "hex/px: $(grep -rEn '#[0-9a-fA-F]{3,8}\b' --include='*.css' --include='*.tsx' sections/FeatureGrid|wc -l)/$(grep -rEn '[0-9]+px' --include='*.css' --include='*.tsx' sections/FeatureGrid|wc -l)"
grep -c 'prefers-reduced-motion' sections/FeatureGrid/FeatureGrid.css
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
for p in 6362; do fuser -k $p/tcp 2>/dev/null; done; sleep 1
(npx http-server storybook-static -p 6362 --silent &) ; sleep 3
npx test-storybook --config-dir .storybook --url http://127.0.0.1:6362 --maxWorkers=2 FeatureGrid.stories 2>&1 | grep -E 'Tests:|Test Suites:|✕'
fuser -k 6362/tcp 2>/dev/null
```
Report: files, the dual-surface + additive Fragment-descending walk, the responsive
collapse, the title-gated/no-dangling-label decision, the icon-badge treatment,
build/grep/suite counts, decisions. Flagship visual section — make it polished
(orchestrator will screenshot-review).
