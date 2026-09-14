import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react";
import { expect, fn, userEvent, within } from "@storybook/test";
import { Pagination } from "../blocks";

const meta: Meta<typeof Pagination> = {
  title: "Blocks/Pagination",
  component: Pagination,
  parameters: { layout: "fullscreen" },
  argTypes: {
    size: { control: "inline-radio", options: ["sm", "md"] },
    siblingCount: { control: { type: "number", min: 0 } },
    boundaryCount: { control: { type: "number", min: 0 } },
    showSummary: { control: "boolean" },
    disabled: { control: "boolean" },
  },
};

export default meta;

type Story = StoryObj<typeof Pagination>;

/* ─── 1. Basic — boundary + ellipsis + sibling window ──────────────────── */
export const Basic: Story = {
  name: "Basic (page 3 of many)",
  args: {
    page: 3,
    pageSize: 10,
    total: 200, // → 20 pages
    onPageChange: fn(),
  },
  parameters: {
    docs: {
      description: {
        story:
          "Page 3 of 20. The row pins the first/last boundary page, shows " +
          "a sibling window around the current page, and inserts a single " +
          "decorative `…` span (aria-hidden, NOT a button) where pages are " +
          "skipped. The current page carries `aria-current=\"page\"`.",
      },
    },
  },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);

    // Current page is marked and accessible.
    const current = canvas.getByRole("button", { current: "page" });
    await expect(current).toHaveAccessibleName("Go to page 3");

    // An ellipsis exists and is hidden from AT (so it is NOT a button).
    const nav = canvas.getByRole("navigation", { name: "Pagination" });
    const ellipsis = nav.querySelector('[data-slot="pagination-ellipsis"]');
    await expect(ellipsis).not.toBeNull();
    await expect(ellipsis).toHaveAttribute("aria-hidden", "true");

    // Clicking a different (rendered, in-window) page fires onPageChange
    // with that page. Page 4 sits in the sibling window around page 3.
    const page4 = canvas.getByRole("button", { name: "Go to page 4" });
    await userEvent.click(page4);
    await expect(args.onPageChange).toHaveBeenCalledWith(4);

    // The last boundary page is always pinned and navigable.
    const lastPage = canvas.getByRole("button", { name: "Go to page 20" });
    await userEvent.click(lastPage);
    await expect(args.onPageChange).toHaveBeenCalledWith(20);

    // Next advances by one from the current page (3 → 4).
    const next = canvas.getByRole("button", { name: "Go to next page" });
    await userEvent.click(next);
    await expect(args.onPageChange).toHaveBeenCalledWith(4);
  },
};

/* ─── 2. FewPages — no ellipsis ────────────────────────────────────────── */
export const FewPages: Story = {
  name: "Few pages (no ellipsis)",
  args: {
    page: 1,
    pageSize: 10,
    total: 35, // → 4 pages, all shown
    onPageChange: fn(),
  },
  parameters: {
    docs: {
      description: {
        story:
          "When every page fits the boundary + window bands there is no " +
          "elided range, so no ellipsis renders. Prev is disabled at page 1.",
      },
    },
  },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    const nav = canvas.getByRole("navigation", { name: "Pagination" });

    // No ellipsis when nothing is skipped.
    const ellipsis = nav.querySelector('[data-slot="pagination-ellipsis"]');
    await expect(ellipsis).toBeNull();

    // Prev is disabled at the lower bound and does not fire.
    const prev = canvas.getByRole("button", { name: "Go to previous page" });
    await expect(prev).toBeDisabled();
    await userEvent.click(prev);
    await expect(args.onPageChange).not.toHaveBeenCalled();
  },
};

/* ─── 3. WithPageSize — Select ─────────────────────────────────────────── */
export const WithPageSize: Story = {
  name: "With page-size selector",
  parameters: {
    docs: {
      description: {
        story:
          "A controlled wrapper wires both `onPageChange` and " +
          "`onPageSizeChange`. The page-size `<Select>` is labelled " +
          "\"Rows per page\".",
      },
    },
  },
  render: () => {
    const [page, setPage] = useState(2);
    const [pageSize, setPageSize] = useState(10);
    const total = 137;
    return (
      <Pagination
        data-testid="pagination-pagesize"
        page={page}
        pageSize={pageSize}
        total={total}
        onPageChange={setPage}
        pageSizeOptions={[10, 25, 50]}
        onPageSizeChange={(size) => {
          setPageSize(size);
          setPage(1);
        }}
      />
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // The page-size label is present.
    await expect(canvas.getByText("Rows per page")).toBeInTheDocument();
    // Summary reflects the math: page 2 × size 10 of 137 → 11–20 of 137.
    await expect(
      canvas.getByText("Showing 11–20 of 137"),
    ).toBeInTheDocument();
  },
};

/* ─── 4. Disabled ──────────────────────────────────────────────────────── */
export const Disabled: Story = {
  name: "Disabled",
  args: {
    page: 3,
    pageSize: 10,
    total: 200,
    disabled: true,
    onPageChange: fn(),
  },
  parameters: {
    docs: {
      description: {
        story:
          "`disabled` makes every button non-interactive; clicks never " +
          "fire `onPageChange`.",
      },
    },
  },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    const page4 = canvas.getByRole("button", { name: "Go to page 4" });
    await expect(page4).toBeDisabled();
    await userEvent.click(page4);
    await expect(args.onPageChange).not.toHaveBeenCalled();
  },
};

/* ─── 5. Empty — total 0 ───────────────────────────────────────────────── */
export const Empty: Story = {
  name: "Empty (no results)",
  args: {
    page: 1,
    pageSize: 10,
    total: 0,
    onPageChange: fn(),
  },
  parameters: {
    docs: {
      description: {
        story:
          "With `total === 0` the summary reads \"No results\" and the row " +
          "shows a single page (`1`) with Prev/Next disabled at the bounds.",
      },
    },
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByText("No results")).toBeInTheDocument();
    const prev = canvas.getByRole("button", { name: "Go to previous page" });
    const next = canvas.getByRole("button", { name: "Go to next page" });
    await expect(prev).toBeDisabled();
    await expect(next).toBeDisabled();
  },
};

/* ─── 6. EdgeNumericProps — finite/integer normalization ───────────────── */
export const EdgeNumericProps: Story = {
  name: "Edge numeric props (fractional / NaN)",
  args: {
    // Fractional page + (below) a NaN slipped into the args: every numeric
    // prop must be finite-guarded + integer-coerced before it reaches the
    // page buttons, the summary, onPageChange, or buildPageItems.
    page: 2.5,
    pageSize: 10,
    total: 137,
    onPageChange: fn(),
  },
  parameters: {
    docs: {
      description: {
        story:
          "Hostile numeric props (`page={2.5}`, and a NaN forced into the " +
          "page-size args) must never leak fractional page labels or " +
          "`NaN` into the summary. They are truncated/finite-guarded first.",
      },
    },
  },
  render: (args) => (
    // Force a non-finite pageSize past the typed surface to prove the
    // runtime guard (TS types say number; a loose JS caller can pass NaN).
    <Pagination {...args} pageSize={NaN as unknown as number} />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const nav = canvas.getByRole("navigation", { name: "Pagination" });

    // Every rendered page button label is an integer string — no "2.5".
    const pageButtons = nav.querySelectorAll('[data-slot="pagination-page"]');
    await expect(pageButtons.length).toBeGreaterThan(0);
    for (const btn of Array.from(pageButtons)) {
      const label = btn.textContent?.trim() ?? "";
      await expect(label).toMatch(/^\d+$/);
    }

    // The summary carries no "NaN".
    const summary = nav.querySelector('[data-slot="pagination-summary"]');
    await expect(summary).not.toBeNull();
    await expect(summary?.textContent ?? "").not.toMatch(/NaN/);
  },
};

/* ─── 7. NoBoundary — trailing-gap end ellipsis ────────────────────────── */
export const NoBoundary: Story = {
  name: "No boundary (trailing-gap ellipsis)",
  args: {
    // boundaryCount={0} drops the pinned end pages; with the current page
    // mid-range the sibling window stops short of the last page, so the
    // trailing tail must collapse into an end ellipsis (not vanish).
    page: 5,
    pageSize: 10,
    total: 100, // → 10 pages
    siblingCount: 1,
    boundaryCount: 0,
    onPageChange: fn(),
  },
  parameters: {
    docs: {
      description: {
        story:
          "With `boundaryCount={0}` and a mid-range page, the sibling " +
          "window (4 5 6) does not reach the last page. The trailing pages " +
          "(7–10) collapse into a single end ellipsis instead of being " +
          "dropped silently.",
      },
    },
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const nav = canvas.getByRole("navigation", { name: "Pagination" });

    // Two ellipses must be present: a leading one (before the 4-5-6 window)
    // AND a trailing one for the unrepresented 7–10 tail. Pre-fix only the
    // leading ellipsis rendered — the trailing range was dropped silently.
    const ellipses = nav.querySelectorAll(
      '[data-slot="pagination-ellipsis"]',
    );
    await expect(ellipses.length).toBe(2);
    const last = ellipses[ellipses.length - 1];
    await expect(last).toHaveAttribute("aria-hidden", "true");

    // The trailing ellipsis sits AFTER the last sibling page (page 6) in DOM
    // order — i.e. it really represents the dropped tail, not a duplicate of
    // the leading gap.
    const page6 = canvas.getByRole("button", { name: "Go to page 6" });
    await expect(
      page6.compareDocumentPosition(last) &
        Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();

    // The sibling window still renders the current page.
    await expect(
      canvas.getByRole("button", { name: "Go to page 5" }),
    ).toBeInTheDocument();
  },
};
