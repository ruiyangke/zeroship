# Slice brief: Pagination block

A reusable, controlled pagination control. Standalone block AND the footer
control DataTable v2 will compose. Lives in `sdks/ui/src/blocks/Pagination/`.

WORKTREE: `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.
> **DO NOT commit, push, or merge.** Implement + self-verify + report.

## Reference patterns
- `components/Button/Button.tsx` — compose for prev/next/page buttons (use a
  quiet variant: `plain`/`gray`; active page = a filled/selected look).
- `components/Breadcrumbs/Breadcrumbs.tsx` — the ellipsis/truncation logic
  pattern for collapsing long page ranges.
- `components/Select/Select.tsx` — reuse for the optional page-size selector.
- `components/_classnames.ts`. House: forwardRef, data-slot, per-prop JSDoc.

## API (controlled, prop-driven)
```ts
export type PaginationSize = "sm" | "md";
interface PaginationProps extends Omit<ComponentPropsWithoutRef<"nav">, "onChange"> {
  page: number;                 // 1-based current page (controlled)
  pageSize: number;             // items per page
  total: number;                // total item count → pageCount = ceil(total/pageSize)
  onPageChange: (page: number) => void;
  siblingCount?: number;        // page buttons either side of current; default 1
  boundaryCount?: number;       // pages pinned at each end; default 1
  showSummary?: boolean;        // "Showing 1–10 of 57" (default true)
  pageSizeOptions?: number[];   // when set, render a page-size <Select>
  onPageSizeChange?: (size: number) => void;
  size?: PaginationSize;        // default "md"
  disabled?: boolean;
}
```
- Compute `pageCount = Math.max(1, Math.ceil(total / pageSize))`. Build the page
  list with first/last boundary pages + sibling window around `page`, inserting
  an **ellipsis** (`<span aria-hidden>…</span>`, NOT a button — non-actionable
  gap) where pages are skipped. Clamp page into `[1, pageCount]`.
- Prev/Next buttons: disabled (`aria-disabled` + no-op) at the bounds.
- Active page button: `aria-current="page"`, selected style; others navigate via
  `onPageChange`.
- Summary: "Showing {from}–{to} of {total}" using the page math (from =
  (page-1)*pageSize+1, to = min(page*pageSize, total); handle total=0 → "No
  results"). Use `--zs-label-secondary`, footnote type.
- Optional page-size `Select` (only if `pageSizeOptions`): labelled "Rows per
  page"; `onPageSizeChange` fires.

## a11y
`<nav aria-label="Pagination">`. Page buttons are real `<button>`s with
`aria-label="Go to page N"` and the current one `aria-current="page"`.
Prev/Next have `aria-label`s; disabled at bounds. Ellipsis is decorative
(`aria-hidden`, not focusable). Keyboard = native buttons. Focus-visible rings.

## Constraints
`--zs-*` tokens only; no raw hex/px; logical properties; forced-colors +
reduced-motion (button hover) where relevant; no "HIG"/"Apple"; data-slot
vocabulary (`pagination`, `pagination-prev`, `-next`, `-page`, `-ellipsis`,
`-summary`, `-page-size`). Export from `blocks/index.ts`; `@import` CSS into
`styles.css` (Composed blocks group); story `layout: "fullscreen"`.

## Stories (`src/stories/Pagination.stories.tsx`)
- Basic (page 3 of many → shows boundary+ellipsis+siblings), FewPages (no
  ellipsis), WithPageSize (Select), Disabled, Empty (total 0). 
- **play()**: click a page button → `onPageChange` fires with that page; click
  Next → page+1; Prev at page 1 is disabled (no fire); current page has
  `aria-current="page"`. axe on each.

## Verify (run, REPORT; do not commit)
```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build && pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src && echo "hex/px: $(grep -rEn '#[0-9a-fA-F]{3,8}\b' --include='*.css' --include='*.tsx' blocks/Pagination|wc -l)/$(grep -rEn '[0-9]+px' --include='*.css' --include='*.tsx' blocks/Pagination|wc -l)"
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6243 --silent &) ; sleep 3
npx test-storybook --config-dir .storybook --url http://127.0.0.1:6243 --maxWorkers=1 Pagination.stories 2>&1 | grep -E 'Tests:|✕'
```
(Dev server on :6006 — use 6243.) Report: files, the page-range/ellipsis algo,
a11y wiring, build, grep counts, suite pass count.
