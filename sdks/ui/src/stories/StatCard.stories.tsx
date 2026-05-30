import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import { StatCard } from "../blocks";

const meta: Meta<typeof StatCard> = {
  title: "Blocks/StatCard",
  component: StatCard,
  parameters: { layout: "fullscreen" },
  argTypes: {
    variant: {
      control: "inline-radio",
      options: ["surface", "elevated", "outline", "ghost"],
    },
    size: { control: "inline-radio", options: ["sm", "md", "lg"] },
  },
};

export default meta;

type Story = StoryObj<typeof StatCard>;

// A small trend glyph for the icon stories — a decorative inline SVG.
const TrendIcon = () => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.5}>
    <path d="M3 17l6-6 4 4 8-8" strokeLinecap="round" strokeLinejoin="round" />
    <path d="M21 7h-5M21 7v5" strokeLinecap="round" strokeLinejoin="round" />
  </svg>
);

/* ─── 1. Up delta ────────────────────────────────────────────────────── */
export const Up: Story = {
  name: "Delta up (increase)",
  parameters: {
    docs: {
      description: {
        story:
          "An `up` delta tints green AND renders a ▲ glyph plus a " +
          "visually-hidden \"increased\" word, so the direction never " +
          "relies on color alone (WCAG 1.4.1). Screen readers announce " +
          "\"increased 12%\".",
      },
    },
  },
  render: () => (
    <StatCard
      data-testid="statcard-up"
      label="Monthly revenue"
      value="$48,120"
      delta={{ value: "12%", direction: "up" }}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("statcard-up");
    // Regression: StatCard overrides the Card root data-slot. Pre-fix
    // Card spread its own `data-slot="card"` after `...rest`, clobbering
    // this to "card"; post-fix Card honors the consumer-supplied value.
    await expect(root).toHaveAttribute("data-slot", "stat-card");
    const delta = root.querySelector('[data-slot="stat-card-delta"]');
    await expect(delta).toHaveAttribute("data-direction", "up");
    // Direction is conveyed beyond color: the SR word is present.
    await expect(delta).toHaveTextContent(/increased/i);
  },
};

/* ─── 2. Down delta ──────────────────────────────────────────────────── */
export const Down: Story = {
  name: "Delta down (decrease)",
  parameters: {
    docs: {
      description: {
        story:
          "A `down` delta tints red, renders a ▼ glyph, and prepends a " +
          "visually-hidden \"decreased\" — \"decreased 4%\" to AT.",
      },
    },
  },
  render: () => (
    <StatCard
      label="Churn rate"
      value="4.0%"
      delta={{ value: "4%", direction: "down" }}
    />
  ),
};

/* ─── 3. Flat delta ──────────────────────────────────────────────────── */
export const Flat: Story = {
  name: "Delta flat (no change)",
  parameters: {
    docs: {
      description: {
        story:
          "A `flat` delta is neutral (label-secondary), renders an " +
          "em-dash glyph, and announces \"no change\".",
      },
    },
  },
  render: () => (
    <StatCard
      label="Active sessions"
      value="1,204"
      delta={{ value: "0%", direction: "flat" }}
    />
  ),
};

/* ─── 4. With icon ───────────────────────────────────────────────────── */
export const WithIcon: Story = {
  name: "With decorative icon",
  parameters: {
    docs: {
      description: {
        story:
          "The optional `icon` sits in the label row, wrapped " +
          "aria-hidden — the label carries the accessible meaning.",
      },
    },
  },
  render: () => (
    <StatCard
      label="New signups"
      value="312"
      delta={{ value: "8%", direction: "up" }}
      icon={<TrendIcon />}
    />
  ),
};

/* ─── 5. No delta ────────────────────────────────────────────────────── */
export const ValueOnly: Story = {
  name: "Label + value only",
  parameters: {
    docs: {
      description: {
        story:
          "The minimal tile — label + value, no change indicator. The " +
          "value is a styled `<div>`, not a heading, so it does not " +
          "pollute the document outline.",
      },
    },
  },
  render: () => (
    <StatCard label="Total storage" value="2.4 TB" />
  ),
};
