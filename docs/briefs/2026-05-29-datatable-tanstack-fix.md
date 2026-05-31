# Fix brief: DataTable TanStack migration — merged review fixes

Merges codex (CHANGES-NEEDED, 3🔴) + claude (APPROVE-WITH-NITS, 1🟡) on the
TanStack engine migration. All are TanStack-projection correctness regressions
vs the bespoke baseline that the current 12 stories don't exercise. File:
`sdks/ui/src/blocks/DataTable/DataTable.tsx` (+ stories for regressions). Public
API stays frozen; CSS unchanged.

WORKTREE: `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.
> **DO NOT commit, push, or merge.** Each fix ships a regression that FAILS on
> the current code and passes after. Prove the pre-fix failures (revert-and-run
> like the Pagination fixer did) and report.

## 🔴 1. `paginated={false}` must render ALL rows (not the first page)
`DataTable.tsx` (~line 930). `table.getRowModel().rows` always goes through
`getPaginationRowModel`, so when pagination is off (`showPagination` false — e.g.
StickyHeader's 24 rows) it renders only the first `pageSize` rows with no footer.
Baseline rendered all sorted rows. FIX: when `!manualPagination && !showPagination`,
render `table.getPrePaginationRowModel().rows` (and derive the visible selection
keys from that SAME row model so select-all spans all rows). Keep the paginated
path otherwise.
REGRESSION: strengthen the **StickyHeader** story's `play()` to assert ALL its
rows render (count === the data length, e.g. 24) and no pagination footer is
present. Fails pre-fix (renders 10).

## 🔴 2. Clamp the page projected INTO TanStack (body/footer agreement)
`DataTable.tsx` (~line 864 vs 944). `paginationState.pageIndex = page - 1` (raw)
while the footer uses a clamped `currentPage`. If `page` is out of range or
filtering shrinks the page count, the body renders empty while `<Pagination>`
shows the clamped last page. FIX: project the CLAMPED page into TanStack
(`pageIndex = currentPage - 1`, computed before building paginationState), so the
sliced rows and the footer always agree.
REGRESSION: a story (e.g. **PageClamp**) with controlled `page` set beyond the
last page (or data that shrinks the page count) asserting the rendered body rows
are the last page's rows (non-empty) and match the footer's current page. Fails
pre-fix (empty body).

## 🔴 3. Ignore filters/sort for `filterable:false` / actions columns
`DataTable.tsx` (~line 856). The projected `columnFiltersState` keeps every
non-empty filter; `enableColumnFilter:false` does NOT stop TanStack's filtered
row model from applying the column's `filterFn`. Baseline ignored
`column.filterable === false`. FIX: when projecting `columnFiltersState`, include
only columns that exist AND have `filterable !== false` AND `type !== "actions"`
(drop the rest). (Same guard already gates which columns render a filter input —
reuse it.)
REGRESSION: a story (e.g. **NonFilterableIgnored**) that passes a `columnFilters`
entry targeting a `filterable: false` column and asserts rows are NOT filtered by
it (row count unchanged). Fails pre-fix.

## 🟡 4. Global search must be type-agnostic (enableGlobalFilter on every searchable column)
`DataTable.tsx` (~ColumnDef map, line 824). TanStack's `getColumnCanGlobalFilter`
gates global filtering on the FIRST row's value being string/number, so a table
whose searchable columns are all non-string/number-typed (date/boolean/object, or
null first row) silently skips global search entirely. Baseline scanned
`filterText()` of every searchable column unconditionally. FIX: set
`enableGlobalFilter: column.filterable !== false && !isActions` on each ColumnDef
so the custom `globalFilterFn` always runs regardless of first-row type.
REGRESSION: a story (e.g. **GlobalSearchNonString**) whose only searchable column
is a non-string type (e.g. a `number`/`date` column, or an accessor returning a
non-string) where the first row's value is non-string, asserting a global search
query still filters rows. Fails pre-fix (no-op search → all rows remain).

## Verify (run, REPORT; do not commit)
```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build && pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src && echo "hex/px: $(grep -rEn '#[0-9a-fA-F]{3,8}\b' --include='*.css' --include='*.tsx' blocks/DataTable|wc -l)/$(grep -rEn '[0-9]+px' --include='*.css' --include='*.tsx' blocks/DataTable|wc -l)"
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6259 --silent &) ; sleep 3
npx test-storybook --config-dir .storybook --url http://127.0.0.1:6259 --maxWorkers=1 DataTable.stories 2>&1 | grep -E 'Tests:|✕'
```
(Dev server on :6006 — use 6259.) Public API must remain unchanged. Report: each
fix's diff, the 4 regressions + proof they fail pre-fix, build/grep/suite counts
(now 12 existing + the new regression stories, all passing).
