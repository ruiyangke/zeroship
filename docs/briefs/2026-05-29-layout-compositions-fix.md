# Fix brief: Layout compositions (Wave 2d) — merged code-review fixes

Merges codex (CHANGES-NEEDED) + claude (APPROVE-WITH-NITS). Worktree:
`/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.

> **DO NOT commit, push, or merge.** Implement + self-verify + report.

Severity: 🔴 real bug · 🟡 calibration · 🟢 nit.

## 🔴 1. Formalize `AppShell.Body` and `PageHeader.Text` as the documented contract

`layouts/AppShell/AppShell.tsx` + `layouts/PageHeader/PageHeader.tsx` (+ their
JSDoc; this brief's sibling `2026-05-29-layout-compositions.md` is now updated).

The working implementation REQUIRES `AppShell.Body` (the Split wrapping
Sidebar+Main; carries the flex row + the `data-sidebar-open` collapse gate) and
`PageHeader.Text` (the Stack stacking Breadcrumbs/Title/Description). But the
file-header/JSDoc imply a flat Header/Sidebar/Main/Footer (resp. Breadcrumbs/
Title/Description/Actions) shape that renders BROKEN. Make the wrappers the
explicit, documented contract:
- **AppShell** canonical structure (put in the root JSDoc + a usage block):
  ```
  <AppShell sidebarOpen=… onSidebarOpenChange=…>
    <AppShell.Header>…</AppShell.Header>
    <AppShell.Body>
      <AppShell.Sidebar>…</AppShell.Sidebar>
      <AppShell.Main>…</AppShell.Main>
    </AppShell.Body>
    <AppShell.Footer>…</AppShell.Footer>   {/* optional */}
  </AppShell>
  ```
  `AppShell.Body` is REQUIRED (it owns the sidebar/main row + collapse).
- **PageHeader** canonical structure:
  ```
  <PageHeader>
    <PageHeader.Text>
      <PageHeader.Breadcrumbs/>   {/* optional */}
      <PageHeader.Title>…</PageHeader.Title>
      <PageHeader.Description/>    {/* optional */}
    </PageHeader.Text>
    <PageHeader.Actions/>          {/* optional */}
  </PageHeader>
  ```
  `PageHeader.Text` is REQUIRED to form the text column.
- **Delete the misleading "we do not require a fixed order … React renders them
  in DOM order" comment** (AppShell.tsx ~L232-235) and any "renders directly"
  phrasing; replace with the canonical-structure note. Add a dev-warn is NOT
  required, but the JSDoc on each root MUST show the required wrapper.
- Ensure all stories use the canonical structure (they already use Body/Text —
  confirm; no story should demonstrate the broken flat shape).
- **Regression guard:** keep/ensure an AppShell story in the canonical shape
  whose `play()` asserts the sidebar collapses to out-of-AT-tree when closed
  (already present via SidebarCollapsed/ToggleRoundTrip) and a PageHeader story
  asserting the text column stacks (Title is a block above Description). These
  exercise the documented contract end-to-end.

## 🟡 2. `id` on `AppShell.Main` must not break the skip link

`layouts/AppShell/AppShell.tsx` (`AppShell.Main`, ~L331-338).

The skip link targets `ctx.mainId`, but `AppShell.Main` spreads consumer
`{...rest}` which can override `id`, breaking the link. Make the generated
`ctx.mainId` WIN (apply it after `...rest`), and dev-warn
(NODE_ENV-gated) if the consumer passed an `id` ("AppShell.Main id is managed
for the skip link; the provided id was ignored").

## 🟡 3. Layout primitives honor a consumer `data-slot` (mirror Card)

`layouts/Split/Split.tsx` (root + `.Side` + `.Main`), `layouts/Stack/Stack.tsx`,
`layouts/Cluster/Cluster.tsx`.

Today these hardcode `data-slot` after the prop spread, so composing them
clobbers a composition's semantic slot (e.g. AppShell.Main shows
`data-slot="split-main"` not `app-shell-main`). Mirror the Card fix: add
`"data-slot"?: string` to each props type, destructure
`"data-slot": dataSlot = "<primitive-default>"` (defaults: `stack`/`grid` n/a
here/`cluster`/`split`/`split-side`/`split-main`), and emit `dataSlot`. Defaults
unchanged → the primitives' own stories still pass. Then update
AppShell.Body/Sidebar/Main and PageHeader.Text/Actions to pass their semantic
`data-slot` (`app-shell-body`/`-sidebar`/`-main`, `page-header-text`/`-actions`)
so composed parts own their slot vocabulary. (Grid is not composed by these
compositions; leave Grid as-is unless trivially symmetric — optional.)
**Regression guard:** an AppShell story asserts `AppShell.Main` root has
`data-slot="app-shell-main"` (fails pre-fix → reads `split-main`).

## Won't-fix (deliberate, with rationale)

- **codex 🔴 `useAppShellSidebar()` throws outside its provider.** KEEP the
  throw. A context hook that throws a clear error when used outside its provider
  is the idiomatic React pattern and is exactly what this codebase's `useTheme`
  does (`theme.tsx` throws "useTheme must be used within ThemeProvider"). A
  silent no-op fallback would hide a real consumer bug. Add/keep a clear error
  message ("useAppShellSidebar must be used within an <AppShell>"). Document
  this in the hook's JSDoc. (This is a correct-by-design decision, not a defect.)
- claude 🟢 `data-sidebar-open` on the Sidebar element: harmless hook; the
  collapse keys off Body. Leave (or remove if trivial during item 3).

## Verification (run, REPORT; do not commit)

```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build
pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src
grep -rn '#[0-9a-fA-F]\{3,8\}\b' --include='*.css' --include='*.tsx' layouts/   # 0
grep -rn '[0-9]\+px' --include='*.css' --include='*.tsx' layouts/                # 0 (dvh/% ok)
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6185 --silent &) ; sleep 3
# Re-run AppShell/PageHeader AND the primitives changed in item 3:
for s in AppShell PageHeader Stack Cluster Split; do npx test-storybook --config-dir .storybook --url http://127.0.0.1:6185 --maxWorkers=1 "$s.stories" 2>&1 | grep -E 'Tests:|✕'; done
```

Report: the AppShell/PageHeader JSDoc + canonical-structure changes (+ the
deleted misleading comment), the Main id-skip-link fix, the primitive
data-slot-override diffs + the composed parts now carrying app-shell-*/
page-header-* slots (with the regression assertion), the useAppShellSidebar
keep-throw rationale, build results, grep counts, and ALL 5 suite pass counts
(primitives must stay green after the data-slot change).
