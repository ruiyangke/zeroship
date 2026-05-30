import type { Meta, StoryObj } from "@storybook/react";
import { expect, fn, userEvent, within } from "@storybook/test";
import { Stepper, type StepperStep } from "../blocks";

const STEPS: StepperStep[] = [
  { id: "account", label: "Account", description: "Your details" },
  { id: "plan", label: "Plan", description: "Pick a tier" },
  { id: "billing", label: "Billing", description: "Payment method" },
  { id: "review", label: "Review", description: "Confirm & finish" },
];

const meta: Meta<typeof Stepper> = {
  title: "Blocks/Stepper",
  component: Stepper,
  parameters: { layout: "fullscreen" },
  argTypes: {
    orientation: {
      control: "inline-radio",
      options: ["horizontal", "vertical"],
    },
    current: { control: "number" },
  },
};

export default meta;

type Story = StoryObj<typeof Stepper>;

/* ─── 1. Horizontal (the default) ────────────────────────────────────── */
export const Horizontal: Story = {
  name: "Horizontal (current = 1)",
  parameters: {
    docs: {
      description: {
        story:
          "Four steps with `current={1}`: step 1 derives `complete` (accent " +
          "fill + Check), step 2 is `current` (accent ring, `aria-current=" +
          '"step"`), steps 3–4 are `upcoming` (muted). Status is conveyed ' +
          "beyond color by the check glyph + a visually-hidden status word.",
      },
    },
  },
  render: () => (
    <div style={{ padding: "var(--zs-space-6)" }}>
      <Stepper data-testid="stepper" steps={STEPS} current={1} />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("stepper");
    await expect(root.tagName).toBe("OL");
    await expect(root).toHaveAttribute("data-orientation", "horizontal");

    const steps = root.querySelectorAll('[data-slot="stepper-step"]');
    await expect(steps).toHaveLength(4);

    // Status derivation: index < 1 complete, === 1 current, > 1 upcoming.
    await expect(steps[0]).toHaveAttribute("data-status", "complete");
    await expect(steps[1]).toHaveAttribute("data-status", "current");
    await expect(steps[2]).toHaveAttribute("data-status", "upcoming");
    await expect(steps[3]).toHaveAttribute("data-status", "upcoming");

    // The CURRENT step carries aria-current="step"; no other step does.
    await expect(steps[1]).toHaveAttribute("aria-current", "step");
    await expect(steps[0]).not.toHaveAttribute("aria-current");

    // Color-not-alone: the complete step renders the Check glyph (a shape)
    // AND a visually-hidden "completed" word (the AT-announced signal).
    const check = steps[0].querySelector('[data-slot="icon"]');
    await expect(check).toBeInTheDocument();
    await expect(check).toHaveAttribute("aria-hidden", "true");
    await expect(steps[0]).toHaveTextContent(/completed/i);
    await expect(steps[1]).toHaveTextContent(/current step/i);
    await expect(steps[2]).toHaveTextContent(/upcoming/i);

    // Connectors are decorative.
    const connector = steps[0].querySelector(
      '[data-slot="stepper-connector"]',
    );
    await expect(connector).toHaveAttribute("aria-hidden", "true");

    // Static (non-clickable) — no buttons.
    await expect(
      root.querySelectorAll("button.zs-stepper__trigger"),
    ).toHaveLength(0);
  },
};

/* ─── 2. Vertical ────────────────────────────────────────────────────── */
export const Vertical: Story = {
  name: "Vertical (current = 2)",
  parameters: {
    docs: {
      description: {
        story:
          "The same steps stacked vertically with vertical connectors. " +
          "`current={2}` → steps 1–2 complete, step 3 current, step 4 " +
          "upcoming.",
      },
    },
  },
  render: () => (
    <div style={{ padding: "var(--zs-space-6)" }}>
      <Stepper
        data-testid="stepper"
        steps={STEPS}
        current={2}
        orientation="vertical"
      />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("stepper");
    await expect(root).toHaveAttribute("data-orientation", "vertical");
    const steps = root.querySelectorAll('[data-slot="stepper-step"]');
    await expect(steps[1]).toHaveAttribute("data-status", "complete");
    await expect(steps[2]).toHaveAttribute("data-status", "current");
    await expect(steps[2]).toHaveAttribute("aria-current", "step");
  },
};

/* ─── 3. Clickable ───────────────────────────────────────────────────── */
export const Clickable: Story = {
  name: "Clickable (onStepChange)",
  parameters: {
    docs: {
      description: {
        story:
          "Supplying `onStepChange` turns each step into a real " +
          "`<button>` — native keyboard focus + a focus-visible ring. " +
          "Clicking a step fires `onStepChange(id, index)`.",
      },
    },
  },
  args: { onStepChange: fn() },
  render: (args) => (
    <div style={{ padding: "var(--zs-space-6)" }}>
      <Stepper
        data-testid="stepper"
        steps={STEPS}
        current={1}
        onStepChange={args.onStepChange}
      />
    </div>
  ),
  play: async ({ args, canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("stepper");

    // Clickable a11y: every step's trigger is a real <button type="button">.
    const buttons = root.querySelectorAll("button.zs-stepper__trigger");
    await expect(buttons).toHaveLength(4);
    for (const btn of Array.from(buttons)) {
      await expect(btn).toHaveAttribute("type", "button");
    }

    // Firing one calls onStepChange with the step id + index.
    await userEvent.click(buttons[2]);
    await expect(args.onStepChange).toHaveBeenCalledWith("billing", 2);

    // Buttons are natively focusable (no role/tabIndex hand-rolling).
    (buttons[0] as HTMLButtonElement).focus();
    await expect(buttons[0]).toHaveFocus();
  },
};

/* ─── 4. Status override ─────────────────────────────────────────────── */
export const StatusOverride: Story = {
  name: "Explicit per-step status",
  parameters: {
    docs: {
      description: {
        story:
          "A step's own `status` pins it, bypassing index-vs-`current` " +
          "derivation — e.g. an error/skipped step rendered `upcoming` even " +
          "though it sits before the active step. Here step 2 is pinned " +
          "`upcoming` despite `current={3}`.",
      },
    },
  },
  render: () => (
    <div style={{ padding: "var(--zs-space-6)" }}>
      <Stepper
        data-testid="stepper"
        current={3}
        steps={[
          { id: "a", label: "Connect", status: "complete" },
          { id: "b", label: "Configure", status: "upcoming" },
          { id: "c", label: "Verify", status: "complete" },
          { id: "d", label: "Launch", status: "current" },
        ]}
      />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("stepper");
    const steps = root.querySelectorAll('[data-slot="stepper-step"]');
    // Pinned statuses win over derivation: index 1 is upcoming, not complete.
    await expect(steps[1]).toHaveAttribute("data-status", "upcoming");
    await expect(steps[3]).toHaveAttribute("data-status", "current");
    await expect(steps[3]).toHaveAttribute("aria-current", "step");
  },
};
