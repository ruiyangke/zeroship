import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import { Split } from "../layouts";

const panel = (label: string) => (
  <div
    style={{
      background: "var(--zs-fill-secondary)",
      padding: "var(--zs-space-4)",
      borderRadius: "var(--zs-radius-3)",
      minBlockSize: "var(--zs-space-10)",
    }}
  >
    {label}
  </div>
);

const meta: Meta<typeof Split> = {
  title: "Layouts/Split",
  component: Split,
  parameters: { layout: "fullscreen" },
};

export default meta;

type Story = StoryObj<typeof Split>;

/* ─── 1. Side start — default ───────────────────────────────────────── */
export const SideStart: Story = {
  name: "Side start (default)",
  parameters: {
    docs: {
      description: {
        story:
          "Default `side=\"start\"`: a fixed-width rail on the inline-start " +
          "edge, fluid main filling the rest. DOM order is always Side- " +
          "then-Main. Layout-only — no roles; wrap with semantic elements " +
          "(or `asChild`) when those are wanted.",
      },
    },
  },
  render: () => (
    <Split sideWidth="12rem" gap={3} data-testid="split-start">
      <Split.Side data-testid="split-start-side">{panel("Side")}</Split.Side>
      <Split.Main data-testid="split-start-main">{panel("Main")}</Split.Main>
    </Split>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("split-start");
    await expect(root).toHaveAttribute("data-slot", "split");
    await expect(root).toHaveAttribute("data-side", "start");
    await expect(getComputedStyle(root).flexDirection).toBe("row");
    await expect(canvas.getByTestId("split-start-side")).toHaveAttribute(
      "data-slot",
      "split-side",
    );
    await expect(canvas.getByTestId("split-start-main")).toHaveAttribute(
      "data-slot",
      "split-main",
    );
  },
};

/* ─── 2. Side end — visual reversal ─────────────────────────────────── */
export const SideEnd: Story = {
  name: "Side end (visual reversal)",
  parameters: {
    docs: {
      description: {
        story:
          "`side=\"end\"` puts the rail on the inline-end edge via " +
          "`flex-direction: row-reverse` — the DOM/reading order stays " +
          "Side-then-Main, only the visual order flips. The `play()` " +
          "asserts the computed direction is `row-reverse`.",
      },
    },
  },
  render: () => (
    <Split side="end" sideWidth="12rem" gap={3} data-testid="split-end">
      <Split.Side>{panel("Side")}</Split.Side>
      <Split.Main>{panel("Main")}</Split.Main>
    </Split>
  ),
  play: async ({ canvasElement }) => {
    const root = within(canvasElement).getByTestId("split-end");
    await expect(root).toHaveAttribute("data-side", "end");
    await expect(getComputedStyle(root).flexDirection).toBe("row-reverse");
  },
};

/* ─── 3. Collapse below ─────────────────────────────────────────────── */
export const CollapseBelow: Story = {
  name: "Collapse below (md)",
  parameters: {
    docs: {
      description: {
        story:
          "`collapseBelow=\"md\"` stacks the rail above the main into a " +
          "column below the `--zs-bp-md` (48rem) breakpoint. Narrow the " +
          "viewport below 48rem to see it stack.",
      },
    },
  },
  render: () => (
    <Split sideWidth="14rem" gap={3} collapseBelow="md" data-testid="split-collapse">
      <Split.Side>{panel("Side (stacks on top below md)")}</Split.Side>
      <Split.Main>{panel("Main")}</Split.Main>
    </Split>
  ),
  play: async ({ canvasElement }) => {
    const root = within(canvasElement).getByTestId("split-collapse");
    await expect(root).toHaveAttribute("data-collapse", "md");
  },
};

/* ─── 4. asChild parts ──────────────────────────────────────────────── */
export const AsChildParts: Story = {
  name: "asChild parts (nav + section)",
  parameters: {
    docs: {
      description: {
        story:
          "Both parts accept `asChild`: render the Side as a `<nav>` " +
          "landmark and the Main as a `<section>` via `Slot`, so the split " +
          "carries real semantics without extra wrappers. In a real app " +
          "the Main is typically `<main>`; the demo uses `<section>` so the " +
          "isolated story stays free of landmark conflicts.",
      },
    },
  },
  render: () => (
    <Split sideWidth="12rem" gap={3}>
      <Split.Side asChild>
        <nav data-testid="split-aschild-side" aria-label="Sidebar">
          {panel("nav")}
        </nav>
      </Split.Side>
      <Split.Main asChild>
        <section data-testid="split-aschild-main" aria-label="Main content">
          {panel("section")}
        </section>
      </Split.Main>
    </Split>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const side = canvas.getByTestId("split-aschild-side");
    await expect(side.tagName).toBe("NAV");
    await expect(side).toHaveClass("zs-split__side");
    const main = canvas.getByTestId("split-aschild-main");
    await expect(main.tagName).toBe("SECTION");
    await expect(main).toHaveClass("zs-split__main");
  },
};

/* ─── 5. asChild root ───────────────────────────────────────────────── */
export const AsChildRoot: Story = {
  name: "asChild root (render-as section)",
  parameters: {
    docs: {
      description: {
        story:
          "The Split root also accepts `asChild`: render the split " +
          "container as the single child element (here a `<section>`) " +
          "via `Slot`, with the Side/Main parts nested inside — no wrapper " +
          "`<div>`. The `play()` asserts the root tag swap + `zs-split` " +
          "class.",
      },
    },
  },
  render: () => (
    <Split asChild sideWidth="12rem" gap={3}>
      <section data-testid="split-aschild-root" aria-label="Split region">
        <Split.Side>{panel("Side")}</Split.Side>
        <Split.Main>{panel("Main")}</Split.Main>
      </section>
    </Split>
  ),
  play: async ({ canvasElement }) => {
    const root = within(canvasElement).getByTestId("split-aschild-root");
    await expect(root.tagName).toBe("SECTION");
    await expect(root).toHaveClass("zs-split");
  },
};

/* ─── 6. Oversized rail in a narrow container (regression) ──────────── */
export const OversizedRailNarrowContainer: Story = {
  name: "Oversized rail, narrow container (regression)",
  parameters: {
    docs: {
      description: {
        story:
          "Regression for the rigid-rail fix: a `sideWidth` (32rem ≈ 512px) " +
          "far wider than the wrapper (280px). The rail is `flex: 0 1 " +
          "min(<sideWidth>, 100%)` (shrinkable, capped at the container) " +
          "with `min-inline-size: 0`, so the oversized basis caps to the " +
          "280px box instead of blowing past it. The `play()` asserts the " +
          "rail is capped to the container (its width never exceeds the " +
          "wrapper) AND there is no horizontal overflow on the split. " +
          "Pre-fix the rigid `flex: 0 0 <sideWidth>` rail forced the row to " +
          "the full 512rem basis and overflowed the box.",
      },
    },
  },
  render: () => (
    // The oversized rail is the subject. Its content (a short label) fits
    // within the capped 280px rail; Main fills whatever the rail leaves.
    // Both parts carry `min-inline-size: 0`, so the row never exceeds the
    // 280px wrapper — the rail's 32rem basis caps to `100%`.
    <div style={{ inlineSize: "280px" }}>
      <Split sideWidth="32rem" gap={3} data-testid="split-oversized">
        <Split.Side data-testid="split-oversized-side">Rail</Split.Side>
        <Split.Main />
      </Split>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("split-oversized");
    const side = canvas.getByTestId("split-oversized-side");
    // The rail's 32rem (≈512px) basis caps to the 280px container — its
    // rendered width never exceeds the wrapper. Pre-fix the rigid basis
    // forced it to ~512px.
    await expect(side.getBoundingClientRect().width).toBeLessThanOrEqual(
      root.getBoundingClientRect().width + 1,
    );
    // And the split as a whole does not overflow horizontally.
    await expect(root.scrollWidth).toBeLessThanOrEqual(root.clientWidth + 1);
  },
};
