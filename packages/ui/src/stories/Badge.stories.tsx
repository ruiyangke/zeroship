import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import { Badge, type BadgeIntent, type BadgeVariant } from "../components";

const meta: Meta<typeof Badge> = {
  title: "Components/Badge",
  component: Badge,
  parameters: { layout: "fullscreen" },
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

/* ─── 4. Long label ellipsizes (regression guard) ────────────────────── *
 *
 * A long label in a narrow container must CLAMP with an ellipsis, not
 * hard-clip. `text-overflow: ellipsis` is inert on the inline-flex root,
 * so the clamp lives on the inner `.zs-badge__label` block span. This
 * guard fails pre-fix (when the clamp sat on the flex root). */
export const LongLabelEllipsis: Story = {
  name: "Long label ellipsizes",
  parameters: {
    docs: {
      description: {
        story:
          "A long label in a narrow container clamps with an ellipsis via " +
          "the inner `.zs-badge__label` block span — `text-overflow` is " +
          "ignored on the inline-flex root, so the clamp must live on a " +
          "block descendant (mirrors Tag).",
      },
    },
  },
  render: () => (
    <div style={{ inlineSize: "6rem" }} data-testid="badge-clamp-container">
      <Badge intent="info" data-testid="badge-long">
        supercalifragilisticexpialidocious
      </Badge>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const badge = canvas.getByTestId("badge-long");
    const label = badge.querySelector<HTMLElement>(".zs-badge__label");
    await expect(label).not.toBeNull();
    if (!label) return;

    // The clamp is on the label span, not the flex root.
    await expect(getComputedStyle(label).textOverflow).toBe("ellipsis");
    await expect(getComputedStyle(label).overflow).toBe("hidden");

    // Content is actually clamped (text wider than the rendered box).
    await expect(label.scrollWidth).toBeGreaterThan(label.clientWidth);

    // And the badge does not overflow its narrow container.
    const container = canvas.getByTestId("badge-clamp-container");
    await expect(badge.getBoundingClientRect().width).toBeLessThanOrEqual(
      container.getBoundingClientRect().width + 1,
    );
  },
};

/* ─── 5. asChild long link ellipsizes ───────────────────────────────── */
export const AsChildLongLinkEllipsis: Story = {
  name: "asChild long link ellipsizes",
  parameters: {
    docs: {
      description: {
        story:
          "`asChild` routes the Badge class onto the child element, so " +
          "there is no inner `.zs-badge__label` span. The routed root " +
          "must still clamp long plain-text links inside narrow containers.",
      },
    },
  },
  render: () => (
    <div style={{ inlineSize: "7rem" }} data-testid="badge-aschild-clamp-box">
      <Badge asChild intent="info" data-testid="badge-aschild-long">
        <a href="#super-long-filter">supercalifragilisticexpialidocious</a>
      </Badge>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const badge = canvas.getByTestId("badge-aschild-long");
    const box = canvas.getByTestId("badge-aschild-clamp-box");
    await expect(badge.tagName).toBe("A");
    await expect(getComputedStyle(badge).textOverflow).toBe("ellipsis");
    await expect(getComputedStyle(badge).overflow).toBe("hidden");
    await expect(badge.getBoundingClientRect().width).toBeLessThanOrEqual(
      box.getBoundingClientRect().width + 1,
    );
  },
};
