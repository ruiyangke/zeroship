import type { Meta, StoryObj } from "@storybook/react";
import { Badge, type BadgeIntent, type BadgeVariant } from "../blocks";

const meta: Meta<typeof Badge> = {
  title: "Blocks/Badge",
  component: Badge,
  parameters: { layout: "centered" },
  argTypes: {
    intent: {
      control: "inline-radio",
      options: ["neutral", "info", "success", "warning", "danger"],
    },
    variant: {
      control: "inline-radio",
      options: ["solid", "soft", "outline"],
    },
    size: { control: "inline-radio", options: ["sm", "md"] },
  },
};

export default meta;

type Story = StoryObj<typeof Badge>;

const INTENTS: BadgeIntent[] = [
  "neutral",
  "info",
  "success",
  "warning",
  "danger",
];
const VARIANTS: BadgeVariant[] = ["solid", "soft", "outline"];

/* ─── 1. Matrix — every intent × variant ─────────────────────────────── */
export const Matrix: Story = {
  name: "Matrix (intent × variant)",
  parameters: {
    docs: {
      description: {
        story:
          "The full 5 intents × 3 variants grid. Every cell paints an " +
          "OPAQUE background base — even `soft` and `outline` — so axe's " +
          "color-contrast walk resolves against a known surface. Contrast " +
          "is the whole point of a status chip, so every cell must be " +
          "axe-clean.",
      },
    },
  },
  render: () => (
    <div
      style={{
        display: "grid",
        gridTemplateColumns: "auto repeat(3, 1fr)",
        gap: "0.75rem",
        alignItems: "center",
      }}
    >
      <span />
      {VARIANTS.map((v) => (
        <strong
          key={v}
          style={{ fontSize: "0.75rem", textAlign: "center" }}
        >
          {v}
        </strong>
      ))}
      {INTENTS.map((intent) => (
        <Row key={intent} intent={intent} />
      ))}
    </div>
  ),
};

function Row({ intent }: { intent: BadgeIntent }) {
  return (
    <>
      <strong style={{ fontSize: "0.75rem" }}>{intent}</strong>
      {VARIANTS.map((variant) => (
        <div
          key={variant}
          style={{ display: "flex", justifyContent: "center" }}
        >
          <Badge
            intent={intent}
            variant={variant}
            data-testid={`badge:${intent}:${variant}`}
          >
            {intent}
          </Badge>
        </div>
      ))}
    </>
  );
}

/* ─── 2. Sizes ───────────────────────────────────────────────────────── */
export const Sizes: Story = {
  name: "Sizes (sm / md)",
  parameters: {
    docs: {
      description: {
        story:
          "`sm` uses caption type with tight padding; `md` (default) uses " +
          "footnote type. Both keep the pill radius.",
      },
    },
  },
  render: () => (
    <div style={{ display: "flex", gap: "1rem", alignItems: "center" }}>
      <Badge size="sm" intent="info" data-testid="badge-size-sm">
        small
      </Badge>
      <Badge size="md" intent="info" data-testid="badge-size-md">
        medium
      </Badge>
    </div>
  ),
};

/* ─── 3. asChild (link) ──────────────────────────────────────────────── */
export const AsChildLink: Story = {
  name: "asChild (link)",
  parameters: {
    docs: {
      description: {
        story:
          "`asChild` renders the Badge as its single child element (here " +
          "an `<a>`) while keeping the Badge styling — useful for a chip " +
          "that links to a filtered view.",
      },
    },
  },
  render: () => (
    <Badge asChild intent="success" data-testid="badge-aschild">
      <a href="#deployed">deployed</a>
    </Badge>
  ),
};
