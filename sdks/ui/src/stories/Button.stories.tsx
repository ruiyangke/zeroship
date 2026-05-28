import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, within } from "@storybook/test";
import { Button } from "../components/Button";

const meta: Meta<typeof Button> = {
  title: "Components/Button",
  component: Button,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Button>;

export const AllStyles: Story = {
  name: "All styles",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All button styles">
      <div className="zs-story-cell">
        <span className="zs-story-label">Filled</span>
        <Button variant="filled">Save</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Tinted</span>
        <Button variant="tinted">Continue</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Gray</span>
        <Button variant="gray">More</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Plain</span>
        <Button variant="plain">Cancel</Button>
      </div>
    </div>
  ),
};

export const AllSizes: Story = {
  name: "All sizes",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All button sizes">
      <div className="zs-story-cell">
        <span className="zs-story-label">Small</span>
        <Button size="small">Save</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Medium</span>
        <Button size="medium">Save</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Large</span>
        <Button size="large">Save</Button>
      </div>
    </div>
  ),
};

export const AllStates: Story = {
  name: "All states",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All button states">
      <div className="zs-story-cell">
        <span className="zs-story-label">Default</span>
        <Button>Save</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Disabled</span>
        <Button disabled>Save</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Loading</span>
        <Button loading>Save</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Tinted disabled</span>
        <Button variant="tinted" disabled>Continue</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Plain loading</span>
        <Button variant="plain" loading>Cancel</Button>
      </div>
    </div>
  ),
};

export const Destructive: Story = {
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Destructive buttons">
      <div className="zs-story-cell">
        <span className="zs-story-label">Filled</span>
        <Button variant="filled" intent="destructive">Delete</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Tinted</span>
        <Button variant="tinted" intent="destructive">Delete</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Gray</span>
        <Button variant="gray" intent="destructive">Delete</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Plain</span>
        <Button variant="plain" intent="destructive">Delete</Button>
      </div>
    </div>
  ),
};

export const DestructiveDisabled: Story = {
  name: "Destructive disabled",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Destructive disabled buttons across variants"
    >
      <div className="zs-story-cell">
        <span className="zs-story-label">Filled</span>
        <Button variant="filled" intent="destructive" disabled>Delete</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Tinted</span>
        <Button variant="tinted" intent="destructive" disabled>Delete</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Gray</span>
        <Button variant="gray" intent="destructive" disabled>Delete</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Plain</span>
        <Button variant="plain" intent="destructive" disabled>Delete</Button>
      </div>
    </div>
  ),
};

export const LoadingDestructive: Story = {
  name: "Loading destructive",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Destructive buttons in loading state"
    >
      <div className="zs-story-cell">
        <span className="zs-story-label">Filled</span>
        <Button variant="filled" intent="destructive" loading>Deleting…</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Tinted</span>
        <Button variant="tinted" intent="destructive" loading>Deleting…</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Gray</span>
        <Button variant="gray" intent="destructive" loading>Deleting…</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Plain</span>
        <Button variant="plain" intent="destructive" loading>Deleting…</Button>
      </div>
    </div>
  ),
};

/* Minimal inline icon glyphs (16x16 viewBox). aria-hidden so the
   button's name still comes from the label. */
function IconArrow() {
  return (
    <svg
      width="16"
      height="16"
      viewBox="0 0 16 16"
      aria-hidden="true"
      focusable="false"
    >
      <path
        d="M3 8h9M8 3l5 5-5 5"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

function IconPlus() {
  return (
    <svg
      width="16"
      height="16"
      viewBox="0 0 16 16"
      aria-hidden="true"
      focusable="false"
    >
      <path
        d="M8 3v10M3 8h10"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
      />
    </svg>
  );
}

export const WithSlots: Story = {
  name: "With slots",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Buttons with icons">
      <div className="zs-story-cell">
        <span className="zs-story-label">Start slot</span>
        <Button variant="filled" startSlot={<IconPlus />}>New project</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">End slot</span>
        <Button variant="tinted" endSlot={<IconArrow />}>Continue</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Both slots</span>
        <Button variant="gray" startSlot={<IconPlus />} endSlot={<IconArrow />}>
          Open
        </Button>
      </div>
    </div>
  ),
};

export const LongLabel: Story = {
  name: "Long label",
  parameters: {
    docs: {
      description: {
        story:
          "When the parent layout constrains the button width, the label " +
          "should ellipsize gracefully via `min-width: 0` + `overflow: " +
          "hidden` + `text-overflow: ellipsis` on `.zs-button__label`. " +
          "The button itself has no `max-width`; consumers decide " +
          "constraints.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Long-label ellipsis behavior"
    >
      <div className="zs-story-cell" style={{ maxWidth: "16rem", display: "block" }}>
        <span className="zs-story-label">Constrained 16rem</span>
        <Button variant="filled" style={{ display: "flex", maxWidth: "100%" }}>
          A very long label that should ellipsize gracefully when the parent
          constrains the button's width
        </Button>
      </div>
      <div className="zs-story-cell" style={{ maxWidth: "20rem", display: "block" }}>
        <span className="zs-story-label">Constrained 20rem</span>
        <Button variant="tinted" style={{ display: "flex", maxWidth: "100%" }}>
          A very long label that should ellipsize gracefully when the parent
          constrains the button's width
        </Button>
      </div>
    </div>
  ),
};

export const FocusVisible: Story = {
  name: "Focus visible",
  parameters: {
    docs: {
      description: {
        story:
          "Press Tab to move focus across the buttons. The focus ring should " +
          "appear via :focus-visible (keyboard) but NOT on click.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Focus-visible focus ring demo"
    >
      <div className="zs-story-cell">
        <span className="zs-story-label">Filled</span>
        {/* eslint-disable-next-line jsx-a11y/no-autofocus */}
        <Button variant="filled" autoFocus>Save</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Tinted</span>
        <Button variant="tinted">Continue</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Gray</span>
        <Button variant="gray">More</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Plain</span>
        <Button variant="plain">Cancel</Button>
      </div>
    </div>
  ),
};

export const ClickInteraction: Story = {
  name: "Click interaction (play)",
  parameters: {
    docs: {
      description: {
        story:
          "Exemplar `play()` story. Demonstrates the convention for " +
          "interaction tests: query by accessible name via " +
          "`within(canvasElement).getByRole`, drive with `userEvent`, " +
          "assert with `expect`. See `.storybook/CONVENTIONS.md`.",
      },
    },
  },
  render: () => <Button variant="filled">Click me</Button>,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const button = canvas.getByRole("button", { name: /click me/i });
    await userEvent.click(button);
    await expect(button).toHaveFocus();
  },
};

export const AsChild: Story = {
  name: "As child (anchor)",
  parameters: {
    docs: {
      description: {
        story:
          "`asChild` swaps the rendered element to the single child while " +
          "keeping Button styling. Use for anchors like \"Learn more\" or " +
          "\"Open in browser\" that should look like buttons but follow " +
          "native anchor semantics (Enter/click activate; no Space).",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Button rendered as anchor via asChild"
    >
      <div className="zs-story-cell">
        <span className="zs-story-label">Filled link</span>
        <Button asChild variant="filled">
          <a href="#open">Open in browser</a>
        </Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Plain link</span>
        <Button asChild variant="plain">
          <a href="#learn">Learn more</a>
        </Button>
      </div>
    </div>
  ),
};
