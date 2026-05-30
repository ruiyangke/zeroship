import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import { Container } from "../layouts";

/* Filled block so the constrained, centered column is visible. */
const fill = (label: string) => (
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

const meta: Meta<typeof Container> = {
  title: "Layouts/Container",
  component: Container,
  parameters: { layout: "fullscreen" },
};

export default meta;

type Story = StoryObj<typeof Container>;

/* ─── 1. Default (lg, centered) ─────────────────────────────────────── */
export const Default: Story = {
  name: "Default (lg, centered)",
  parameters: {
    docs: {
      description: {
        story:
          "Default `size=\"lg\"`, centered with `margin-inline: auto`, " +
          "`padX=4`. Container is the SOLE width authority — the only " +
          "primitive that sets `max-width`.",
      },
    },
  },
  render: () => (
    <Container data-testid="container-default">
      {fill("Centered lg column")}
    </Container>
  ),
  play: async ({ canvasElement }) => {
    const c = within(canvasElement).getByTestId("container-default");
    await expect(c).toHaveAttribute("data-slot", "container");
    await expect(c).toHaveAttribute("data-size", "lg");
  },
};

/* ─── 2. Sizes ──────────────────────────────────────────────────────── */
export const Sizes: Story = {
  name: "Sizes (sm / md / lg / xl / full)",
  parameters: {
    docs: {
      description: {
        story:
          "Each size maps to a `--zs-container-*` token (`full` opts out of " +
          "any max-width). `max-width` only CONSTRAINS, so the sizes only " +
          "diverge once the available width exceeds them — this demo lays " +
          "them out start-aligned inside a fixed-wide (88rem) frame so the " +
          "stepped caps read as a clear staircase regardless of the canvas " +
          "width (the panel scrolls on a narrow viewport rather than " +
          "clamping every size to the same width). Each row's fill carries " +
          "a start-edge accent rule so the column extent is unmistakable.",
      },
    },
  },
  render: () => (
    // Fixed-wide frame: guarantees room for every cap (full ≈ 88rem here) so
    // the sizes visibly differ even when the Storybook canvas is narrow.
    <div style={{ minInlineSize: "88rem", paddingBlock: "var(--zs-space-4)" }}>
      <div style={{ display: "flex", flexDirection: "column", gap: "var(--zs-space-3)" }}>
        {(
          [
            ["sm", "sm — 30rem"],
            ["md", "md — 48rem"],
            ["lg", "lg — 64rem"],
            ["xl", "xl — 80rem"],
            ["full", "full — no max-width"],
          ] as const
        ).map(([size, label]) => (
          <Container
            key={size}
            size={size}
            center={false}
            padX={0}
            data-testid={`container-${size}`}
          >
            <div
              style={{
                background: "var(--zs-fill-secondary)",
                borderInlineStart:
                  "var(--zs-space-half) solid var(--zs-accent)",
                padding: "var(--zs-space-3) var(--zs-space-4)",
                borderRadius: "var(--zs-radius-3)",
              }}
            >
              {label}
            </div>
          </Container>
        ))}
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const full = within(canvasElement).getByTestId("container-full");
    await expect(getComputedStyle(full).maxWidth).toBe("none");
  },
};

/* ─── 3. Not centered ───────────────────────────────────────────────── */
export const NotCentered: Story = {
  name: "Not centered (center=false)",
  parameters: {
    docs: {
      description: {
        story:
          "`center={false}` drops `margin-inline: auto` so the column " +
          "pins to the inline-start edge instead of centering.",
      },
    },
  },
  render: () => (
    <Container size="sm" center={false} data-testid="container-start">
      {fill("Pinned to inline-start")}
    </Container>
  ),
};

/* ─── 4. asChild ────────────────────────────────────────────────────── */
export const AsChild: Story = {
  name: "asChild (render-as section)",
  parameters: {
    docs: {
      description: {
        story:
          "`asChild` renders the Container as the single child element via " +
          "`Slot` — no wrapper div around the page content column. Shown " +
          "here as a `<section>`; in a real page you would render the " +
          "primary region as `<main>`. (The demo avoids `<main>` so the " +
          "isolated story stays free of landmark conflicts.)",
      },
    },
  },
  render: () => (
    <Container asChild size="md">
      <section data-testid="container-aschild">{fill("section element")}</section>
    </Container>
  ),
  play: async ({ canvasElement }) => {
    const el = within(canvasElement).getByTestId("container-aschild");
    await expect(el.tagName).toBe("SECTION");
    await expect(el).toHaveClass("zs-container");
  },
};
