import type { Meta, StoryObj } from "@storybook/react";
import type { ReactNode } from "react";
import { Button, Input, Tooltip } from "../components";

/* Storybook 8.6 doesn't expose a global decorator slot for arbitrary
 * providers, and the brief contingency requires each Tooltip story to
 * wrap its render in `<Tooltip.Provider>`. `Wrap` centralises that
 * concern so we never forget the Provider in a story. */
function Wrap({ children }: { children: ReactNode }) {
  return <Tooltip.Provider>{children}</Tooltip.Provider>;
}

const meta: Meta<typeof Tooltip> = {
  title: "Components/Tooltip",
  component: Tooltip,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Tooltip>;

/* ─── 1. Basic ──────────────────────────────────────────────────────── */
export const Basic: Story = {
  name: "Basic (hover-only)",
  parameters: {
    docs: {
      description: {
        story:
          "Default delay of 600ms before the tooltip appears on hover. " +
          "Mouseout closes immediately. Base UI also opens the tooltip " +
          "when the trigger receives keyboard focus.",
      },
    },
  },
  render: () => (
    <Wrap>
      <div className="zs-story-row" role="group" aria-label="Basic">
        <Tooltip>
          <Tooltip.Trigger
            render={
              <Button
                aria-label="Tooltip basic"
                data-testid="tooltip-basic-trigger"
              >
                Hover me
              </Button>
            }
          />
          <Tooltip.Portal>
            <Tooltip.Popup data-testid="tooltip-basic-popup">
              Quick label
            </Tooltip.Popup>
          </Tooltip.Portal>
        </Tooltip>
      </div>
    </Wrap>
  ),
};

/* ─── 2. WithDelay ──────────────────────────────────────────────────── */
export const WithDelay: Story = {
  name: "Custom delay",
  parameters: {
    docs: {
      description: {
        story:
          "Delay overridden to 200ms for a quick reveal. Useful for " +
          "icon-only buttons where the label must read immediately.",
      },
    },
  },
  render: () => (
    <Wrap>
      <div className="zs-story-row" role="group" aria-label="With delay">
        <Tooltip delay={200}>
          <Tooltip.Trigger
            render={
              <Button
                aria-label="Tooltip delay 200ms"
                data-testid="tooltip-delay-trigger"
              >
                Quick reveal
              </Button>
            }
          />
          <Tooltip.Portal>
            <Tooltip.Popup data-testid="tooltip-delay-popup">
              Opens after 200ms
            </Tooltip.Popup>
          </Tooltip.Portal>
        </Tooltip>
      </div>
    </Wrap>
  ),
};

/* ─── 3. WithArrow ──────────────────────────────────────────────────── */
export const WithArrow: Story = {
  name: "With arrow",
  parameters: {
    docs: {
      description: {
        story:
          "Arrow renders the shared 16×8 SVG triangle Popover uses; " +
          "Base UI rotates the wrapping div per side automatically.",
      },
    },
  },
  render: () => (
    <Wrap>
      <div className="zs-story-row" role="group" aria-label="With arrow">
        <Tooltip delay={150}>
          <Tooltip.Trigger
            render={
              <Button
                aria-label="Tooltip with arrow"
                data-testid="tooltip-arrow-trigger"
              >
                Anchored
              </Button>
            }
          />
          <Tooltip.Portal>
            <Tooltip.Popup data-testid="tooltip-arrow-popup">
              <Tooltip.Arrow data-testid="tooltip-arrow-glyph" />
              With arrow pointer
            </Tooltip.Popup>
          </Tooltip.Portal>
        </Tooltip>
      </div>
    </Wrap>
  ),
};

/* ─── 4. PlacementSide ──────────────────────────────────────────────── */
export const PlacementSide: Story = {
  name: "Placement: side",
  parameters: {
    docs: {
      description: {
        story:
          "Four placement sides; Base UI auto-flips on collision near " +
          "the viewport edge.",
      },
    },
  },
  render: () => (
    <Wrap>
      <div
        className="zs-story-row"
        role="group"
        aria-label="Placement sides"
        style={{ flexWrap: "wrap", gap: "1.5rem" }}
      >
        {(["top", "right", "bottom", "left"] as const).map((side) => (
          <Tooltip key={side} delay={150}>
            <Tooltip.Trigger
              render={
                <Button
                  variant="tinted"
                  aria-label={`Tooltip ${side}`}
                  data-testid={`tooltip-side-${side}-trigger`}
                >
                  {side}
                </Button>
              }
            />
            <Tooltip.Portal>
              <Tooltip.Popup
                side={side}
                data-testid={`tooltip-side-${side}-popup`}
              >
                <Tooltip.Arrow />
                {side} side
              </Tooltip.Popup>
            </Tooltip.Portal>
          </Tooltip>
        ))}
      </div>
    </Wrap>
  ),
};

/* ─── 5. OnFocusable ────────────────────────────────────────────────── */
export const OnFocusable: Story = {
  name: "On focusable (keyboard)",
  parameters: {
    docs: {
      description: {
        story:
          "Tab to the trigger and the tooltip opens on focus — Base UI " +
          "auto-wires `aria-describedby` linking the popup to the " +
          "trigger, so the assistive tech announces the label.",
      },
    },
  },
  render: () => (
    <Wrap>
      <div
        className="zs-story-row"
        role="group"
        aria-label="On focusable"
        style={{ gap: "1rem" }}
      >
        <Input
          placeholder="Tab into me first"
          data-testid="tooltip-onfocusable-prev"
          aria-label="Previous input"
        />
        <Tooltip delay={150}>
          <Tooltip.Trigger
            render={
              <Button
                aria-label="Tooltip on focusable"
                data-testid="tooltip-onfocusable-trigger"
              >
                Focus me
              </Button>
            }
          />
          <Tooltip.Portal>
            <Tooltip.Popup data-testid="tooltip-onfocusable-popup">
              Visible on keyboard focus
            </Tooltip.Popup>
          </Tooltip.Portal>
        </Tooltip>
      </div>
    </Wrap>
  ),
};

/* ─── 6. RichContent ────────────────────────────────────────────────── */
export const RichContent: Story = {
  name: "Rich content (multi-line)",
  parameters: {
    docs: {
      description: {
        story:
          "Multi-line tooltip with strong / em formatting. Max-width " +
          "preserves readability; long labels wrap rather than overflow.",
      },
    },
  },
  render: () => (
    <Wrap>
      <div className="zs-story-row" role="group" aria-label="Rich content">
        <Tooltip delay={150}>
          <Tooltip.Trigger
            render={
              <Button
                aria-label="Tooltip rich content"
                data-testid="tooltip-rich-trigger"
              >
                Rich label
              </Button>
            }
          />
          <Tooltip.Portal>
            <Tooltip.Popup data-testid="tooltip-rich-popup">
              <strong>Heads up:</strong> this action affects every member
              of the project. <em>Cannot be undone.</em>
            </Tooltip.Popup>
          </Tooltip.Portal>
        </Tooltip>
      </div>
    </Wrap>
  ),
};

/* ─── 7. Disabled ───────────────────────────────────────────────────── */
export const Disabled: Story = {
  name: "Disabled",
  parameters: {
    docs: {
      description: {
        story:
          "When `disabled` is set on the Root, the tooltip never opens " +
          "regardless of hover or focus.",
      },
    },
  },
  render: () => (
    <Wrap>
      <div className="zs-story-row" role="group" aria-label="Disabled">
        <Tooltip disabled>
          <Tooltip.Trigger
            render={
              <Button
                aria-label="Tooltip disabled"
                data-testid="tooltip-disabled-trigger"
              >
                No tooltip
              </Button>
            }
          />
          <Tooltip.Portal>
            <Tooltip.Popup data-testid="tooltip-disabled-popup">
              This should never appear.
            </Tooltip.Popup>
          </Tooltip.Portal>
        </Tooltip>
      </div>
    </Wrap>
  ),
};

/* ─── 8. RTL ────────────────────────────────────────────────────────── */
export const Rtl: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew tooltip content under `direction: rtl`. Logical " +
          "properties keep padding axis-correct; Base UI flips align=" +
          "`start` to the trailing physical side.",
      },
    },
  },
  render: () => (
    <Wrap>
      <div
        className="zs-story-row"
        role="group"
        aria-label="RTL"
        dir="rtl"
        lang="he"
      >
        <Tooltip delay={150}>
          <Tooltip.Trigger
            render={
              <Button
                aria-label="Tooltip RTL"
                data-testid="tooltip-rtl-trigger"
              >
                ריחוף
              </Button>
            }
          />
          <Tooltip.Portal>
            <Tooltip.Popup data-testid="tooltip-rtl-popup">
              תווית בעברית
            </Tooltip.Popup>
          </Tooltip.Portal>
        </Tooltip>
      </div>
    </Wrap>
  ),
};
