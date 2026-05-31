import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import { Stack } from "../layouts";

/* Demo box — visualizes arrangement. Tokens only (no raw hex/px); the
 * background reads from a theme fill token and the padding/size from the
 * spacing scale so the box itself stays on-grid. */
const box = (label: string) => (
  <div
    key={label}
    style={{
      background: "var(--zs-fill-secondary)",
      padding: "var(--zs-space-3)",
      borderRadius: "var(--zs-radius-2)",
      minInlineSize: "var(--zs-space-10)",
      textAlign: "center",
    }}
  >
    {label}
  </div>
);

const meta: Meta<typeof Stack> = {
  title: "Layouts/Stack",
  component: Stack,
  parameters: { layout: "fullscreen" },
};

export default meta;

type Story = StoryObj<typeof Stack>;

/* ─── 1. Column — default ───────────────────────────────────────────── */
export const Column: Story = {
  name: "Column (default)",
  parameters: {
    docs: {
      description: {
        story:
          "Default `direction=\"column\"` stacks children vertically. " +
          "`gap` comes from the `--zs-space-*` scale — no arbitrary spacing.",
      },
    },
  },
  render: () => (
    <Stack gap={3} data-testid="stack-column">
      {box("One")}
      {box("Two")}
      {box("Three")}
    </Stack>
  ),
  play: async ({ canvasElement }) => {
    const stack = within(canvasElement).getByTestId("stack-column");
    await expect(stack).toHaveAttribute("data-slot", "stack");
    await expect(getComputedStyle(stack).flexDirection).toBe("column");
  },
};

/* ─── 2. Row — direction flip ───────────────────────────────────────── */
export const Row: Story = {
  name: "Row (direction flip)",
  parameters: {
    docs: {
      description: {
        story:
          "`direction=\"row\"` flips the main axis to horizontal. The " +
          "`play()` asserts the computed `flex-direction` follows the prop.",
      },
    },
  },
  render: () => (
    <Stack direction="row" gap={2} data-testid="stack-row">
      {box("One")}
      {box("Two")}
      {box("Three")}
    </Stack>
  ),
  play: async ({ canvasElement }) => {
    const stack = within(canvasElement).getByTestId("stack-row");
    await expect(getComputedStyle(stack).flexDirection).toBe("row");
  },
};

/* ─── 3. Align + justify ────────────────────────────────────────────── */
export const AlignJustify: Story = {
  name: "Align + justify",
  parameters: {
    docs: {
      description: {
        story:
          "Cross-axis `align` and main-axis `justify` flow through the " +
          "shared `alignValue`/`justifyValue` maps so they read the same " +
          "across every layout primitive.",
      },
    },
  },
  render: () => (
    <Stack
      direction="row"
      gap={2}
      align="center"
      justify="between"
      data-testid="stack-align"
      style={{ minBlockSize: "var(--zs-space-10)" }}
    >
      {box("Start")}
      {box("End")}
    </Stack>
  ),
};

/* ─── 4. Wrap ───────────────────────────────────────────────────────── */
export const Wrap: Story = {
  name: "Wrap",
  parameters: {
    docs: {
      description: {
        story:
          "`wrap` lets a row Stack break onto multiple lines when the " +
          "children overflow the container inline-size.",
      },
    },
  },
  render: () => (
    <Stack
      direction="row"
      gap={2}
      wrap
      data-testid="stack-wrap"
      style={{ maxInlineSize: "var(--zs-container-sm)" }}
    >
      {Array.from({ length: 8 }, (_, i) => box(`Item ${i + 1}`))}
    </Stack>
  ),
};

/* ─── 5. asChild ────────────────────────────────────────────────────── */
export const AsChild: Story = {
  name: "asChild (render-as nav)",
  parameters: {
    docs: {
      description: {
        story:
          "`asChild` routes through `Slot` so the Stack renders-as the " +
          "single child element (here a `<nav>`) instead of a `<div>` — " +
          "no wrapper, refs/className/style compose under React 19.",
      },
    },
  },
  render: () => (
    <Stack asChild gap={2}>
      <nav data-testid="stack-aschild" aria-label="Section links">
        {box("Home")}
        {box("About")}
      </nav>
    </Stack>
  ),
  play: async ({ canvasElement }) => {
    const nav = within(canvasElement).getByTestId("stack-aschild");
    await expect(nav.tagName).toBe("NAV");
    await expect(nav).toHaveClass("zs-stack");
  },
};

/* ─── 6. Row child stays contained (regression) ─────────────────────── */
export const RowChildContained: Story = {
  name: "Row child contained (regression)",
  parameters: {
    docs: {
      description: {
        story:
          "Regression for the row-child min-size fix: a row Stack holding a " +
          "wide block of text inside a narrow wrapper. " +
          "`.zs-stack > * { min-inline-size: 0; min-block-size: 0 }` lets " +
          "the flex child shrink below its min-content size so the long " +
          "breakable text wraps instead of forcing the Stack wider than its " +
          "parent. The `play()` asserts no horizontal overflow on the Stack. " +
          "Pre-fix the wide child blew the row out.",
      },
    },
  },
  render: () => (
    <div style={{ inlineSize: "260px" }}>
      <Stack direction="row" gap={2} data-testid="stack-row-contained">
        <div
          style={{
            margin: 0,
            background: "var(--zs-fill-secondary)",
            padding: "var(--zs-space-3)",
            borderRadius: "var(--zs-radius-2)",
          }}
        >
          A very long sentence of ordinary breakable words that would
          otherwise blow out the row stack when the wrapper is narrow.
        </div>
      </Stack>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const stack = within(canvasElement).getByTestId("stack-row-contained");
    await expect(stack.scrollWidth).toBeLessThanOrEqual(stack.clientWidth + 1);
  },
};
