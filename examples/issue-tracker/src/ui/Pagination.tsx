import type { ComponentPropsWithoutRef } from "react";

import { Button } from "./Button";
import { cn } from "./cn";

type PageItem =
  | { type: "page"; page: number }
  | { type: "ellipsis"; key: "start" | "end" };

function buildPageItems(
  currentPage: number,
  pageCount: number,
  siblingCount: number,
  boundaryCount: number,
): PageItem[] {
  const shown = new Set<number>();
  for (let page = 1; page <= Math.min(boundaryCount, pageCount); page++) shown.add(page);
  for (let page = Math.max(pageCount - boundaryCount + 1, 1); page <= pageCount; page++) {
    shown.add(page);
  }
  for (
    let page = Math.max(currentPage - siblingCount, 1);
    page <= Math.min(currentPage + siblingCount, pageCount);
    page++
  ) {
    shown.add(page);
  }

  const items: PageItem[] = [];
  let previous = 0;
  for (const page of [...shown].sort((left, right) => left - right)) {
    const gap = page - previous;
    if (gap === 2) {
      items.push({ type: "page", page: previous + 1 });
    } else if (gap > 2) {
      items.push({ type: "ellipsis", key: page < currentPage ? "start" : "end" });
    }
    items.push({ type: "page", page });
    previous = page;
  }
  if (pageCount - previous === 1) {
    items.push({ type: "page", page: pageCount });
  } else if (pageCount - previous > 1) {
    items.push({ type: "ellipsis", key: "end" });
  }
  return items;
}

export interface PaginationProps extends Omit<ComponentPropsWithoutRef<"nav">, "onChange"> {
  page: number;
  pageSize: number;
  total: number;
  onPageChange: (page: number) => void;
  siblingCount?: number;
  boundaryCount?: number;
}

export function Pagination({
  page,
  pageSize,
  total,
  onPageChange,
  siblingCount = 1,
  boundaryCount = 1,
  className,
  "aria-label": ariaLabel = "Pagination",
  ...props
}: PaginationProps) {
  const safeTotal = Math.max(0, Number.isFinite(total) ? Math.trunc(total) : 0);
  const safePageSize = Math.max(1, Number.isFinite(pageSize) ? Math.trunc(pageSize) : 1);
  const pageCount = Math.max(1, Math.ceil(safeTotal / safePageSize));
  const currentPage = Math.min(
    Math.max(Number.isFinite(page) ? Math.trunc(page) : 1, 1),
    pageCount,
  );
  const from = (currentPage - 1) * safePageSize + 1;
  const to = Math.min(currentPage * safePageSize, safeTotal);
  const items = buildPageItems(
    currentPage,
    pageCount,
    Math.max(0, Math.trunc(siblingCount)),
    Math.max(0, Math.trunc(boundaryCount)),
  );

  const goTo = (next: number) => {
    const resolved = Math.min(Math.max(next, 1), pageCount);
    if (resolved !== currentPage) onPageChange(resolved);
  };

  return (
    <nav
      {...props}
      aria-label={ariaLabel}
      className={cn(
        "flex min-w-0 flex-wrap items-center gap-3 text-sm text-ink-secondary",
        className,
      )}
    >
      <p className="m-0 tabular-nums">
        {safeTotal === 0 ? "No results" : `Showing ${from}–${to} of ${safeTotal}`}
      </p>
      <div className="ms-auto inline-flex items-center gap-1">
        <Button
          type="button"
          variant="plain"
          className="h-7 px-3"
          aria-label="Go to previous page"
          disabled={currentPage <= 1}
          onClick={() => goTo(currentPage - 1)}
        >
          <span aria-hidden="true" className="text-md leading-none">
            ‹
          </span>
          <span className="whitespace-nowrap">Prev</span>
        </Button>

        {items.map((item) =>
          item.type === "ellipsis" ? (
            <span
              key={`ellipsis-${item.key}`}
              aria-hidden="true"
              className="inline-flex size-6 min-w-6 items-center justify-center text-ink-muted"
            >
              …
            </span>
          ) : (
            <Button
              key={`page-${item.page}`}
              type="button"
              variant="plain"
              className={cn(
                "h-7 min-w-6 px-1 font-mono tabular-nums",
                item.page === currentPage &&
                  "border-line bg-surface-sunken text-accent-strong!",
              )}
              aria-label={`Go to page ${item.page}`}
              aria-current={item.page === currentPage ? "page" : undefined}
              onClick={() => goTo(item.page)}
            >
              {item.page}
            </Button>
          ),
        )}

        <Button
          type="button"
          variant="plain"
          className="h-7 px-3"
          aria-label="Go to next page"
          disabled={currentPage >= pageCount}
          onClick={() => goTo(currentPage + 1)}
        >
          <span className="whitespace-nowrap">Next</span>
          <span aria-hidden="true" className="text-md leading-none">
            ›
          </span>
        </Button>
      </div>
    </nav>
  );
}
