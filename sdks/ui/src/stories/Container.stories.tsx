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
          "Each size maps to a `--zs-container-*` token. `full` opts out " +
          "of any max-width (`none`).",
      },
    },
  },
  render: () => (
    <div style={{ display: "flex", flexDirection: "column", gap: "var(--zs-space-3)" }}>
      <Container size="sm" data-testid="container-sm">{fill("sm — 30rem")}</Container>
      <Container size="md" data-testid="container-md">{fill("md — 48rem")}</Container>
      <Container size="lg" data-testid="container-lg">{fill("lg — 64rem")}</Container>
      <Container size="xl" data-testid="container-xl">{fill("xl — 80rem")}</Container>
      <Container size="full" data-testid="container-full">{fill("full — no max-width")}</Container>
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
