import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import { Button, Separator } from "../components";

const meta: Meta<typeof Separator> = {
  title: "Components/Separator",
  component: Separator,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Separator>;

/* ─── 1. Horizontal — default ───────────────────────────────────────── */
export const Horizontal: Story = {
  name: "Horizontal (default)",
  parameters: {
    docs: {
      description: {
        story:
          "Default horizontal hairline cut. `decorative=true` by default " +
          "so the line carries `role=\"none\"` + `aria-hidden=\"true\"` — " +
          "AT skips it.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Horizontal separator"
      style={{ flexDirection: "column", alignItems: "stretch", gap: "0" }}
    >
      <div style={{ padding: "0.5rem 0" }}>Row above</div>
      <Separator data-testid="separator-horizontal" />
      <div style={{ padding: "0.5rem 0" }}>Row below</div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const sep = canvas.getByTestId("separator-horizontal");
    await expect(sep).toHaveAttribute("data-orientation", "horizontal");
    await expect(sep).toHaveAttribute("role", "none");
    await expect(sep).toHaveAttribute("aria-hidden", "true");
  },
};

/* ─── 2. Vertical — between inline content ──────────────────────────── */
export const Vertical: Story = {
  name: "Vertical",
  parameters: {
    docs: {
      description: {
        story:
          "Vertical cut between inline content. `align-self: stretch` so " +
          "the line fills the parent row height without an explicit " +
          "size.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Vertical separator"
      style={{ alignItems: "center", gap: "1rem" }}
    >
      <span style={{ padding: "0.5rem 0" }}>Item A</span>
      <Separator orientation="vertical" data-testid="separator-vertical" />
      <span style={{ padding: "0.5rem 0" }}>Item B</span>
      <Separator
        orientation="vertical"
        data-testid="separator-vertical-2"
      />
      <span style={{ padding: "0.5rem 0" }}>Item C</span>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const sep = canvas.getByTestId("separator-vertical");
    await expect(sep).toHaveAttribute("data-orientation", "vertical");
    await expect(sep).toHaveAttribute("aria-hidden", "true");
  },
};

/* ─── 3. Hairline — explicit variant ────────────────────────────────── */
export const Hairline: Story = {
  name: "Hairline variant",
  parameters: {
    docs: {
      description: {
        story:
          "Explicit `variant=\"hairline\"` (the default). Paints a " +
          "one-device-pixel line via `--zs-selection-hairline`.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Hairline"
      style={{ flexDirection: "column", alignItems: "stretch", gap: "0" }}
    >
      <Separator
        variant="hairline"
        data-testid="separator-hairline"
      />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const sep = canvas.getByTestId("separator-hairline");
    await expect(sep).toHaveAttribute("data-variant", "hairline");
  },
};

/* ─── 4. Thick — heavier section cut ────────────────────────────────── */
export const Thick: Story = {
  name: "Thick variant",
  parameters: {
    docs: {
      description: {
        story:
          "`variant=\"thick\"` paints a `--zs-space-1` (0.25rem) bar — " +
          "reserved for major section cuts where a hairline would read " +
          "as not-quite-finished.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Thick"
      style={{ flexDirection: "column", alignItems: "stretch", gap: "0" }}
    >
      <Separator variant="thick" data-testid="separator-thick" />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const sep = canvas.getByTestId("separator-thick");
    await expect(sep).toHaveAttribute("data-variant", "thick");
  },
};

/* ─── 5. NotDecorative — role=separator path ────────────────────────── */
export const NotDecorative: Story = {
  name: "Not decorative (role=separator)",
  parameters: {
    docs: {
      description: {
        story:
          "`decorative={false}` defers to Base UI's Separator which emits " +
          "`role=\"separator\"` + `aria-orientation`. Use when the cut " +
          "carries meaning AT users should hear (between unrelated " +
          "groups in a complex page).",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Semantic separator"
      style={{ flexDirection: "column", alignItems: "stretch", gap: "0" }}
    >
      <Separator
        decorative={false}
        data-testid="separator-semantic-horizontal"
      />
      <div style={{ height: "1rem" }} />
      <Separator
        decorative={false}
        orientation="vertical"
        data-testid="separator-semantic-vertical"
        style={{ blockSize: "2rem" }}
      />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const h = canvas.getByTestId("separator-semantic-horizontal");
    await expect(h).toHaveAttribute("role", "separator");
    await expect(h).toHaveAttribute("aria-orientation", "horizontal");
    const v = canvas.getByTestId("separator-semantic-vertical");
    await expect(v).toHaveAttribute("role", "separator");
    await expect(v).toHaveAttribute("aria-orientation", "vertical");
  },
};

/* ─── 6. InsideList — between rows ──────────────────────────────────── */
export const InsideList: Story = {
  name: "Inside list (between rows)",
  parameters: {
    docs: {
      description: {
        story:
          "Canonical use: a Separator between rows in a list. The " +
          "Separator owns no margin; the parent's row spacing carries " +
          "the rhythm.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="List with separators"
      style={{
        flexDirection: "column",
        alignItems: "stretch",
        gap: "0",
        padding: "0",
        inlineSize: "20rem",
      }}
    >
      <div
        style={{ padding: "0.75rem 1rem" }}
        data-testid="separator-list-row-1"
      >
        Row 1
      </div>
      <Separator />
      <div
        style={{ padding: "0.75rem 1rem" }}
        data-testid="separator-list-row-2"
      >
        Row 2
      </div>
      <Separator />
      <div
        style={{ padding: "0.75rem 1rem" }}
        data-testid="separator-list-row-3"
      >
        Row 3
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByTestId("separator-list-row-1")).toBeInTheDocument();
    await expect(canvas.getByTestId("separator-list-row-3")).toBeInTheDocument();
  },
};

/* ─── 7. InsideToolbar — vertical between buttons ───────────────────── */
export const InsideToolbar: Story = {
  name: "Inside toolbar (vertical)",
  parameters: {
    docs: {
      description: {
        story:
          "Vertical Separator between toolbar groups. `align-self: " +
          "stretch` lets the line fill the row height automatically.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Toolbar separators"
      style={{ alignItems: "center", gap: "0.5rem" }}
    >
      <Button size="sm" variant="plain" data-testid="separator-tool-1">
        Bold
      </Button>
      <Button size="sm" variant="plain" data-testid="separator-tool-2">
        Italic
      </Button>
      <Separator
        orientation="vertical"
        data-testid="separator-toolbar-divider"
      />
      <Button size="sm" variant="plain" data-testid="separator-tool-3">
        Link
      </Button>
      <Button size="sm" variant="plain" data-testid="separator-tool-4">
        Image
      </Button>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const sep = canvas.getByTestId("separator-toolbar-divider");
    await expect(sep).toHaveAttribute("data-orientation", "vertical");
  },
};

/* ─── 8. RTL — logical-property regression ──────────────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "RTL containers — the vertical Separator's `border-inline-end` " +
          "edge flips automatically. The visual stays identical because " +
          "the Separator has no inline content; this story exists as a " +
          "regression net.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="RTL separators"
      dir="rtl"
      style={{ alignItems: "center", gap: "1rem" }}
    >
      <span>عنصر أ</span>
      <Separator
        orientation="vertical"
        data-testid="separator-rtl-vertical"
      />
      <span>عنصر ب</span>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const sep = canvas.getByTestId("separator-rtl-vertical");
    await expect(sep).toHaveAttribute("data-orientation", "vertical");
  },
};
