# Redesign brief: AppShell — refined glass operations console

The current AppShell reads bare/ugly: the sidebar shares Main's surface (no rail
differentiation), the header is a thin undifferentiated band, and the demo is
plain text ("Acme" / 3 unstyled links / "Welcome back"). Redesign for a
polished, production-grade app frame **within the crystal token system** (this
is a governed design system — `--zs-*` tokens only, no raw hex/px, no "HIG"/
"Apple" naming). Worktree:
`/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.

> **DO NOT commit, push, or merge.** Implement + self-verify + report.
> Keep the AppShell **API unchanged** (parts, props, controlled sidebar, skip
> link, data-slots) — this is a visual redesign of the default CSS + a rebuilt
> showcase demo. All existing `play()` assertions MUST still pass.

## Aesthetic direction

A calm, refined "operations console" in the crystal language: a glass **raised
header bar**, a subtly **sunken material sidebar rail** with grouped navigation,
and a quiet **canvas Main**. Surface hierarchy (raised header > canvas main >
sunken rail), generous-but-disciplined spacing rhythm on the `--zs-space-*`
scale, hairline separators, one restrained accent (`--zs-accent`) reserved for
the active nav item + primary actions. Precision over decoration — elegance
from spacing, type hierarchy, and surface layering, not ornament.

## Part A — component default CSS (`layouts/AppShell/AppShell.css`)

Improve the DEFAULT visual treatment so the shell looks polished out of the box:

1. **Sidebar rail differentiation (the main fix).** Give `.zs-app-shell__sidebar`
   a distinct surface: `background: var(--zs-surface-sunken)` and an
   **inline hairline on the edge facing Main** — `border-inline-end` when the
   rail is on the start side, `border-inline-start` when `sidebarSide="end"`
   (key off the body's `[data-side]`/the shell's `[data-sidebar-side]`). Keep
   the existing padding + overflow + collapse transition. Add the matching
   `forced-colors` border mirror.
2. **Header bar polish.** Keep `--zs-surface-raised` + bottom hairline. Give it a
   consistent comfortable bar height (e.g. a `min-block-size` via a space token)
   and vertically-center its content (`display:flex; align-items:center`), so a
   brand lockup + actions sit on a real bar. Keep padding token-based.
3. **Footer polish.** Muted footnote treatment (`--zs-label-secondary`,
   `--zs-text-footnote-*`), top hairline, vertically centered.
4. **Main stays arrange-only** (no forced padding — consumers may want
   full-bleed). Document the "wrap Main content in `Container`/your own padding"
   pattern in the JSDoc.
Keep logical properties, `prefers-reduced-motion`, and the `forced-colors`
block (extend it for the new rail border). No raw hex/px; `dvh`/% allowed.

## Part B — rebuild the showcase demo (`stories/AppShell.stories.tsx`, FullShell)

Rebuild `FullShell` into a realistic, polished console that **dogfoods the
catalog** (this is both the showcase and a composition proof). Import from
`../layouts` and `../components` and the blocks as needed:

- **Header:** a brand lockup on the start (a small mark — an inline `aria-hidden`
  SVG glyph in an accent-tinted rounded square + the wordmark "Acme" in a
  display/headline weight), a flexible spacer (`Stack`/`Cluster` + `flex:1`),
  then an end cluster: a subtle "New project" `Button` + an `Avatar`
  (initials/fallback). Vertically centered on the bar.
- **Sidebar:** a `<nav aria-label="Primary">` with TWO grouped sections (e.g.
  "Workspace" → Dashboard / Projects / Deployments / Activity; "Account" →
  Settings / Billing), each group with a small uppercase muted section label
  (`--zs-label-tertiary`, `--zs-text-caption-*`, letter-spacing). Each nav item:
  a styled link = inline `aria-hidden` SVG icon + label, with hover
  (`--zs-fill-quaternary` bg) and **active** state (one item, e.g. Dashboard:
  `aria-current="page"`, accent-tinted bg via `color-mix(... --zs-accent ...)`
  + accent text + a leading accent rail/indicator). Rounded (`--zs-radius-2`),
  comfortable hit target, `:focus-visible` ring via `--zs-focus-ring-*`. These
  nav-item styles live in the story's CSS-in-JS/`<style>` or inline tokens
  (the AppShell stays structural) — but make them genuinely polished; they
  double as the recipe consumers copy.
- **Main:** wrap content in a `Container size="lg"` (or padded) and compose:
  a `PageHeader` (Breadcrumbs Home › Dashboard + Title "Dashboard" + Description
  + an Actions `Button`), then a `Grid` of 3–4 `StatCard`s (e.g. Revenue / Active
  users / Deploys, with up/down deltas), then a `Card` with a short
  "Recent activity" list (a `DescriptionList` or a few rows). Real, calm content
  — no lorem walls.
- **Footer:** muted "© Acme" + a couple of small footnote links, vertically
  centered.

Keep the other stories (`SidebarCollapsed`, `ToggleRoundTrip`, `SidebarEnd`)
functional — they can stay simpler, but update their nav to the new styled
item recipe so the catalog reads consistently. **All existing `play()`
assertions must still pass** (single shell `<main>`, skip link → main id,
`data-slot="app-shell-main"`/`-body`/`-sidebar`, toggle round-trip, side=end).

## a11y (must hold)

`<header>`/`<nav aria-label>`/`<main>`/`<footer>` landmarks; the active nav item
`aria-current="page"`; all decorative icons/mark `aria-hidden`; the brand mark
SVG `aria-hidden` (wordmark conveys); avatar has an accessible name; nav items
keyboard-focusable with visible focus rings; the baked-in skip link still works.
The AppShell story's two disabled landmark axe rules (duplicate-main /
main-is-top-level) stay disabled WITH the existing justifying comment — do not
introduce NEW axe violations (run the suite).

## Verification (run, REPORT; do not commit)

```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build
pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src
grep -rn '#[0-9a-fA-F]\{3,8\}\b' --include='*.css' --include='*.tsx' layouts/AppShell stories/AppShell.stories.tsx   # 0
grep -rn '[0-9]\+px' --include='*.css' --include='*.tsx' layouts/AppShell stories/AppShell.stories.tsx                # 0 (dvh/% ok)
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6225 --silent &) ; sleep 3
npx test-storybook --config-dir .storybook --url http://127.0.0.1:6225 --maxWorkers=1 AppShell.stories 2>&1 | grep -E 'Tests:|✕'
```
NOTE: a Storybook DEV server runs on :6006 — DO NOT use that port; use 6225.

Report: the rail/header/footer CSS changes, the rebuilt demo composition (what
catalog components it dogfoods + the nav-item active/hover recipe), confirmation
all AppShell `play()`s pass + no new axe violations, build results, grep counts,
suite pass count, and any decision the brief didn't cover. This is a VISUAL
redesign — the orchestrator will do a screenshot visual review, so make the
FullShell genuinely polished.
