import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import { Cluster } from "../layouts";

/* Chip-like demo item — tokens only. */
const chip = (label: string) => (
  <span
    key={label}
    style={{
      background: "var(--zs-fill-secondary)",
      padding: "var(--zs-space-1) var(--zs-space-3)",
      borderRadius: "var(--zs-radius-full)",
      whiteSpace: "nowrap",
    }}
  >
    {label}
  </span>
);

const meta: Meta<typeof Cluster> = {
  title: "Layouts/Cluster",
  component: Cluster,
  parameters: { layout: "padded" },
};

export default meta;

type Story = StoryObj<typeof Cluster>;

/* ─── 1. Default — wrapping chips ───────────────────────────────────── */
export const Default: Story = {
  name: "Default (wrapping chips)",
  parameters: {
    docs: {
      description: {
        story:
          "Canonical use: a bag of chips/tags that wrap as the container " +
          "narrows. Non-interactive visual wrap — for an interactive group " +
          "with roving tabindex, use `Toolbar`. Defaults: `gap=2`, " +
          "`align=center`, `justify=start`.",
      },
    },
  },
  render: () => (
    <Cluster
      data-testid="cluster-default"
      style={{ maxInlineSize: "var(--zs-container-sm)" }}
    >
      {[
        "TypeScript",
        "React",
        "Rust",
        "compio",
        "io_uring",
        "PostgreSQL",
        "V8",
        "Storybook",
      ].map(chip)}
    </Cluster>
  ),
  play: async ({ canvasElement }) => {
    const cluster = within(canvasElement).getByTestId("cluster-default");
    await expect(cluster).toHaveAttribute("data-slot", "cluster");
    await expect(getComputedStyle(cluster).flexWrap).toBe("wrap");
  },
};

/* ─── 2. Justify between ────────────────────────────────────────────── */
export const JustifyBetween: Story = {
  name: "Justify between",
  parameters: {
    docs: {
      description: {
        story:
          "`justify=\"between\"` spreads items across the line (e.g. a " +
          "metadata row with leading tags and a trailing action).",
      },
    },
  },
  render: () => (
    <Cluster
      justify="between"
      data-testid="cluster-between"
      style={{ inlineSize: "var(--zs-container-sm)" }}
    >
      {chip("Draft")}
      {chip("Edited 2h ago")}
      {chip("3 comments")}
    </Cluster>
  ),
};

/* ─── 3. asChild ────────────────────────────────────────────────────── */
export const AsChild: Story = {
  name: "asChild (render-as ul)",
  parameters: {
    docs: {
      description: {
        story:
          "`asChild` renders the Cluster as the single child element " +
          "(here a `<ul>`) via `Slot`, with no wrapper div.",
      },
    },
  },
  render: () => (
    <Cluster asChild gap={2}>
      <ul
        data-testid="cluster-aschild"
        aria-label="Tags"
        style={{ margin: 0, padding: 0, listStyle: "none" }}
      >
        <li>{chip("alpha")}</li>
        <li>{chip("beta")}</li>
        <li>{chip("gamma")}</li>
      </ul>
    </Cluster>
  ),
  play: async ({ canvasElement }) => {
    const ul = within(canvasElement).getByTestId("cluster-aschild");
    await expect(ul.tagName).toBe("UL");
    await expect(ul).toHaveClass("zs-cluster");
  },
};
