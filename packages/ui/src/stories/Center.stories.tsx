import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import { Center } from "../layouts";

const card = (label: string) => (
  <div
    style={{
      background: "var(--zs-fill-secondary)",
      padding: "var(--zs-space-4)",
      borderRadius: "var(--zs-radius-3)",
      textAlign: "center",
    }}
  >
    {label}
  </div>
);

const meta: Meta<typeof Center> = {
  title: "Layouts/Center",
  component: Center,
  parameters: { layout: "fullscreen" },
};

export default meta;

type Story = StoryObj<typeof Center>;

/* ─── 1. Both axes — default ────────────────────────────────────────── */
export const BothAxes: Story = {
  name: "Both axes (default)",
  parameters: {
    docs: {
      description: {
        story:
          "Default: centers content horizontally AND vertically. The " +
          "canonical use is an empty/loading/error state in the middle of " +
          "its region. `minHeight` gives the box room to center within.",
      },
    },
  },
  render: () => (
    <Center minHeight="60dvh" data-testid="center-both">
      {card("Centered on both axes")}
    </Center>
  ),
  play: async ({ canvasElement }) => {
    const c = within(canvasElement).getByTestId("center-both");
    await expect(c).toHaveAttribute("data-slot", "center");
    const cs = getComputedStyle(c);
    await expect(cs.alignItems).toBe("center");
    await expect(cs.justifyContent).toBe("center");
  },
};

/* ─── 2. Inline — horizontal only ───────────────────────────────────── */
export const Inline: Story = {
  name: "Inline (horizontal only)",
  parameters: {
    docs: {
      description: {
        story:
          "`inline` drops vertical centering — content is centered " +
          "horizontally and pinned to the top. Use when the Center has no " +
          "defined height. The `play()` asserts `align-items: flex-start`.",
      },
    },
  },
  render: () => (
    <Center inline data-testid="center-inline">
      {card("Horizontally centered, top-aligned")}
    </Center>
  ),
  play: async ({ canvasElement }) => {
    const c = within(canvasElement).getByTestId("center-inline");
    await expect(c).toHaveAttribute("data-inline");
    const cs = getComputedStyle(c);
    await expect(cs.alignItems).toBe("flex-start");
    await expect(cs.justifyContent).toBe("center");
  },
};

/* ─── 3. asChild ────────────────────────────────────────────────────── */
export const AsChild: Story = {
  name: "asChild (render-as section)",
  parameters: {
    docs: {
      description: {
        story:
          "`asChild` renders the Center as the single child element " +
          "(here a `<section>`) via `Slot`, with no wrapper div.",
      },
    },
  },
  render: () => (
    <Center asChild minHeight="40dvh">
      <section data-testid="center-aschild" aria-label="Empty state">
        {card("Empty state section")}
      </section>
    </Center>
  ),
  play: async ({ canvasElement }) => {
    const section = within(canvasElement).getByTestId("center-aschild");
    await expect(section.tagName).toBe("SECTION");
    await expect(section).toHaveClass("zs-center");
  },
};

/* ─── 4. Wide child stays contained (regression) ────────────────────── */
export const WideChildContained: Story = {
  name: "Wide child contained",
  parameters: {
    docs: {
      description: {
        story:
          "Shows this component behavior with realistic content and keeps the edge case easy to inspect.",
      },
    },
  },
  render: () => (
    <div style={{ inlineSize: "240px" }}>
      <Center minHeight="20dvh" data-testid="center-wide">
        <div
          style={{
            background: "var(--zs-fill-secondary)",
            padding: "var(--zs-space-3)",
            borderRadius: "var(--zs-radius-2)",
            overflowWrap: "anywhere",
          }}
        >
          aVeryLongUnbreakableTokenThatWouldOtherwiseSpillOutBothSides
        </div>
      </Center>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const c = within(canvasElement).getByTestId("center-wide");
    await expect(c.scrollWidth).toBeLessThanOrEqual(c.clientWidth + 1);
  },
};
