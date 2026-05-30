# Slice brief: Breadcrumbs (follow-on block)

**Worktree:** `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`
**Branch:** `builder/ui-layouts-blocks`. A standalone navigational breadcrumb
trail; rewires `PageHeader.Breadcrumbs` to compose it. Lives in
`sdks/ui/src/blocks/Breadcrumbs/`.

> **DO NOT commit, push, or merge.** Implement + self-verify + report.

## Reference patterns

- `sdks/ui/src/blocks/Tag/Tag.tsx` — chip-style block, focus rings, dev-warns.
- `sdks/ui/src/components/Card/Card.tsx` — dual ergonomic+compound surface,
  compound parts via a namespace, `asChild` via `Slot` + dev-warn, `data-slot`.
- `sdks/ui/src/blocks/_intent.ts` (context — not used here), `_slot.ts`,
  `_classnames.ts`.
- `sdks/ui/src/layouts/PageHeader/PageHeader.tsx` — the `PageHeader.Breadcrumbs`
  part to rewire (currently a bare `<nav aria-label="Breadcrumb"><ol>` wrapper).

## Token facts (live crystal set)

Link text `--zs-label`; link hover/focus `--zs-accent`; current page
`--zs-label-secondary`; separators + ellipsis `--zs-label-tertiary`; focus ring
`--zs-focus-ring-width`/`-color`/`-offset`. Type `--zs-text-footnote-*` or
`--zs-text-subheadline-*`. `--zs-space-*` for gaps. No raw hex/px.

## API (dual surface)

```ts
export interface BreadcrumbItem {
  label: ReactNode;
  href?: string;          // omitted → rendered as plain text (non-link)
  current?: boolean;      // marks the current page (aria-current="page")
}
export interface BreadcrumbsProps extends Omit<ComponentPropsWithoutRef<"nav">, "title"> {
  /** Ergonomic trail. Omit and use compound parts for full control. */
  items?: BreadcrumbItem[];
  /** Separator between crumbs (aria-hidden). Default a chevron "›". */
  separator?: ReactNode;
  /** Collapse the middle into an ellipsis when the trail exceeds this. */
  maxItems?: number;
  /** Accessible label on the wrapping <nav>. Default "Breadcrumb". */
  "aria-label"?: string;
  /** Root data-slot override (default "breadcrumbs") so PageHeader can relabel. */
  "data-slot"?: string;
  children?: ReactNode;     // compound parts (mutually exclusive with items)
}
// Compound: Breadcrumbs.Item (<li>), Breadcrumbs.Link (<a>, asChild),
//           Breadcrumbs.Page (current, <span aria-current="page">),
//           Breadcrumbs.Separator (<li aria-hidden> with the glyph)
```

### Structure & behavior
- Root renders `<nav aria-label="Breadcrumb" data-slot="breadcrumbs"><ol>…</ol></nav>`.
  Honor a consumer `data-slot` (default `"breadcrumbs"`) and `aria-label`.
- **Ergonomic `items`:** map to `<li>` crumbs separated by `Separator` `<li>`s.
  A crumb with `current: true` (or, if none flagged, the LAST item) renders as
  `Breadcrumbs.Page` (`<span aria-current="page">`, not a link). Crumbs with
  `href` render `Breadcrumbs.Link` (`<a href>`); without `href` → plain text.
  `items` and `children` are mutually exclusive — dev-warn if both passed;
  prefer `items`.
- **Compound parts:** `Breadcrumbs.Item` = `<li>`; `Breadcrumbs.Link` = `<a>`
  with `asChild` (route through `Slot` + dev-warn) so consumers pass a router
  `<Link>`; `Breadcrumbs.Page` = the current `<span aria-current="page">`;
  `Breadcrumbs.Separator` = `<li role="presentation" aria-hidden="true">` with
  the separator glyph. (Consumers compose Item/Separator themselves in this
  mode, OR — nicer — the root auto-inserts separators between Item children;
  pick auto-insert between `Breadcrumbs.Item`s and document it. Use
  `Children`/type checks ONLY if needed for auto-insert; if that's fragile,
  require explicit `Breadcrumbs.Separator` and document. Implementer's call —
  state which.)
- **Collapse (`maxItems`):** when the crumb count exceeds `maxItems`, keep the
  FIRST crumb + the last `(maxItems - 1)` crumbs, and replace the middle with an
  ellipsis crumb: a real `<button type="button" aria-expanded={false}
  aria-label="Show N hidden breadcrumbs">…</button>`. Activating it expands the
  full trail INLINE (sets internal state, re-renders all crumbs, `aria-expanded`
  → true). No portal/Menu dependency. Keyboard = native button. (Document that a
  Menu-dropdown variant is a possible future enhancement.)

### a11y
- `<nav aria-label="Breadcrumb">` → `<ol>` → `<li>`s. Current page is
  `aria-current="page"` and NOT a link. Separators `aria-hidden="true"`
  (+`role="presentation"`). Ellipsis is a real focusable `<button>` with an
  accessible label. Links get a visible `:focus-visible` ring
  (`--zs-focus-ring-*`). `forced-colors`: links/current/separators stay legible
  (system colors). `prefers-reduced-motion`: the inline expand has no animation
  (or a token-gated one that disables) — keep it simple.

## Rewire PageHeader.Breadcrumbs

`sdks/ui/src/layouts/PageHeader/PageHeader.tsx`: replace the bare
`PageHeader.Breadcrumbs` (`<nav><ol>`) so it renders the new `Breadcrumbs`,
passing `data-slot="page-header-breadcrumbs"` (the slot it carries today). The
simplest: `PageHeader.Breadcrumbs = (props) => <Breadcrumbs data-slot="page-header-breadcrumbs" {...props} />`
(forwardRef). Keep its displayName "PageHeader.Breadcrumbs". Update the
PageHeader stories that hand-rolled `<a>`/separators inside `.Breadcrumbs` to the
new `items` (or compound) API. Re-run the PageHeader suite. (Pre-launch — no
back-compat alias.)

## Stories (`src/stories/Breadcrumbs.stories.tsx`)

`meta` `parameters: { layout: "fullscreen" }` (catalog convention). Stories:
- **Basic** — `items` with 3-4 crumbs, last is current. axe.
- **Compound** — the parts form (Item/Link/Page/Separator). axe.
- **CustomSeparator** — `separator="/"`.
- **Collapsed** — `items` of ~7 with `maxItems={4}`; **play()**: assert the
  ellipsis `<button>` is present + `aria-expanded="false"`, click it, assert the
  hidden crumbs appear + `aria-expanded="true"`.
- **RouterLink** — `Breadcrumbs.Link asChild` rendering a custom `<a>`-like
  element; assert the tag/href survive.
- **play()** somewhere: current page has `aria-current="page"` and is not an
  `<a>`. `data-testid` on the nav + ellipsis.

## Wiring & constraints

- Export `Breadcrumbs` + `BreadcrumbItem`/`BreadcrumbsProps` + part-prop types
  from `src/blocks/index.ts`. `@import` `Breadcrumbs.css` into `styles.css`
  ("Composed blocks"). `forwardRef`, `displayName`, per-prop JSDoc, `data-slot`
  vocabulary (`breadcrumbs`, `breadcrumbs-item`, `breadcrumbs-link`,
  `breadcrumbs-page`, `breadcrumbs-separator`, `breadcrumbs-ellipsis`).
- `--zs-*` only, no raw hex/px; logical properties; forced-colors +
  reduced-motion where relevant; no "HIG"/"Apple"; pre-launch no-back-compat.

## Verification (run, REPORT; do not commit)

```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build
pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src
grep -rn '#[0-9a-fA-F]\{3,8\}\b' --include='*.css' --include='*.tsx' --include='*.ts' blocks/Breadcrumbs   # 0
grep -rn '[0-9]\+px' --include='*.css' --include='*.tsx' --include='*.ts' blocks/Breadcrumbs                # 0
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6211 --silent &) ; sleep 3
for s in Breadcrumbs PageHeader; do npx test-storybook --config-dir .storybook --url http://127.0.0.1:6211 --maxWorkers=1 "$s.stories" 2>&1 | grep -E 'Tests:|✕'; done
```
NOTE: a Storybook DEV server runs on 6006 — do not use that port; use 6211.

Report: files, the auto-insert-separator decision, the collapse/ellipsis
behavior, the PageHeader rewire (+ updated stories), a11y wiring (nav/ol/li,
aria-current, aria-hidden separators, ellipsis button), build results, grep
counts, and the 2 suite pass counts. Factual handoff.
