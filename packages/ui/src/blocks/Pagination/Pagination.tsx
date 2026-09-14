/*
 * Pagination — a controlled, prop-driven pagination control.
 *
 * Standalone block AND the footer control DataTable v2 composes. It owns
 * NO page state: `page` / `pageSize` / `total` come in, `onPageChange`
 * (and optional `onPageSizeChange`) go out. The caller is the single
 * source of truth.
 *
 * Layout (left → right):
 *
 *   .zs-pagination                       <nav aria-label="Pagination">
 *     ├─ .zs-pagination__summary         "Showing 1–10 of 57" (optional)
 *     ├─ .zs-pagination__page-size       "Rows per page" <Select> (optional)
 *     └─ .zs-pagination__pages           the page-button row
 *          ├─ Prev button
 *          ├─ page buttons / ellipsis spans
 *          └─ Next button
 *
 * Composition:
 *   - Prev / Next / page buttons are real `<Button>`s (quiet `plain` for
 *     idle pages, `tinted` for the current page) so they inherit the
 *     package focus-ring, hover, and forced-colors treatment for free.
 *   - The optional page-size selector is a `<Select>` — same option-list
 *     surface used everywhere a fixed choice list appears.
 *
 * Page-range algorithm (see `buildPageItems`):
 *   - `pageCount = max(1, ceil(total / pageSize))`. `page` is clamped into
 *     `[1, pageCount]` before anything renders, so an out-of-range
 *     controlled value never produces a broken row.
 *   - We always pin `boundaryCount` pages at each end and show a window of
 *     `siblingCount` pages either side of the current page. Where a gap of
 *     MORE THAN ONE page falls between two shown pages we insert a single
 *     ellipsis; a gap of exactly one collapses to that one page number
 *     (rendering "1 … 3" when "1 2 3" is shorter would be silly).
 *   - Ellipses are decorative `<span aria-hidden>…</span>` — NOT buttons.
 *     They mark an elided, non-actionable range.
 *
 * a11y:
 *   - Root is `<nav aria-label="Pagination">`.
 *   - Each page button is a real `<button aria-label="Go to page N">`; the
 *     current one carries `aria-current="page"`.
 *   - Prev / Next carry `aria-label`s and are `disabled` at the bounds
 *     (and whenever the whole control is `disabled`).
 *   - Ellipses are `aria-hidden` and not focusable.
 *   - Keyboard handling is entirely native (real buttons). Focus-visible
 *     rings come from Button.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ReactNode,
  type Ref,
} from "react";
import { Button } from "../../components/Button";
import { Select } from "../../components/Select";
import { classnames } from "../../components/_classnames";

/* ─── public types ─────────────────────────────────────────────────────── */

export type PaginationSize = "sm" | "md";

export interface PaginationProps
  extends Omit<ComponentPropsWithoutRef<"nav">, "onChange"> {
  /** 1-based current page (controlled). Clamped into `[1, pageCount]`. */
  page: number;
  /** Items per page. Drives `pageCount = ceil(total / pageSize)`. */
  pageSize: number;
  /** Total item count across all pages. `0` renders the "No results" summary. */
  total: number;
  /**
   * Fired with the requested 1-based page when a page / Prev / Next button
   * is activated. Never fires for the current page, a disabled control, or
   * an out-of-bounds Prev/Next.
   */
  onPageChange: (page: number) => void;
  /**
   * Page buttons shown either side of the current page. Default `1`
   * (so the current page sits in a 3-wide window).
   */
  siblingCount?: number;
  /**
   * Page buttons pinned at each end of the row. Default `1` (first + last
   * always visible).
   */
  boundaryCount?: number;
  /**
   * Show the "Showing {from}–{to} of {total}" summary. Default `true`.
   * `total === 0` renders "No results".
   */
  showSummary?: boolean;
  /**
   * When provided, render a "Rows per page" `<Select>` of these sizes.
   * Omit to hide the page-size control entirely.
   */
  pageSizeOptions?: number[];
  /** Fired with the chosen size when the page-size `<Select>` changes. */
  onPageSizeChange?: (size: number) => void;
  /** Control density. `sm` maps to small buttons, `md` (default) to medium. */
  size?: PaginationSize;
  /** Disable the entire control — every button becomes non-interactive. */
  disabled?: boolean;
  /** Accessible label on the wrapping `<nav>`. Default `"Pagination"`. */
  "aria-label"?: string;
}

/* ─── page-range model ─────────────────────────────────────────────────── */

/**
 * A rendered slot in the page row: either a concrete page number or an
 * elided gap. Gaps carry a stable `key` so the two possible ellipsis
 * positions (head / tail) keep their identity across renders.
 */
type PageItem =
  | { type: "page"; page: number }
  | { type: "ellipsis"; key: "start" | "end" };

/**
 * Build the ordered list of page slots. Pure function of the resolved
 * page math so it is trivially unit-testable and shared with DataTable v2.
 *
 * Strategy: pin `boundaryCount` pages at each end + a `siblingCount`
 * window around `currentPage`, then walk `1..pageCount` keeping any page
 * that lands in a pinned/window band. A jump of more than one between
 * consecutive kept pages becomes ONE ellipsis; a jump of exactly one
 * back-fills the single skipped page (cheaper to show "3 4 5" than
 * "3 … 5").
 */
export function buildPageItems(
  currentPage: number,
  pageCount: number,
  siblingCount: number,
  boundaryCount: number,
): PageItem[] {
  // Defensive clamps so a hostile prop set can't produce NaN/fractional
  // bands. currentPage & pageCount are finite-guarded + floored too, since
  // a direct caller (e.g. DataTable) may pass raw numbers.
  const siblings = Math.max(0, Math.floor(siblingCount));
  const boundaries = Math.max(0, Math.floor(boundaryCount));
  const count = Math.max(1, Number.isFinite(pageCount) ? Math.floor(pageCount) : 1);
  const current = Math.min(
    Math.max(Number.isFinite(currentPage) ? Math.floor(currentPage) : 1, 1),
    count,
  );

  // The set of page numbers we definitely show.
  const shown = new Set<number>();
  // Leading + trailing boundary bands.
  for (let p = 1; p <= Math.min(boundaries, count); p++) shown.add(p);
  for (let p = Math.max(count - boundaries + 1, 1); p <= count; p++)
    shown.add(p);
  // Sibling window around the current page.
  for (
    let p = Math.max(current - siblings, 1);
    p <= Math.min(current + siblings, count);
    p++
  )
    shown.add(p);

  const ordered = [...shown].sort((a, b) => a - b);

  const items: PageItem[] = [];
  let prev = 0;
  for (const p of ordered) {
    const gap = p - prev;
    if (gap === 2) {
      // Exactly one page skipped — show it instead of an ellipsis.
      items.push({ type: "page", page: prev + 1 });
    } else if (gap > 2) {
      // A multi-page gap → a single ellipsis. Key it by which side of the
      // current page it falls on so head/tail ellipses stay distinct.
      items.push({
        type: "ellipsis",
        key: p < current ? "start" : "end",
      });
    }
    items.push({ type: "page", page: p });
    prev = p;
  }
  // Trailing gap: when the last shown page stops short of `count` (e.g.
  // boundaryCount={0} drops the trailing boundary band), the leftover tail
  // would vanish. Back-fill a single page for a one-page gap, or a single
  // end ellipsis for a wider one — mirroring the leading-gap handling that
  // falls out of `prev` starting at 0.
  if (count - prev === 1) {
    items.push({ type: "page", page: count });
  } else if (count - prev > 1) {
    items.push({ type: "ellipsis", key: "end" });
  }
  return items;
}

/* ─── root ─────────────────────────────────────────────────────────────── */

const PaginationRoot = forwardRef<HTMLElement, PaginationProps>(
  function Pagination(
    {
      page,
      pageSize,
      total,
      onPageChange,
      siblingCount = 1,
      boundaryCount = 1,
      showSummary = true,
      pageSizeOptions,
      onPageSizeChange,
      size = "md",
      disabled = false,
      className,
      "aria-label": ariaLabel = "Pagination",
      ...rest
    },
    ref,
  ) {
    // Resolve the page math once. Numeric props can arrive fractional
    // (page={2.5}) or non-finite (NaN / Infinity) from a hostile/loose
    // caller, so finite-guard + integer-coerce every input BEFORE it can
    // leak into pageCount, the summary, onPageChange, or buildPageItems.
    const intOr = (value: number, fallback: number) =>
      Number.isFinite(value) ? Math.trunc(value) : fallback;
    const safeTotal = Math.max(0, intOr(total, 0));
    const safePageSize = Math.max(1, intOr(pageSize, 1));
    const pageCount = Math.max(1, Math.ceil(safeTotal / safePageSize));
    const currentPage = Math.min(Math.max(intOr(page, 1), 1), pageCount);

    const buttonSize = size === "sm" ? "small" : "medium";

    const goTo = (next: number) => {
      if (disabled) return;
      const clamped = Math.min(Math.max(next, 1), pageCount);
      if (clamped === currentPage) return;
      onPageChange(clamped);
    };

    const items = buildPageItems(
      currentPage,
      pageCount,
      siblingCount,
      boundaryCount,
    );

    const atStart = currentPage <= 1;
    const atEnd = currentPage >= pageCount;

    // Summary: "Showing {from}–{to} of {total}", or "No results" at zero.
    let summary: ReactNode = null;
    if (showSummary) {
      if (safeTotal <= 0) {
        summary = "No results";
      } else {
        const from = (currentPage - 1) * safePageSize + 1;
        const to = Math.min(currentPage * safePageSize, safeTotal);
        // En dash for the range; the values are localized integers.
        summary = `Showing ${from}–${to} of ${safeTotal}`;
      }
    }

    return (
      // Rest spread FIRST so the documented contract attrs (data-slot,
      // aria-label, the resolved className) win over raw caller spread.
      <nav
        {...rest}
        ref={ref as Ref<HTMLElement>}
        data-slot="pagination"
        data-size={size}
        aria-label={ariaLabel}
        className={classnames("zs-pagination", className)}
      >
        {summary != null ? (
          <p data-slot="pagination-summary" className="zs-pagination__summary">
            {summary}
          </p>
        ) : null}

        {pageSizeOptions != null && pageSizeOptions.length > 0 ? (
          <label
            data-slot="pagination-page-size"
            className="zs-pagination__page-size"
          >
            <span className="zs-pagination__page-size-label">
              Rows per page
            </span>
            <Select
              size={size}
              value={String(pageSize)}
              disabled={disabled}
              onValueChange={(value) => {
                if (disabled || value == null) return;
                const next = Number(value);
                if (Number.isFinite(next)) onPageSizeChange?.(next);
              }}
            >
              {pageSizeOptions.map((opt) => (
                <Select.Item key={opt} value={String(opt)}>
                  {opt}
                </Select.Item>
              ))}
            </Select>
          </label>
        ) : null}

        <div data-slot="pagination-pages" className="zs-pagination__pages">
          <Button
            type="button"
            variant="plain"
            size={buttonSize}
            data-slot="pagination-prev"
            className="zs-pagination__button zs-pagination__button--prev"
            aria-label="Go to previous page"
            disabled={disabled || atStart}
            onClick={() => goTo(currentPage - 1)}
          >
            <span aria-hidden="true" className="zs-pagination__chevron">
              {"‹"}
            </span>
            <span className="zs-pagination__edge-label">Prev</span>
          </Button>

          {items.map((item) =>
            item.type === "ellipsis" ? (
              <span
                key={`ellipsis-${item.key}`}
                data-slot="pagination-ellipsis"
                className="zs-pagination__ellipsis"
                aria-hidden="true"
              >
                {"…"}
              </span>
            ) : (
              <Button
                key={`page-${item.page}`}
                type="button"
                variant={item.page === currentPage ? "tinted" : "plain"}
                size={buttonSize}
                data-slot="pagination-page"
                data-active={item.page === currentPage || undefined}
                className="zs-pagination__button zs-pagination__button--page"
                aria-label={`Go to page ${item.page}`}
                aria-current={item.page === currentPage ? "page" : undefined}
                disabled={disabled}
                onClick={() => goTo(item.page)}
              >
                {item.page}
              </Button>
            ),
          )}

          <Button
            type="button"
            variant="plain"
            size={buttonSize}
            data-slot="pagination-next"
            className="zs-pagination__button zs-pagination__button--next"
            aria-label="Go to next page"
            disabled={disabled || atEnd}
            onClick={() => goTo(currentPage + 1)}
          >
            <span className="zs-pagination__edge-label">Next</span>
            <span aria-hidden="true" className="zs-pagination__chevron">
              {"›"}
            </span>
          </Button>
        </div>
      </nav>
    );
  },
);
PaginationRoot.displayName = "Pagination";

export const Pagination = PaginationRoot;
