import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import { Spinner } from "../blocks";

const meta: Meta<typeof Spinner> = {
  title: "Blocks/Spinner",
  component: Spinner,
  parameters: { layout: "fullscreen" },
};

export default meta;

type Story = StoryObj<typeof Spinner>;

/* ─── 1. Sizes — sm / md / lg ───────────────────────────────────────── */
export const Sizes: Story = {
  name: "Sizes (sm / md / lg)",
  parameters: {
    docs: {
      description: {
        story:
          "The three sizes. Each Spinner is a `role='status'` region with " +
          "a visually-hidden label (default 'Loading') so screen readers " +
          "announce it; the ring itself is `aria-hidden`.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Spinner sizes">
      <div className="zs-story-cell">
        <span className="zs-story-label">sm</span>
        <Spinner size="sm" label="Loading small" data-testid="spinner-sm" />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">md</span>
        <Spinner size="md" label="Loading medium" data-testid="spinner-md" />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">lg</span>
        <Spinner size="lg" label="Loading large" data-testid="spinner-lg" />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Each spinner is a status region with the accessible name from its
    // visually-hidden label.
    await expect(
      canvas.getByRole("status", { name: /loading small/i }),
    ).toBeInTheDocument();
    await expect(
      canvas.getByRole("status", { name: /loading medium/i }),
    ).toBeInTheDocument();
    await expect(
      canvas.getByRole("status", { name: /loading large/i }),
    ).toBeInTheDocument();
    await expect(canvas.getByTestId("spinner-sm")).toHaveAttribute(
      "data-size",
      "sm",
    );
  },
};

/* ─── 2. Default label ──────────────────────────────────────────────── */
export const DefaultLabel: Story = {
  name: "Default label (Loading)",
  parameters: {
    docs: {
      description: {
        story:
          "With no `label`, the accessible name defaults to 'Loading'. " +
          "The play() asserts the `role='status'` + accessible name.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Default spinner">
      <div className="zs-story-cell">
        <Spinner data-testid="spinner-default" />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const status = canvas.getByRole("status", { name: /loading/i });
    await expect(status).toHaveAttribute("data-testid", "spinner-default");
  },
};

/* ─── 3. Consumer aria-label override ───────────────────────────────── *
 *
 * A consumer-supplied `aria-label` must NAME the status region — it wins
 * over the default 'Loading'. Regression for the fix where a generated
 * `aria-labelledby` was emitted before `{...rest}` and (per the
 * accessible-name spec, labelledby > label) silently clobbered the
 * consumer's `aria-label`, leaving the announced name stuck at "Loading". */
export const ConsumerAriaLabel: Story = {
  name: "Consumer aria-label override",
  parameters: {
    docs: {
      description: {
        story:
          "A consumer `aria-label` names the `role='status'` region and " +
          "wins over the default 'Loading'. The internal visually-hidden " +
          "label span is omitted so the consumer's value is the only name.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Named spinner">
      <div className="zs-story-cell">
        <Spinner aria-label="Saving changes" data-testid="spinner-named" />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // The consumer aria-label provides the accessible name; "Loading"
    // must NOT win.
    await expect(
      canvas.getByRole("status", { name: /saving changes/i }),
    ).toBeInTheDocument();
  },
};

/* ─── 4. Reduced motion ─────────────────────────────────────────────── *
 *
 * Under prefers-reduced-motion the rotation stops (`animation: none`)
 * and the static arc remains painted — an acceptable non-animated busy
 * indicator. The role='status' label still conveys "loading" to AT.
 * The motion gate lives in Spinner.css; this story asserts the status
 * region + accessible name survive regardless of the motion preference. */
export const ReducedMotion: Story = {
  name: "Reduced motion (spin stopped, indicator preserved)",
  parameters: {
    docs: {
      description: {
        story:
          "Under `prefers-reduced-motion: reduce` the spin stops but the " +
          "static ring stays painted, and the `role='status'` label still " +
          "announces 'loading'. The motion gate lives in Spinner.css.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Reduced-motion spinner">
      <div className="zs-story-cell">
        <Spinner label="Loading content" data-testid="spinner-rm" />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // The status region + accessible name persist with motion off; the
    // ring is still painted (it carries a border, never display:none).
    const status = canvas.getByRole("status", { name: /loading content/i });
    const ring = status.querySelector(".zs-spinner__ring");
    await expect(ring).not.toBeNull();
    await expect(getComputedStyle(ring as Element).display).not.toBe("none");
  },
};
