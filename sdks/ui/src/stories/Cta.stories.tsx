import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, within } from "@storybook/test";
import { Cta } from "../sections";
import { Button } from "../components";

const meta: Meta<typeof Cta> = {
  title: "Sections/Cta",
  component: Cta,
  parameters: { layout: "fullscreen" },
  argTypes: {
    tone: {
      control: "inline-radio",
      options: ["default", "muted", "accent"],
      description:
        "Full-bleed band tone (the shared page-rhythm system): default " +
        "(transparent), muted (subtle surface panel), accent (accent fill " +
        "with ink remapped to accent-ink — the bold closing band).",
    },
  },
};

export default meta;

type Story = StoryObj<typeof Cta>;

/* ─── 1. Default — eyebrow + title + description + 2 Buttons, centered ────── */
export const Default: Story = {
  name: "Default (eyebrow + title + description + 2 buttons, centered)",
  parameters: {
    docs: {
      description: {
        story:
          "The default band: an eyebrow + `<h2>` title + description above a " +
          "centered actions `Cluster` of two `<Button>`s. The section is " +
          "labelled by the real `<h2>` title.",
      },
    },
  },
  render: () => (
    <Cta
      data-testid="cta-default"
      eyebrow="Ready when you are"
      title="Ship your idea today"
      description="Describe what you want. AI builds it. We host, scale, and bill for you."
      actions={
        <>
          <Button>Get started</Button>
          <Button variant="gray">Talk to sales</Button>
        </>
      }
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("cta-default");
    await expect(root.tagName).toBe("SECTION");
    await expect(root).toHaveAttribute("data-slot", "cta");

    // The title is a real <h2> and the section is labelled by it.
    const title = canvas.getByRole("heading", { name: "Ship your idea today" });
    await expect(title.tagName).toBe("H2");
    await expect(root.getAttribute("aria-labelledby")).toBe(title.id);

    // A CTA Button is present + clickable.
    const cta = canvas.getByRole("button", { name: "Get started" });
    await userEvent.click(cta);
    await expect(cta).toBeEnabled();
  },
};

/* ─── 2. Tinted — variant="tinted" accent-tint panel ─────────────────────── */
export const Tinted: Story = {
  name: "Tinted (variant=tinted, accent-tint panel)",
  parameters: {
    docs: {
      description: {
        story:
          "The `tinted` variant wraps the body in a rounded accent-tint panel " +
          "(a contained CTA card) via the house `color-mix` idiom.",
      },
    },
  },
  render: () => (
    <Cta
      data-testid="cta-tinted"
      variant="tinted"
      eyebrow="Limited beta"
      title="Join the waitlist"
      description="Be first in line when we open the doors."
      actions={<Button>Request access</Button>}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("cta-tinted");
    await expect(root).toHaveAttribute("data-variant", "tinted");
    // The panel carries the tinted treatment marker.
    const panel = root.querySelector('[data-slot="cta-panel"]');
    await expect(panel).toHaveAttribute("data-variant", "tinted");
    const title = canvas.getByRole("heading", { name: "Join the waitlist" });
    await expect(root.getAttribute("aria-labelledby")).toBe(title.id);
  },
};

/* ─── 3. StartAligned — align="start" ────────────────────────────────────── */
export const StartAligned: Story = {
  name: "StartAligned (align=start)",
  parameters: {
    docs: {
      description: {
        story:
          "Start-aligned band (`align=\"start\"`): the column + actions hug " +
          "the inline-start edge.",
      },
    },
  },
  render: () => (
    <Cta
      data-testid="cta-start"
      align="start"
      title="Start building"
      description="No credit card required."
      actions={<Button>Create your first app</Button>}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("cta-start");
    await expect(root).toHaveAttribute("data-align", "start");
    const title = canvas.getByRole("heading", { name: "Start building" });
    await expect(title.tagName).toBe("H2");
    await expect(root.getAttribute("aria-labelledby")).toBe(title.id);
  },
};

/* ─── 5. Accent — tone="accent" bold closing band ────────────────────────── */
export const Accent: Story = {
  name: "Accent (tone=accent, bold closing band)",
  parameters: {
    docs: {
      description: {
        story:
          "The `accent` tone fills the WHOLE band with `--zs-accent` and remaps " +
          "every inner ink (eyebrow / title / description) to `--zs-accent-ink` " +
          "for the bold closing statement — the same accent-ink-on-accent " +
          "pairing Button.filled ships axe-clean. The default (`filled`) CTA " +
          "Button INVERTS on the accent band — it becomes a solid accent-ink " +
          "chip with accent text, a true high-contrast inverse action (an " +
          "un-inverted accent-filled button would vanish into the band). " +
          "Distinct from `variant=\"tinted\"`, which paints a contained card on " +
          "the measure rather than the full band.",
      },
    },
  },
  render: () => (
    <Cta
      data-testid="cta-accent"
      tone="accent"
      eyebrow="Ready when you are"
      title="Ship your idea today"
      description="Describe what you want. AI builds it. We host, scale, and bill for you."
      actions={<Button>Get started</Button>}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("cta-accent");
    await expect(root).toHaveAttribute("data-tone", "accent");
    // Labelled by the real <h2>; the eyebrow carries the shared class.
    const title = canvas.getByRole("heading", { name: "Ship your idea today" });
    await expect(root.getAttribute("aria-labelledby")).toBe(title.id);
    await expect(root.querySelector(".zs-section-eyebrow")).not.toBeNull();
    // The CTA Button is present + enabled.
    const cta = canvas.getByRole("button", { name: "Get started" });
    await userEvent.click(cta);
    await expect(cta).toBeEnabled();
  },
};

/* ─── 4. Minimal — title + 1 Button ──────────────────────────────────────── */
export const Minimal: Story = {
  name: "Minimal (title + 1 button)",
  parameters: {
    docs: {
      description: {
        story:
          "A minimal CTA: just a title and a single `<Button>`. The section " +
          "is still labelled by the real `<h2>`. (CTA always carries a title, " +
          "but the aria-labelledby is still gated on the heading rendering — " +
          "an empty title would never dangle a reference.)",
      },
    },
  },
  render: () => (
    <Cta
      data-testid="cta-minimal"
      title="Ready to ship?"
      actions={<Button>Get started</Button>}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("cta-minimal");
    const title = canvas.getByRole("heading", { name: "Ready to ship?" });
    await expect(title.tagName).toBe("H2");
    await expect(root.getAttribute("aria-labelledby")).toBe(title.id);
    // The single CTA Button is present.
    await expect(
      canvas.getByRole("button", { name: "Get started" }),
    ).toBeInTheDocument();
    // No description/eyebrow rendered.
    await expect(
      root.querySelector('[data-slot="cta-description"]'),
    ).toBeNull();
    await expect(root.querySelector('[data-slot="cta-eyebrow"]')).toBeNull();
  },
};
