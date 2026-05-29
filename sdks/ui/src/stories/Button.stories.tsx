import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, within } from "@storybook/test";
import { useState } from "react";
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

export const DisabledAndLoadingAreInert: Story = {
  name: "Disabled and loading are inert (play)",
  render: function DisabledAndLoadingAreInertRender() {
    const [count, setCount] = useState(0);
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Disabled and loading button interactions"
      >
        <Button onClick={() => setCount((value) => value + 1)}>
          Increment
        </Button>
        <Button disabled onClick={() => setCount((value) => value + 10)}>
          Locked
        </Button>
        <Button loading onClick={() => setCount((value) => value + 100)}>
          Saving
        </Button>
        <output role="status" aria-label="Button activation count">
          Count: {count}
        </output>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const increment = canvas.getByRole("button", { name: /increment/i });
    const locked = canvas.getByRole("button", { name: /locked/i });
    const saving = canvas.getByRole("button", { name: /saving/i });
    const count = canvas.getByRole("status", {
      name: /button activation count/i,
    });

    await expect(locked).toBeDisabled();
    await expect(saving).toBeDisabled();
    await expect(saving).toHaveAttribute("aria-busy", "true");

    await userEvent.click(locked);
    await userEvent.click(saving);
    await expect(count).toHaveTextContent("Count: 0");

    await userEvent.click(increment);
    await expect(increment).toHaveFocus();
    await expect(count).toHaveTextContent("Count: 1");
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const openLink = canvas.getByRole("link", { name: /open in browser/i });
    const learnLink = canvas.getByRole("link", { name: /learn more/i });

    await expect(openLink.tagName).toBe("A");
    await expect(openLink).toHaveAttribute("href", "#open");
    await expect(learnLink.tagName).toBe("A");
    await expect(learnLink).toHaveAttribute("data-variant", "plain");

    await userEvent.click(openLink);
    await expect(openLink).toHaveFocus();
  },
};

export const AsChildBusyAndDisabled: Story = {
  name: "asChild busy and disabled (play)",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="asChild state attributes"
    >
      <Button asChild disabled variant="tinted">
        <a href="#disabled-link">Disabled link action</a>
      </Button>
      <Button asChild loading variant="gray">
        <a href="#busy-link">Busy link action</a>
      </Button>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const disabledLink = canvas.getByRole("link", {
      name: /disabled link action/i,
    });
    const busyLink = canvas.getByRole("link", { name: /busy link action/i });

    await expect(disabledLink.tagName).toBe("A");
    await expect(disabledLink).toHaveAttribute("aria-disabled", "true");
    await expect(busyLink.tagName).toBe("A");
    await expect(busyLink).toHaveAttribute("aria-disabled", "true");
    await expect(busyLink).toHaveAttribute("aria-busy", "true");
  },
};

/*
 * Wave-8 🔴 #1 regression. Pre-fix, an `asChild` Button with `disabled`
 * (or `loading`) set only carried `aria-disabled` — the CSS keyed off
 * `:disabled` / `[aria-busy]`, so the rendered <a> stayed visually
 * enabled (no `cursor: not-allowed`, the active token color, full
 * hover paint) AND a click on the anchor BOTH navigated to `href` AND
 * invoked the child's onClick + any caller-attached onClick. This
 * story exercises the real activation path: a `useState` counter is
 * incremented from a click handler attached DIRECTLY to the child <a>,
 * and a second counter is incremented from `<Button onClick>` (which
 * lands on the Slot via {...rest}). With the activation guard in
 * place, both counters MUST stay at 0 after a click, the URL hash MUST
 * NOT change to the link target, and the rendered <a> MUST visually
 * resolve to `cursor: not-allowed` (the per-variant disabled styling
 * keyed off `[aria-disabled="true"]`). Pre-fix this story fails on
 * every one of those assertions.
 */
export const AsChildDisabledIsInert: Story = {
  name: "asChild disabled is inert (play)",
  render: function AsChildDisabledIsInertRender() {
    const [childClicks, setChildClicks] = useState(0);
    const [wrapperClicks, setWrapperClicks] = useState(0);
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="asChild disabled inert interactions"
      >
        <Button
          asChild
          disabled
          variant="filled"
          onClick={() => setWrapperClicks((value) => value + 1)}
        >
          <a
            href="#wave8-asChild-disabled-target"
            data-testid="aschild-disabled-inert-link"
            onClick={() => setChildClicks((value) => value + 1)}
          >
            Inert link action
          </a>
        </Button>
        <output
          role="status"
          aria-label="asChild disabled child click count"
          data-testid="aschild-disabled-inert-child-count"
        >
          Child clicks: {childClicks}
        </output>
        <output
          role="status"
          aria-label="asChild disabled wrapper click count"
          data-testid="aschild-disabled-inert-wrapper-count"
        >
          Wrapper clicks: {wrapperClicks}
        </output>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const link = canvas.getByRole("link", { name: /inert link action/i });
    const childCount = canvas.getByRole("status", {
      name: /asChild disabled child click count/i,
    });
    const wrapperCount = canvas.getByRole("status", {
      name: /asChild disabled wrapper click count/i,
    });

    // 1. Visual disabled — cursor:not-allowed lands via the
    //    `[aria-disabled="true"]` mirror added in wave-8 🔴 #1.
    const cursor = getComputedStyle(link).cursor;
    await expect(cursor).toBe("not-allowed");

    // 2. Aria state is correct.
    await expect(link).toHaveAttribute("aria-disabled", "true");
    await expect(link).toHaveAttribute("href", "#wave8-asChild-disabled-target");

    // 3. The hash before the click is captured so we can prove the
    //    click did NOT navigate. We don't rely on the test runner's
    //    URL because Storybook iframes can rewrite location.hash for
    //    their own routing — read window.location.hash directly via
    //    evaluate to keep this real-path.
    const hashBefore = window.location.hash;

    // 4. Click the anchor — neither the child onClick nor the wrapper
    //    onClick should fire, and navigation should be suppressed.
    await userEvent.click(link);

    const hashAfter = window.location.hash;
    await expect(hashAfter).toBe(hashBefore);
    await expect(childCount).toHaveTextContent("Child clicks: 0");
    await expect(wrapperCount).toHaveTextContent("Wrapper clicks: 0");
  },
};
