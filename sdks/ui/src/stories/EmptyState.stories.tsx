import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import { EmptyState } from "../blocks";
import { Button } from "../components";

/* A decorative inline glyph for the demos — an outline "inbox". SVG
 * attribute units (viewBox space), not CSS px. */
const InboxGlyph = () => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
    <path d="M22 12h-6l-2 3h-4l-2-3H2" />
    <path d="M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z" />
  </svg>
);

const meta: Meta<typeof EmptyState> = {
  title: "Blocks/EmptyState",
  component: EmptyState,
  parameters: { layout: "fullscreen" },
};

export default meta;

type Story = StoryObj<typeof EmptyState>;

/* ─── 1. Default — icon + title + description + action ──────────────── */
export const Default: Story = {
  name: "Default (icon + title + description + action)",
  parameters: {
    docs: {
      description: {
        story:
          "The ergonomic-prop form: pass `icon`, `title`, `description`, " +
          "and `action` and EmptyState lays them out as a centered " +
          "column. The title renders as an `<h2>`; the icon is " +
          "decorative (`aria-hidden`).",
      },
    },
  },
  render: () => (
    <EmptyState
      data-testid="empty-default"
      icon={<InboxGlyph />}
      title="No messages yet"
      description="When someone writes to you, their messages will appear here."
      action={<Button>Compose</Button>}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("empty-default");
    await expect(root).toHaveAttribute("data-slot", "empty-state");
    // The centered column carries the block's own slot label (the Center
    // asChild-merges `empty-state-column` onto the rendered column node) —
    // NOT the generic `center`/`stack` it would read pre-relabel.
    const column = root.querySelector("[data-slot='empty-state-column']");
    await expect(column).not.toBeNull();
    // Title is a real h2 heading; icon is not in the AT tree.
    const heading = canvas.getByRole("heading", { name: /no messages yet/i });
    await expect(heading.tagName).toBe("H2");
    // Empty is NOT an alert.
    await expect(canvas.queryByRole("alert")).not.toBeInTheDocument();
    await expect(
      canvas.getByRole("button", { name: /compose/i }),
    ).toBeInTheDocument();
  },
};

/* ─── 2. Title only ────────────────────────────────────────────────── */
export const TitleOnly: Story = {
  name: "Title only",
  parameters: {
    docs: {
      description: {
        story: "The minimal form — just a `title`. Renders a lone heading.",
      },
    },
  },
  render: () => (
    <EmptyState data-testid="empty-title-only" title="No results found" />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(
      canvas.getByRole("heading", { name: /no results found/i }),
    ).toBeInTheDocument();
  },
};

/* ─── 3. Compound form ─────────────────────────────────────────────── */
export const Compound: Story = {
  name: "Compound parts",
  parameters: {
    docs: {
      description: {
        story:
          "Full control via compound parts: `EmptyState.Icon` / `.Title` " +
          "/ `.Description` / `.Actions`. Here the title is releveled to " +
          "`<h3>` via `Title asChild` to match a deeper outline.",
      },
    },
  },
  render: () => (
    <EmptyState data-testid="empty-compound">
      <EmptyState.Icon>
        <InboxGlyph />
      </EmptyState.Icon>
      <EmptyState.Title asChild>
        <h3>Your inbox is clear</h3>
      </EmptyState.Title>
      <EmptyState.Description>
        Nothing needs your attention right now.
      </EmptyState.Description>
      <EmptyState.Actions>
        <Button variant="tinted">Refresh</Button>
      </EmptyState.Actions>
    </EmptyState>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const heading = canvas.getByRole("heading", { name: /your inbox is clear/i });
    // asChild releveled the heading to h3.
    await expect(heading.tagName).toBe("H3");
    await expect(
      canvas.getByRole("button", { name: /refresh/i }),
    ).toBeInTheDocument();
  },
};

/* ─── DoubleHeading (dev-warn guard) ────────────────────────────────────
 * Regression for the dev-only warning when BOTH the ergonomic `title` prop
 * AND a compound `<EmptyState.Title>` child are supplied. They are additive,
 * so two same-level headings land in the document outline — a real a11y
 * outline defect. The block emits a `console.warn` telling the author to use
 * one or the other. We install a console.warn spy in beforeEach (the warn
 * renders two same-level headings. The dev-only `console.warn` telling the
 * author to use one or the other is compiled out of the production build, so
 * it cannot be asserted via the test-runner gate; this is a behavioral guard
 * that the dual-mode footgun produces two headings. Tagged `!autodocs`; the
 * heading-order axe rule is disabled because the duplicate outline is the
 * very defect this story demonstrates. */
export const DoubleHeading: Story = {
  tags: ["!autodocs"],
  parameters: {
    a11y: {
      // The double heading is the intentionally-invalid state under test.
      config: { rules: [{ id: "heading-order", enabled: false }] },
    },
    docs: {
      description: {
        story:
          "Regression: both a `title` prop AND an <EmptyState.Title> child " +
          "are supplied, rendering two same-level headings. The block emits " +
          "a dev-only `console.warn` (not observable in the prod build) " +
          "telling the author to use one or the other.",
      },
    },
  },
  render: () => (
    <EmptyState data-testid="empty-state-double-heading" title="Prop title">
      <EmptyState.Title>Compound title</EmptyState.Title>
    </EmptyState>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Both headings render — prop content first, then the compound child.
    // This documents the dual-mode double-heading footgun the dev-warn flags.
    const names = canvas.getAllByRole("heading").map((h) => h.textContent);
    await expect(names).toEqual(["Prop title", "Compound title"]);
  },
};
