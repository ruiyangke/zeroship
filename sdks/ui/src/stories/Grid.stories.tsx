import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import { Grid } from "../layouts";

const box = (label: string) => (
  <div
    key={label}
    style={{
      background: "var(--zs-fill-secondary)",
      padding: "var(--zs-space-3)",
      borderRadius: "var(--zs-radius-2)",
      textAlign: "center",
    }}
  >
    {label}
  </div>
);

const meta: Meta<typeof Grid> = {
  title: "Layouts/Grid",
  component: Grid,
  parameters: { layout: "fullscreen" },
};

export default meta;

type Story = StoryObj<typeof Grid>;

/* ─── 1. Intrinsic — minColWidth (preferred) ────────────────────────── */
export const Intrinsic: Story = {
  name: "Intrinsic (minColWidth — preferred)",
  parameters: {
    docs: {
      description: {
        story:
          "The preferred, breakpoint-free path: `minColWidth` becomes " +
          "`repeat(auto-fit, minmax(<minColWidth>, 1fr))` so the grid " +
          "packs as many equal columns as fit and reflows fluidly with " +
          "no media queries. `columns` is ignored when set.",
      },
    },
  },
  render: () => (
    <Grid minColWidth="10rem" gap={3} data-testid="grid-intrinsic">
      {Array.from({ length: 6 }, (_, i) => box(`Cell ${i + 1}`))}
    </Grid>
  ),
  play: async ({ canvasElement }) => {
    const grid = within(canvasElement).getByTestId("grid-intrinsic");
    await expect(grid).toHaveAttribute("data-slot", "grid");
    await expect(getComputedStyle(grid).display).toBe("grid");
  },
};

/* ─── 2. Fixed count ────────────────────────────────────────────────── */
export const FixedCount: Story = {
  name: "Fixed count (columns number)",
  parameters: {
    docs: {
      description: {
        story:
          "`columns={3}` fixes three equal-fraction columns at every " +
          "width. Use when a constant column count is the intent.",
      },
    },
  },
  render: () => (
    <Grid columns={3} gap={3} data-testid="grid-fixed">
      {Array.from({ length: 6 }, (_, i) => box(`Cell ${i + 1}`))}
    </Grid>
  ),
  play: async ({ canvasElement }) => {
    const grid = within(canvasElement).getByTestId("grid-fixed");
    await expect(getComputedStyle(grid).display).toBe("grid");
    // Three explicit tracks → three values in grid-template-columns.
    const tracks = getComputedStyle(grid)
      .gridTemplateColumns.split(" ")
      .filter(Boolean);
    await expect(tracks.length).toBe(3);
  },
};

/* ─── 3. Responsive columns object ──────────────────────────────────── */
export const Responsive: Story = {
  name: "Responsive ({ sm, md, lg })",
  parameters: {
    docs: {
      description: {
        story:
          "A `{ sm, md, lg }` object changes the column count at the " +
          "governed `--zs-bp-*` breakpoints via media queries. It is " +
          "mobile-first: the base (below `sm`) is always 1 and each " +
          "provided breakpoint promotes the count upward — so `{ lg: 4 }` " +
          "is 1 column until `lg`, then 4. Resize the viewport to see the " +
          "count change.",
      },
    },
  },
  render: () => (
    <Grid
      columns={{ sm: 2, md: 3, lg: 4 }}
      gap={3}
      data-testid="grid-responsive"
    >
      {Array.from({ length: 8 }, (_, i) => box(`Cell ${i + 1}`))}
    </Grid>
  ),
  play: async ({ canvasElement }) => {
    const grid = within(canvasElement).getByTestId("grid-responsive");
    await expect(grid).toHaveAttribute("data-responsive");
    await expect(getComputedStyle(grid).display).toBe("grid");
  },
};

/* ─── 4. Flow dense ─────────────────────────────────────────────────── */
export const FlowDense: Story = {
  name: "Flow dense",
  parameters: {
    docs: {
      description: {
        story:
          "`flow=\"dense\"` sets `grid-auto-flow: dense` so the grid " +
          "backfills earlier gaps with later items.",
      },
    },
  },
  render: () => (
    <Grid columns={4} gap={2} flow="dense" data-testid="grid-dense">
      {Array.from({ length: 10 }, (_, i) => box(`${i + 1}`))}
    </Grid>
  ),
};

/* ─── 5. asChild ────────────────────────────────────────────────────── */
export const AsChild: Story = {
  name: "asChild (render-as section)",
  parameters: {
    docs: {
      description: {
        story:
          "`asChild` routes through `Slot` so the Grid renders-as the " +
          "single child element (here a `<section>`) instead of a `<div>` " +
          "— no wrapper; refs/className/style compose under React 19.",
      },
    },
  },
  render: () => (
    <Grid asChild columns={3} gap={3}>
      <section data-testid="grid-aschild" aria-label="Card grid">
        {Array.from({ length: 6 }, (_, i) => box(`Cell ${i + 1}`))}
      </section>
    </Grid>
  ),
  play: async ({ canvasElement }) => {
    const section = within(canvasElement).getByTestId("grid-aschild");
    await expect(section.tagName).toBe("SECTION");
    await expect(section).toHaveClass("zs-grid");
  },
};

/* ─── 6. Responsive base-1 (regression) ─────────────────────────────── */
export const ResponsiveBaseIsOne: Story = {
  name: "Responsive base is 1 (regression)",
  parameters: {
    docs: {
      description: {
        story:
          "Regression for the Wave-1 fix: a `columns` object's base " +
          "`--grid-cols` is always 1 (mobile-first), never the smallest " +
          "provided value. `{ lg: 4 }` must base to 1 column and promote " +
          "to 4 only at `--zs-bp-lg`. The `play()` asserts the base custom " +
          "property is `\"1\"` — viewport-independent, so it reads the base " +
          "token rather than the media-query result. Pre-fix this read " +
          "`\"4\"`.",
      },
    },
  },
  render: () => (
    <Grid columns={{ lg: 4 }} gap={3} data-testid="grid-resp">
      {Array.from({ length: 4 }, (_, i) => box(`Cell ${i + 1}`))}
    </Grid>
  ),
  play: async ({ canvasElement }) => {
    const grid = within(canvasElement).getByTestId("grid-resp");
    await expect(
      getComputedStyle(grid).getPropertyValue("--grid-cols").trim(),
    ).toBe("1");
  },
};
