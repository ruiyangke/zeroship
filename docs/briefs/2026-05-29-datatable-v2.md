# Slice brief: DataTable v2 — pagination, filtering, rich cells, managed mode

Evolve the existing presentational `DataTable` (`sdks/ui/src/blocks/DataTable/`)
into a full-featured data grid while PRESERVING its controlled/presentational
contract. Big slice — gets full dual review + visual.

WORKTREE: `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.
> **DO NOT commit, push, or merge.** Implement + self-verify + report.

## Current state (keep working)
Columns (`key/header/cell/sortable/align/width`), `data`, `rowKey`, `caption`/
`aria-label`, `sort`/`onSortChange`, `selection`(none/single/multiple)/
`selectedKeys`/`onSelectionChange`, `stickyHeader`, `density`, `loading`/
`loadingRowCount`, `renderEmpty`/`renderError`. Semantic `<table>`, aria-sort,
aria-selected, select-all indeterminate. All existing DataTable `play()`s must
still pass (adapt them if the API shape changes, but keep the assertions' intent).

## v2 additions

### 1. Managed (client-side) mode — DEFAULT — with per-axis manual opt-outs
The key DX win for the AI builder: `<DataTable columns data rowKey />` should
sort, filter, and paginate **out of the box**. Adopt the TanStack-style model:
- DataTable holds internal state for sort / globalFilter / columnFilters / page
  (+ `defaultSort`, `defaultPageSize`, etc. for initial values), applies them to
  `data` to derive the visible rows, AND still fires `onSortChange` /
  `onGlobalFilterChange` / `onPageChange` so consumers can observe.
- Per-axis manual flags opt INTO server-side: `manualSorting`, `manualFiltering`,
  `manualPagination` (each default `false`). When a manual flag is set, DataTable
  does NOT transform that axis itself — it renders `data` as-given and only
  emits intent; the consumer owns it. `manualPagination` requires `total` (row
  count) for the page math; otherwise `total = data.length`.
- A fully-controlled consumer can pass `sort`+`manualSorting`, etc. (controlled
  value + manual transform = the old presentational behavior). Document this
  matrix clearly in the file-header JSDoc.

### 2. Sort (enhance existing)
Keep single-column toggle (asc→desc→…). In client mode, sort the visible rows by
the active column: respect a `column.sortFn?` if given, else a sensible default
(numbers numeric, dates by time, strings localeCompare; nullish last). `aria-sort`
stays correct.

### 3. Filtering
- **Global search** (toolbar): a search `Input`; `globalFilter`/`onGlobalFilterChange`
  (controlled) or internal (client). In client mode, keep rows where ANY column's
  searchable text contains the query (case-insensitive). A column opts out with
  `column.filterable === false`; derive searchable text from `column.getFilterValue?.(row)`
  else the cell's string value.
- **Per-column filter** (optional): columns with `filterable !== false` AND a
  declared filter render a small filter control in an OPTIONAL filter row under
  the header (toggle via a `filterable` table-level prop or always-when-any-
  column-declares). Keep it to a **text contains** input per filterable column
  (skip per-column Select UI this round — note as a future add). `columnFilters`/
  `onColumnFiltersChange`. Client mode applies them.
- Empty-after-filter → the empty slot (renderEmpty / default EmptyState) with a
  "no results match" message variant.

### 4. Pagination
Compose the **Pagination block** (`import { Pagination, buildPageItems }`) in a
footer region. Client mode: DataTable slices the (filtered+sorted) rows by
`page`/`pageSize` and renders Pagination with `total` = filtered count.
`manualPagination`: consumer provides `page`/`pageSize`/`total`. Props:
`paginated?` (default true when rows exceed pageSize, OR explicit), `pageSize`/
`defaultPageSize` (default 10), `pageSizeOptions?`, `page`/`onPageChange`,
`onPageSizeChange`. Hide pagination when total ≤ smallest pageSize and not forced.

### 5. Rich cell formatting — column `type` presets
Add `column.type?: "text" | "number" | "currency" | "date" | "datetime" |
"boolean" | "badge" | "link" | "actions"` driving a default renderer (an explicit
`column.cell` ALWAYS overrides). Implement small internal formatter helpers
(token-pure, no deps beyond `Intl`):
- `number`: `Intl.NumberFormat` (+ `column.numberOptions?`). Right-align default.
- `currency`: `Intl.NumberFormat` style currency (+ `column.currency` code,
  default "USD"). Right-align.
- `date`/`datetime`: `Intl.DateTimeFormat` (+ `column.dateOptions?`); accepts
  Date | number | ISO string.
- `boolean`: a check / dash glyph (aria-labelled "Yes"/"No"), not color-only.
- `badge`: render the value via the real `Badge`; `column.badgeIntent?: (value)=>BadgeIntent`
  maps value→intent (default neutral).
- `link`: an `<a>` with `column.href?: (row)=>string` (+ rel/target opts);
  falls back to text if no href.
- `actions`: a trailing **RowActions** kebab — a `Menu` (compose the real `Menu`
  component) triggered by an icon-only `Button` (`aria-label="Row actions"`),
  items from `column.actions?: (row)=>{label,onSelect,disabled?,danger?}[]`.
  Sticky/last column, not sortable, header empty or "Actions" visually-hidden.
The default cell renderer reads `column.accessor?: (row)=>unknown` (else
`row[column.key]`) and formats by `type`.

### 6. Toolbar
A header region above the table: optional `title` (heading), optional `toolbar`
(ReactNode — extra actions/buttons on the end), and the global search Input
(when `searchable`, default true if any column searchable). Lay out with
`Cluster`/flex; the search trails or leads sensibly; wraps on narrow.

### 7. Misc polish
- `onRowClick?: (row)=>void` (row gets `cursor` + hover + keyboard? keep simple:
  click only; do NOT make rows buttons — document that interactive rows should
  use a link/actions). Skip if it complicates a11y; optional.
- Column `minWidth`/`width`; cell text truncation with `title` on overflow.
- Keep zebra OFF (separators only) unless trivial.

## a11y (must hold + extend)
Semantic table; `aria-sort`; `aria-selected`; select-all indeterminate; caption-
or-aria-label dev-warn. Global search Input labelled; per-column filter inputs
labelled by their column. RowActions Menu is the real Menu (focus/aria handled
by Base UI). Pagination a11y comes from the Pagination block. Boolean cells carry
a text label (not color/glyph alone). Loading→Skeleton rows, header shown.

## Constraints
`--zs-*` tokens only; no raw hex/px; logical properties; forced-colors + reduced-
motion; no "HIG"/"Apple"; pre-launch no-back-compat. Compose REAL Pagination /
Badge / Menu / Button / Input / Select / Skeleton / EmptyState — don't re-roll.
Generic over `<T>`; keep the `forwardRef` generic re-assertion. Virtualization
still DEFERRED (document; managed mode renders the current page only, which keeps
DOM bounded — note that paginating is the scale strategy).

## Stories (`src/stories/DataTable.stories.tsx` — extend)
Keep existing (Basic/Sortable/SelectableMultiple/Empty/Loading/StickyHeader/
DensityCompact). ADD, each with a useState wrapper + `play()`:
- **Managed** — `<DataTable>` with ~25 rows, no manual flags: type a global
  search → rows filter; click a sortable header → rows reorder; Pagination
  shows; click page 2 → different rows. (Assert filter reduces row count, sort
  reorders, page 2 changes rows.)
- **RichColumns** — columns demonstrating number/currency/date/boolean/badge/
  link + an actions kebab; play: open the actions Menu, click an item → its
  `onSelect` fires.
- **ManualServerSide** — `manualSorting`+`manualFiltering`+`manualPagination`
  with controlled state; play: assert the on*Change handlers fire and DataTable
  renders exactly the given `data` page (does NOT re-sort/filter itself).
- **GlobalFilterEmpty** — search with no matches → empty slot renders.

## Verify (run, REPORT; do not commit)
```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build && pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src && echo "hex/px: $(grep -rEn '#[0-9a-fA-F]{3,8}\b' --include='*.css' --include='*.tsx' blocks/DataTable|wc -l)/$(grep -rEn '[0-9]+px' --include='*.css' --include='*.tsx' blocks/DataTable|wc -l)"
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6247 --silent &) ; sleep 3
npx test-storybook --config-dir .storybook --url http://127.0.0.1:6247 --maxWorkers=1 DataTable.stories 2>&1 | grep -E 'Tests:|✕'
```
(Dev server on :6006 — use 6247.) Report: the managed/manual matrix, the cell-
type system + formatters, the filter/pagination wiring (composing Pagination),
RowActions Menu, a11y, build, grep counts, suite pass count, every decision the
brief left open.
