# Slice brief: migrate DataTable engine → @tanstack/react-table

Swap DataTable's bespoke internal pipeline (useState/useMemo sort/filter/
paginate) for the headless **@tanstack/react-table** engine, KEEPING the public
API, the visual/token layer, the cell-type system, RowActions, Pagination
composition, a11y, and all 12 stories. This mirrors our existing architecture
(Base UI = headless engine for interactive primitives; TanStack = headless
engine for the table) — visuals/tokens stay ours.

WORKTREE: `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.
Baseline to migrate FROM: the just-committed bespoke v2 (HEAD `9a4039be`),
`sdks/ui/src/blocks/DataTable/`. Use `mcp__plugin_context7_context7` to pull
current @tanstack/react-table v8 API docs if needed (resolve-library-id
"tanstack table" → query-docs).
> **DO NOT commit, push, or merge.** Implement + self-verify + report.

## Add the dependency
`@tanstack/react-table` (current stable v8, `^8`) as a runtime dep of
`sdks/ui` (`pnpm add @tanstack/react-table --filter @zeroship/ui`, or edit
package.json + `pnpm install` from the worktree root). Confirm it installs and
the workspace still builds.

## Hard contract (DO NOT break)
The **public API is frozen** — `DataTableProps<T>`, `DataTableColumn<T>` (key,
header, cell, accessor, type, sortable, align, width, minWidth, truncate,
filterable, getFilterValue, currency, numberOptions, dateOptions, badgeIntent,
href, linkTarget/Rel, actions), `sort`/`onSortChange`, `globalFilter`/
`onGlobalFilterChange`, `columnFilters`/`onColumnFiltersChange`, selection props,
`page`/`pageSize`/`onPageChange`/`onPageSizeChange`/`defaultPage`/`defaultPageSize`/
`pageSizeOptions`/`paginated`, `manualSorting`/`manualFiltering`/`manualPagination`,
`stickyHeader`/`density`/`loading`/`renderEmpty`/`renderError`/`renderNoResults`/
`caption`/`onRowClick`, generic `<T>` + the forwardRef generic re-assertion.
**All 12 existing DataTable stories + their `play()` assertions must pass
UNCHANGED** (they are the regression net for this migration — do not weaken
them). The visual output should be equivalent (re-capture to confirm parity).

## Engine mapping (internal only)
- **Columns:** adapt each `DataTableColumn<T>` → a TanStack `ColumnDef<T>`:
  `accessorFn` = `column.accessor ?? (row)=>row[key]`; `id` = `column.key`;
  `header` = our header; `cell` = our existing cell renderer (the `column.type`
  formatter system + `column.cell` override) via `flexRender`/a cell fn that
  receives the row; `enableSorting` = `!!column.sortable`; `enableColumnFilter` =
  `column.filterable !== false`; carry `align/width/minWidth/truncate` in
  `column.meta`. The selection checkbox column + the actions column are
  display/meta columns (no accessor; `enableSorting:false`).
- **State + managed/manual:** drive a `useReactTable` instance with `state` for
  sorting / globalFilter / columnFilters / pagination / rowSelection. Map our
  managed-vs-controlled-vs-manual matrix:
  - managed (default): use TanStack `getSortedRowModel`/`getFilteredRowModel`/
    `getPaginationRowModel`; seed `initialState` from our `default*`; wire
    `onSortingChange` etc. to BOTH update TanStack state AND fire our `on*Change`.
  - `manualSorting`/`manualFiltering`/`manualPagination` → set the matching
    TanStack `manual*: true` (TanStack then does NOT transform that axis) and
    rely on the consumer-provided rows/`total`; still fire our `on*Change`.
  - controlled value provided → feed it into TanStack `state` (controlled).
  Preserve our single-column sort toggle semantics (asc→desc) and "reset to
  page 1 on filter/sort/pageSize change in managed pagination".
- **Selection:** map our `selection` (none/single/multiple) → TanStack
  `enableRowSelection`/`enableMultiRowSelection`; `getRowId` = `rowKey`;
  `selectedKeys[] ⇄ rowSelection` record; header select-all via
  `table.getToggleAllRowsSelectedHandler()` + `getIsAllRowsSelected()`/
  `getIsSomeRowsSelected()` (indeterminate); fire `onSelectionChange(keys)`.
- **Sort header:** `aria-sort` from `header.column.getIsSorted()` ("ascending"/
  "descending"/absent); toggle via `column.getToggleSortingHandler()` inside our
  `<button>`.
- **Pagination:** render OUR `Pagination` block from `table.getState().pagination`
  (TanStack 0-based `pageIndex` ⇄ our 1-based `page`: page = pageIndex+1) +
  `table.getPageCount()`/filtered row count for `total`; wire its `onPageChange`
  → `table.setPageIndex(page-1)`, `onPageSizeChange` → `table.setPageSize`.
- **Rows:** render header from `table.getHeaderGroups()`, body from
  `table.getRowModel().rows` (managed → current page rows; manualPagination →
  the given page). Loading→Skeleton (header shown); empty/no-results slots as today.

## Keep (carry over verbatim where possible)
The CSS (`DataTable.css`), the cell-type formatters + `column.type` rendering,
RowActions Menu (real `Menu`), Toolbar + global search `Input` + column-filter
inputs, Badge/link/boolean renderers, all a11y wiring, `data-slot` vocabulary,
forced-colors/reduced-motion. Virtualization still deferred (note: TanStack +
`@tanstack/react-virtual` is now the documented future path).

## Constraints
`--zs-*` tokens only; no raw hex/px; logical properties; no "HIG"/"Apple";
pre-launch no-back-compat. Compose the REAL Pagination/Badge/Menu/Button/Input/
Select/Skeleton/EmptyState. Generic `<T>` preserved.

## Verify (run, REPORT; do not commit)
```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm install   # after adding the dep
pnpm --filter @zeroship/ui build && pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src && echo "hex/px: $(grep -rEn '#[0-9a-fA-F]{3,8}\b' --include='*.css' --include='*.tsx' blocks/DataTable|wc -l)/$(grep -rEn '[0-9]+px' --include='*.css' --include='*.tsx' blocks/DataTable|wc -l)"
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6253 --silent &) ; sleep 3
npx test-storybook --config-dir .storybook --url http://127.0.0.1:6253 --maxWorkers=1 DataTable.stories 2>&1 | grep -E 'Tests:|✕'
```
(Dev server on :6006 — use 6253.) Report: the dep added + version, the
ColumnDef/state/selection/pagination mapping, confirmation the public API is
unchanged and ALL 12 stories pass unchanged, build + grep + suite results, and
any API prop that had to change (should be none — flag loudly if so).
