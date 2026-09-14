import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import {
  Bell,
  Check,
  ChevronRight,
  Search,
  Settings,
  Trash2,
} from "lucide-react";
import { Button, Icon } from "../components";

const meta: Meta<typeof Icon> = {
  title: "Components/Icon",
  component: Icon,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Icon>;

/* ─── 1. Sizes — one icon at sm / md / lg ───────────────────────────── */
export const Sizes: Story = {
  name: "Sizes",
  parameters: {
    docs: {
      description: {
        story:
          "The same icon at the three token sizes. Each maps to a " +
          "`--zs-icon-{sm,md,lg}` foundation token via CSS — no px prop " +
          "is forwarded to Lucide, so sizing stays token-pure.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Icon sizes"
      style={{ alignItems: "center", gap: "1.5rem" }}
    >
      <Icon as={Settings} size="sm" data-testid="icon-sm" />
      <Icon as={Settings} size="md" data-testid="icon-md" />
      <Icon as={Settings} size="lg" data-testid="icon-lg" />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const sm = canvas.getByTestId("icon-sm");
    const md = canvas.getByTestId("icon-md");
    const lg = canvas.getByTestId("icon-lg");
    await expect(sm).toHaveClass("zs-icon", "zs-icon--sm");
    await expect(md).toHaveClass("zs-icon", "zs-icon--md");
    await expect(lg).toHaveClass("zs-icon", "zs-icon--lg");
    // Decorative by default — no label passed.
    await expect(md).toHaveAttribute("aria-hidden", "true");
    await expect(md).toHaveAttribute("data-slot", "icon");
    await expect(md).toHaveAttribute("focusable", "false");
  },
};

/* ─── 2. Inline with text — currentColor + alignment ────────────────── */
export const InlineWithText: Story = {
  name: "Inline with text",
  parameters: {
    docs: {
      description: {
        story:
          "Icon sitting beside its own text. It inherits the surrounding " +
          "`currentColor` and `vertical-align: middle` keeps it on the " +
          "text baseline-box. The icon is decorative (the word carries " +
          "the meaning), so it gets `aria-hidden`.",
      },
    },
  },
  render: () => (
    <p
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: "0.5rem",
        color: "var(--zs-system-green)",
      }}
    >
      <Icon as={Check} data-testid="icon-inline" />
      Saved
    </p>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const icon = canvas.getByTestId("icon-inline");
    await expect(icon).toHaveAttribute("aria-hidden", "true");
    await expect(icon).toHaveAttribute("data-slot", "icon");
  },
};

/* ─── 3. In a Button — decorative beside a label ────────────────────── */
export const InAButton: Story = {
  name: "In a Button",
  parameters: {
    docs: {
      description: {
        story:
          "Icon used inside the real Button beside its text label. The " +
          "Button name comes from its text, so the icon stays decorative " +
          "(`aria-hidden`) and inherits the Button's `currentColor`.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Icons in buttons"
      style={{ alignItems: "center", gap: "0.75rem" }}
    >
      <Button>
        <Icon as={Search} size="sm" data-testid="icon-button" />
        Search
      </Button>
      <Button intent="destructive" variant="tinted">
        <Icon as={Trash2} size="sm" />
        Delete
      </Button>
      <Button variant="plain">
        Next
        <Icon as={ChevronRight} size="sm" />
      </Button>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const icon = canvas.getByTestId("icon-button");
    await expect(icon).toHaveAttribute("aria-hidden", "true");
    await expect(icon).toHaveAttribute("data-slot", "icon");
    // The accessible name of the button comes from its text, not the icon.
    await expect(
      canvas.getByRole("button", { name: "Search" }),
    ).toBeInTheDocument();
  },
};

/* ─── 4. Labelled — meaningful vs decorative a11y ───────────────────── */
export const Labelled: Story = {
  name: "Labelled",
  parameters: {
    docs: {
      description: {
        story:
          "When `label` is set the icon is meaningful: `role=\"img\"` + " +
          "`aria-label`, so AT announces it. With no `label` it is " +
          "decorative: `aria-hidden`. Use a labelled icon only when the " +
          "icon is the sole carrier of meaning (e.g. an icon-only " +
          "notification affordance).",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Labelled vs decorative"
      style={{ alignItems: "center", gap: "1.5rem" }}
    >
      <Icon as={Bell} label="Notifications" data-testid="icon-labelled" />
      <Icon as={Bell} data-testid="icon-decorative" />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    // Labelled → meaningful: role="img" + accessible name, NOT hidden.
    const labelled = canvas.getByTestId("icon-labelled");
    await expect(labelled).toHaveAttribute("role", "img");
    await expect(labelled).toHaveAttribute("aria-label", "Notifications");
    await expect(labelled).not.toHaveAttribute("aria-hidden");
    await expect(labelled).toHaveAttribute("data-slot", "icon");
    // Resolves through the accessibility tree by its name.
    await expect(
      canvas.getByRole("img", { name: "Notifications" }),
    ).toBe(labelled);

    // No label → decorative: aria-hidden, no role.
    const decorative = canvas.getByTestId("icon-decorative");
    await expect(decorative).toHaveAttribute("aria-hidden", "true");
    await expect(decorative).not.toHaveAttribute("role");
    await expect(decorative).toHaveAttribute("data-slot", "icon");
  },
};
