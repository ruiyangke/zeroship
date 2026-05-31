# Slice brief: DataTable (Wave 3)

**Worktree:** `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`
**Branch:** `builder/ui-layouts-blocks`. **Plan:** Task 18. **Spec:** §4c + §5 Wave 3.

The long pole: a **presentational** data table (the long-promised `Table`).
Lives in `sdks/ui/src/blocks/DataTable/`. **The consumer owns the data and all
sorting/selection LOGIC — DataTable renders state and emits intent.**

> **DO NOT commit, push, or merge.** Implement + self-verify + report.

## Reference patterns

- `sdks/ui/src/components/Checkbox/Checkbox.tsx` — the selection column + the
  select-all header checkbox (incl. its `indeterminate` state).
- `sdks/ui/src/blocks/EmptyState/EmptyState.tsx` — the default empty slot.
- `sdks/ui/src/blocks/Skeleton/Skeleton.tsx` — loading rows.
- `sdks/ui/src/components/Card/Card.tsx` — house style (forwardRef, data-slot,
  JSDoc header, dev-warn).
- `_classnames.ts`. (DataTable root is a `<table>` — no `asChild`/`Slot`.)

## Token facts

Header bg `--zs-fill-secondary` / `--zs-surface-raised`; row separators
`--zs-separator`; text `--zs-label` / header `--zs-label-secondary`; selected
row tint `color-mix(in oklab, --zs-accent N%, --zs-surface)` (opaque, glass
rule); hover row `--zs-fill-quaternary`. Sticky header uses the same opaque
header bg. No raw hex/px.

## API (generic over the row type T)

```ts
export interface DataTableColumn<T> {
  key: string;
  header: ReactNode;
  cell: (row: T) => ReactNode;
  sortable?: boolean;
  align?: "start" | "center" | "end";
  width?: string;            // CSS length (col width hint)
}
export interface DataTableSort { key: string; direction: "asc" | "desc"; }
export type DataTableSelectionMode = "none" | "single" | "multiple";
export interface DataTableProps<T> {
  columns: DataTableColumn<T>[];
  data: T[];
  rowKey: (row: T) => string;
  caption?: ReactNode;       // <caption>; if absent, aria-label required
  "aria-label"?: string;     // required when no caption (dev-warn if neither)
  sort?: DataTableSort | null;
  onSortChange?: (sort: DataTableSort) => void;
  selection?: DataTableSelectionMode;        // default "none"
  selectedKeys?: string[];                    // controlled selection
  onSelectionChange?: (keys: string[]) => void;
  stickyHeader?: boolean;
  density?: "comfortable" | "compact";        // default "comfortable"
  loading?: boolean;
  loadingRowCount?: number;                   // default 5 (skeleton rows)
  renderEmpty?: () => ReactNode;              // default <EmptyState/>
  renderError?: () => ReactNode;              // when provided + truthy, shown instead of rows
}
```
`forwardRef<HTMLTableElement>`, `data-slot="data-table"` (+ data-slots on
thead/tbody/row/cell). Per-prop JSDoc.

## Settled design decisions (from the plan — implement these; document in JSDoc)

1. **Sortable headers.** A `sortable` column renders its `header` inside a real
   `<button type="button">` within `<th scope="col" aria-sort=…>`. `aria-sort`
   is `"ascending"`/`"descending"` on the active column, `"none"` on other
   sortable columns, absent on non-sortable. Clicking emits `onSortChange`
   toggling direction (asc→desc; a new column starts `asc`). Keyboard = native
   button. A visible sort affordance (▲/▼, `aria-hidden`) reflects state.
2. **Selection.** Opt-in via `selection`. A leading checkbox column (uses the
   real `Checkbox`): per-row checkbox toggles that row's key in
   `onSelectionChange`; rows carry `aria-selected` when selected. For
   `"multiple"`, a header select-all `Checkbox` selects/clears all visible row
   keys and shows the **indeterminate** state when some-but-not-all are
   selected. For `"single"`, no header checkbox (single select). `selectedKeys`
   is controlled.
3. **Sticky header.** `stickyHeader` → `<thead>`/`<th>` `position: sticky;
   inset-block-start: 0` with the opaque header bg (so rows scroll under it).
   The table sits in a scroll container the consumer sizes.
4. **Density.** `comfortable` (default) vs `compact` change cell padding tokens
   only.
5. **State slots.** When `loading` → render `loadingRowCount` Skeleton rows
   (text variant) in the tbody (header still shown). When `data` is empty (and
   not loading) → render `renderEmpty?.() ?? <EmptyState title="No data" …/>` in
   a full-width row. When `renderError` is provided and returns truthy → render
   it in a full-width row instead of data. Document precedence:
   error > loading > empty > data.
6. **Virtualization: DEFERRED.** Do NOT add a windowing dependency. Add a JSDoc
   note on the component: "Renders all rows; for very large datasets paginate
   or virtualize upstream — windowing is a future slice."

## a11y

- Real semantic `<table><caption?><thead><tr><th scope="col">…</thead>
  <tbody><tr><td>…</tbody></table>`.
- `aria-sort` on sortable headers (see #1). Selected rows `aria-selected="true"`.
- Caption OR `aria-label` is REQUIRED — dev-warn (NODE_ENV-gated) if neither.
- The select-all checkbox has an accessible label ("Select all rows"); per-row
  checkboxes labelled by their row (e.g. `aria-label` from a derived key or a
  caption-relative label) — document how the consumer supplies row labels (a
  sensible default: `Select row {n}`).

## Stories (`src/stories/DataTable.stories.tsx`)

- **Basic** (populated, caption, a few columns) — smoke + axe.
- **Sortable** — `play()`: click a sortable header, assert `onSortChange` fired
  with toggled direction AND the header's `aria-sort` flips.
- **SelectableMultiple** — `play()`: click the header select-all, assert
  `onSelectionChange` fired with ALL row keys; with a partial `selectedKeys`,
  assert the header checkbox is `indeterminate`; a row checkbox toggles its key.
- **Empty** — empty `data`, asserts the EmptyState renders (axe).
- **Loading** — `loading`, asserts Skeleton rows render (axe; skeletons
  aria-hidden, so the table still needs its accessible name).
- **StickyHeader** + **Density (compact)** — smoke.
Drive controlled `sort`/`selectedKeys` via a story render wrapper (useState) so
the play() round-trips are real. `data-testid` on the table + key controls.

## Wiring & constraints

- Export `DataTable` + types from `src/blocks/index.ts`; `@import`
  `DataTable.css` into `styles.css` ("Composed blocks"). Compose the real
  `Checkbox`/`EmptyState`/`Skeleton` (don't re-roll). `--zs-*` only, no raw
  hex/px; logical properties; `prefers-reduced-motion` (row hover) +
  `forced-colors` (borders, selected row, sort affordance visible). No "HIG"/
  "Apple". Pre-launch no-back-compat.

## Verification (run, REPORT; do not commit)

```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build
pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src
grep -rn '#[0-9a-fA-F]\{3,8\}\b' --include='*.css' --include='*.tsx' --include='*.ts' blocks/DataTable   # 0
grep -rn '[0-9]\+px' --include='*.css' --include='*.tsx' --include='*.ts' blocks/DataTable                # 0
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6191 --silent &) ; sleep 3
npx test-storybook --config-dir .storybook --url http://127.0.0.1:6191 --maxWorkers=1 DataTable.stories 2>&1 | grep -E 'Tests:|✕'
```

Report: files, the sort/selection/sticky/density implementation choices, the
state-slot precedence, the a11y wiring (aria-sort, aria-selected, select-all
indeterminate, caption/aria-label dev-warn), build results, grep counts, the
suite pass count (incl. the sort + selection play() round-trips), and any
decision the brief didn't cover. Confirm virtualization is deferred (documented).
