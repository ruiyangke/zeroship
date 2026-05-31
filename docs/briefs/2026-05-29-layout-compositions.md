# Slice brief: Layout compositions (Wave 2d) — AppShell, PageHeader

**Worktree:** `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`
**Branch:** `builder/ui-layouts-blocks`. **Plan:** Tasks 7–8. **Spec:** §4b.

Two LAYOUT COMPOSITIONS built FROM the Wave 1 primitives. They live in
`sdks/ui/src/layouts/<Name>/` (layouts, not blocks).

> **DO NOT commit, push, or merge.** Implement + self-verify + report.

## Reference patterns

- `sdks/ui/src/layouts/Split/Split.tsx` — AppShell COMPOSES Split for the
  sidebar/main division; mirror its compound shape + `data-slot`.
- `sdks/ui/src/layouts/{Stack,Cluster,Container}/` — compose for header/footer
  rows and the PageHeader action cluster.
- `sdks/ui/src/components/Card/Card.tsx` — compound parts, `asChild`/`Slot`,
  dev-warn, JSDoc header.
- `sdks/ui/src/components/Drawer/Drawer.tsx` — controlled/uncontrolled
  open-state pattern (controlled `open`+`onOpenChange` with uncontrolled
  fallback) to mirror for AppShell's sidebar.
- `_slot.ts`, `_classnames.ts`.

## Token facts

Surface: `--zs-surface` / `--zs-surface-raised` / `--zs-surface-sunken`;
separators `--zs-separator`; space/radius scales; type tokens for PageHeader
title (`--zs-text-title-1-*` or `--zs-text-large-title-*`) and description
(`--zs-text-subheadline-*` / `--zs-label-secondary`). The theme host is
`<html>` — **AppShell MUST NOT set `data-theme`** (overlays portal to body and
inherit from html).

---

## 1. AppShell (`src/layouts/AppShell/`) — app frame

```ts
// <AppShell><AppShell.Header/><AppShell.Sidebar/><AppShell.Main/><AppShell.Footer/></AppShell>
export type AppShellSidebarSide = "start" | "end";
interface AppShellProps extends ComponentPropsWithoutRef<"div"> {
  sidebarOpen?: boolean;                       // controlled
  defaultSidebarOpen?: boolean;                // uncontrolled (default true)
  onSidebarOpenChange?: (open: boolean) => void;
  sidebarWidth?: string;                       // default "16rem"
  sidebarSide?: AppShellSidebarSide;           // default "start"
}
// Parts: AppShell.Header, AppShell.Sidebar, AppShell.Main, AppShell.Footer
```
- **Structure:** a full-height column — `Header` (top band, `--zs-surface-raised`
  + bottom separator), then a body row that is a `Split` (`Sidebar` rail +
  `Main` fluid), then optional `Footer`. Use the `Split` primitive for the
  body. `min-block-size: 100%` / `100dvh` on the root so it fills the viewport
  region (document the dvh choice; dvh is allowed, it's not px).
- **Sidebar collapse:** controlled/uncontrolled state (mirror Drawer's pattern —
  internal `useState(defaultSidebarOpen ?? true)` unless `sidebarOpen` is
  provided; `onSidebarOpenChange` fires on toggle). When closed, the Sidebar is
  removed from layout (width 0 / `display:none`) and Main spans full width.
  Expose the toggle as the consumer's responsibility (a trigger lives in their
  Header content); ALSO accept `data-state` styling hooks. Provide NO built-in
  hamburger button (consumer composes it and calls the setter) — but DO expose
  the open state + setter, e.g. via a `useAppShellSidebar()` context hook or by
  the consumer driving `sidebarOpen` from outside. Pick the controlled-prop
  approach (simplest, matches Drawer): the consumer owns the trigger and passes
  `sidebarOpen`/`onSidebarOpenChange`. Document this.
- **a11y landmarks:** `Header`→`<header>`, `Sidebar`→`<aside>` by default (allow
  `asChild` to render `<nav>` when it's primary nav), `Main`→`<main>`,
  `Footer`→`<footer>`. **Bake in a skip-to-content link** as the first focusable
  child of the shell: a `.zs-skip-link` anchor (visually-hidden until
  `:focus`/`:focus-visible`, then visible) targeting the Main's id (generate an
  id with `useId`, wire `href="#id"` + `id` on Main). Add the `.zs-skip-link`
  CSS to styles.css base rules.
- `forwardRef`, `data-slot="app-shell"` (+ `app-shell-header/-sidebar/-main/
  -footer`), per-prop JSDoc. Do NOT set `data-theme` anywhere.
- **Stories:** full shell (header + sidebar + main + footer); sidebar-collapsed
  (controlled `sidebarOpen={false}`); **play()**: render with a toggle button
  wired to state, click it, assert the sidebar shows/hides (visibility or
  presence) and `onSidebarOpenChange` fires; assert exactly one `<main>` and
  that the skip link's href targets it. (Avoid duplicate-`<main>`: the shell's
  Main is the only one.)

## 2. PageHeader (`src/layouts/PageHeader/`) — page title band

```ts
// <PageHeader><PageHeader.Breadcrumbs/><PageHeader.Title/><PageHeader.Description/><PageHeader.Actions/></PageHeader>
interface PageHeaderProps extends ComponentPropsWithoutRef<"div"> {
  asChild?: boolean;
}
// Parts: PageHeader.Breadcrumbs, .Title, .Description, .Actions
```
- Layout: a row with a text column (Breadcrumbs → Title → Description stacked)
  on the start side and `.Actions` (a right-aligned `Cluster`) on the end;
  wraps gracefully on narrow widths. Compose `Stack`/`Cluster`.
- `.Title` is an `<h1>` by default; `asChild` to relevel to match the document
  outline. `.Breadcrumbs` is `<nav aria-label="Breadcrumb">` wrapping an ordered
  list (`<ol>`); document that consumers put `<a>`/separators inside.
  `.Description` is a muted `<p>`.
- `forwardRef`, `data-slot="page-header"` (+ parts), per-prop JSDoc, asChild via
  Slot + dev-warn on the root and Title.
- **Stories:** full (breadcrumbs + title + description + actions); minimal
  (title only); assert the heading is present and Actions cluster to the end.

---

## Wiring & constraints

- Export both (+ types/parts) from `src/layouts/index.ts`. `@import` both CSS
  into `styles.css` (extend the "Layout primitives" group, or a new
  "Layout compositions" group). Add `.zs-skip-link` to the base rules.
- `--zs-*` only; no raw hex/px (dvh/% allowed); logical properties;
  `prefers-reduced-motion` (sidebar transition) + `forced-colors` (header/
  separators visible). No "HIG"/"Apple". Pre-launch no-back-compat.

## Verification (run, REPORT; do not commit)

```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build
pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src
grep -rn '#[0-9a-fA-F]\{3,8\}\b' --include='*.css' --include='*.tsx' --include='*.ts' layouts/AppShell layouts/PageHeader   # 0
grep -rn '[0-9]\+px' --include='*.css' --include='*.tsx' --include='*.ts' layouts/AppShell layouts/PageHeader                # 0 (dvh/% ok)
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6181 --silent &) ; sleep 3
for s in AppShell PageHeader; do npx test-storybook --config-dir .storybook --url http://127.0.0.1:6181 --maxWorkers=1 $s.stories 2>&1 | grep -E 'Tests:|✕'; done
```

Report: per-composition files, the sidebar controlled/uncontrolled approach
chosen, the skip-link wiring, build results, grep counts, the 2 suites' pass
counts, and any decision the brief didn't cover (esp. AppShell sidebar toggle
ownership + landmark roles).
