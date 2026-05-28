import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, within } from "@storybook/test";
import { useState } from "react";
import { Card, Collapsible } from "../components";

const meta: Meta<typeof Collapsible> = {
  title: "Components/Collapsible",
  component: Collapsible,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Collapsible>;

/* ─── 1. Basic — uncontrolled, closed at first paint ───────────────── */
export const Basic: Story = {
  name: "Basic",
  parameters: {
    docs: {
      description: {
        story:
          "Minimal Collapsible — closed on mount, opens when the Trigger " +
          "is clicked. `aria-expanded` on the Trigger flips with the open " +
          "state; the Panel carries `hidden` while closed.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic">
      <Collapsible data-testid="collapsible-basic">
        <Collapsible.Trigger data-testid="collapsible-basic-trigger">
          Show advanced options
        </Collapsible.Trigger>
        <Collapsible.Panel data-testid="collapsible-basic-panel">
          Advanced options reveal here. Use them when the defaults do not
          fit your deployment.
        </Collapsible.Panel>
      </Collapsible>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const trigger = canvas.getByRole("button", {
      name: /show advanced options/i,
    });

    await expect(trigger).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(trigger);
    await expect(trigger).toHaveAttribute("aria-expanded", "true");
    await expect(
      canvas.getByText(/advanced options reveal here/i),
    ).toBeVisible();

    await userEvent.click(trigger);
    await expect(trigger).toHaveAttribute("aria-expanded", "false");
  },
};

/* ─── 2. Controlled — external state drives open ───────────────────── */
export const Controlled: Story = {
  name: "Controlled",
  parameters: {
    docs: {
      description: {
        story:
          "Controlled Collapsible — external state drives `open` and " +
          "`onOpenChange`. The state reads out in the live readout so " +
          "the contract is observable.",
      },
    },
  },
  render: function Render() {
    const [open, setOpen] = useState<boolean>(true);
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Controlled"
        style={{ flexDirection: "column", alignItems: "stretch" }}
      >
        <Collapsible
          open={open}
          onOpenChange={setOpen}
          data-testid="collapsible-controlled"
        >
          <Collapsible.Trigger
            data-testid="collapsible-controlled-trigger"
          >
            Release notes
          </Collapsible.Trigger>
          <Collapsible.Panel data-testid="collapsible-controlled-panel">
            v1.4.0 lands native asChild on every primitive, a smaller
            forced-colors palette, and the new Accordion + Collapsible
            duo.
          </Collapsible.Panel>
        </Collapsible>
        <output
          aria-live="polite"
          data-testid="collapsible-controlled-readout"
          style={{
            marginBlockStart: "var(--zs-space-3)",
            fontSize: "var(--zs-text-caption-1-size)",
            color: "var(--zs-label-secondary)",
          }}
        >
          Open: {String(open)}
        </output>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const trigger = canvas.getByRole("button", { name: /release notes/i });
    const readout = canvas.getByText(/open: true/i);

    await expect(trigger).toHaveAttribute("aria-expanded", "true");
    await userEvent.click(trigger);
    await expect(trigger).toHaveAttribute("aria-expanded", "false");
    await expect(readout).toHaveTextContent("Open: false");

    await userEvent.click(trigger);
    await expect(trigger).toHaveAttribute("aria-expanded", "true");
    await expect(readout).toHaveTextContent("Open: true");
  },
};

/* ─── 3. Disabled — root cascade ───────────────────────────────────── */
export const Disabled: Story = {
  name: "Disabled",
  parameters: {
    docs: {
      description: {
        story:
          "`disabled` on the Root cascades to the Trigger so the " +
          "affordance reads inactive. The Trigger no longer responds to " +
          "clicks; the Panel stays in whatever state it was in.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled">
      <Collapsible disabled data-testid="collapsible-disabled">
        <Collapsible.Trigger data-testid="collapsible-disabled-trigger">
          Disabled disclosure
        </Collapsible.Trigger>
        <Collapsible.Panel data-testid="collapsible-disabled-panel">
          You cannot toggle this open or closed from the Trigger.
        </Collapsible.Panel>
      </Collapsible>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const trigger = canvas.getByRole("button", {
      name: /disabled disclosure/i,
    });

    await expect(trigger).toBeDisabled();
    await expect(trigger).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(trigger);
    await expect(trigger).toHaveAttribute("aria-expanded", "false");
    await expect(trigger).not.toHaveFocus();
  },
};

/* ─── 3a. RegressionTransitions — fix #1 regression baseline ──────── *
 *
 * Dedicated to the fix #1 regression check on Collapsible. No play()
 * — Storybook autoplay does not race the aria-wiring script. The
 * story renders closed; the script clicks once to open (measures the
 * settled open height), then clicks again to close and samples the
 * in-flight block-size. */
export const RegressionTransitions: Story = {
  name: "Regression — transitions (fix #1)",
  parameters: {
    docs: {
      description: {
        story:
          "Regression baseline for the Slice 14 Collapsible fix #1 " +
          "panel-transition check. No play() — the aria-wiring script " +
          "drives the click itself and samples mid-transition block-size.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Regression transitions"
    >
      <Collapsible data-testid="collapsible-regression-transitions">
        <Collapsible.Trigger
          data-testid="collapsible-regression-transitions-trigger"
        >
          Show advanced options
        </Collapsible.Trigger>
        <Collapsible.Panel
          data-testid="collapsible-regression-transitions-panel"
        >
          Advanced options reveal here. Use them when the defaults do not
          fit your deployment.
        </Collapsible.Panel>
      </Collapsible>
    </div>
  ),
};

/* ─── 4. InsideCard — Collapsible sits inside a Card ───────────────── */
export const InsideCard: Story = {
  name: "Inside Card",
  parameters: {
    docs: {
      description: {
        story:
          "Collapsible inside a Card — a common pattern for compact " +
          "settings rows. The Trigger's padding sits inside the Card's " +
          "content rhythm so the disclosure aligns with the surrounding " +
          "ink.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Inside card">
      <Card style={{ inlineSize: "28rem" }}>
        <Card.Header>
          <Card.Title>Notification preferences</Card.Title>
          <Card.Description>
            Choose which events trigger an email.
          </Card.Description>
        </Card.Header>
        <Card.Content>
          <Collapsible
            defaultOpen
            data-testid="collapsible-in-card"
          >
            <Collapsible.Trigger data-testid="collapsible-in-card-trigger">
              Advanced filters
            </Collapsible.Trigger>
            <Collapsible.Panel data-testid="collapsible-in-card-panel">
              Filter by event type, source, and severity. Filters apply
              to both real-time and digest delivery.
            </Collapsible.Panel>
          </Collapsible>
        </Card.Content>
      </Card>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const trigger = canvas.getByRole("button", {
      name: /advanced filters/i,
    });

    await expect(trigger).toHaveAttribute("aria-expanded", "true");
    await userEvent.click(trigger);
    await expect(trigger).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(trigger);
    await expect(
      canvas.getByText(/filter by event type/i),
    ).toBeVisible();
  },
};

/* ─── 5. RTL — mirrored layout ─────────────────────────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Collapsible in RTL — the chevron mirrors automatically (it " +
          "lives at the inline-end via flex justify-content; no manual " +
          "flip needed). All padding stays balanced via logical props.",
      },
    },
  },
  render: () => (
    <div
      dir="rtl"
      className="zs-story-row"
      role="group"
      aria-label="RTL"
    >
      <Collapsible defaultOpen data-testid="collapsible-rtl">
        <Collapsible.Trigger data-testid="collapsible-rtl-trigger">
          הצג אפשרויות מתקדמות
        </Collapsible.Trigger>
        <Collapsible.Panel data-testid="collapsible-rtl-panel">
          האפשרויות המתקדמות נחשפות כאן. השתמש בהן כאשר ברירות המחדל
          אינן מתאימות לפריסה שלך.
        </Collapsible.Panel>
      </Collapsible>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const trigger = canvas.getByRole("button", {
      name: /הצג אפשרויות מתקדמות/i,
    });

    await expect(trigger).toHaveAttribute("aria-expanded", "true");
    await userEvent.click(trigger);
    await expect(trigger).toHaveAttribute("aria-expanded", "false");
  },
};
