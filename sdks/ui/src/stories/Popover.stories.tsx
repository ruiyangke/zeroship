import type { Meta, StoryObj } from "@storybook/react";
import { Button, Dialog, Popover } from "../components";

const meta: Meta<typeof Popover> = {
  title: "Components/Popover",
  component: Popover,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Popover>;

/* ─── 1. Basic ──────────────────────────────────────────────────────── */
export const Basic: Story = {
  name: "Basic",
  parameters: {
    docs: {
      description: {
        story:
          "Anchored panel that opens on click of the trigger. No Title / " +
          "Description / Backdrop — just bare content. Default placement " +
          "is bottom-center; Floating UI auto-flips on collision.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic popover">
      <Popover>
        <Popover.Trigger
          render={<Button data-testid="popover-basic-trigger">Open popover</Button>}
        />
        <Popover.Portal>
          <Popover.Popup data-testid="popover-basic-popup">
            A short helper hint anchored below the trigger.
          </Popover.Popup>
        </Popover.Portal>
      </Popover>
    </div>
  ),
};

/* ─── 2. WithTitleDescription ───────────────────────────────────────── */
export const WithTitleDescription: Story = {
  name: "With title + description",
  parameters: {
    docs: {
      description: {
        story:
          "Title and Description auto-wire `aria-labelledby` and " +
          "`aria-describedby` on the popup, mirroring the Dialog " +
          "subpart pattern.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With title and description">
      <Popover>
        <Popover.Trigger
          render={
            <Button data-testid="popover-titledesc-trigger">Show details</Button>
          }
        />
        <Popover.Portal>
          <Popover.Popup data-testid="popover-titledesc-popup">
            <Popover.Title>Account settings</Popover.Title>
            <Popover.Description>
              Configure how the platform handles deploys and notifications
              for this project.
            </Popover.Description>
          </Popover.Popup>
        </Popover.Portal>
      </Popover>
    </div>
  ),
};

/* ─── 3. WithArrow ──────────────────────────────────────────────────── */
export const WithArrow: Story = {
  name: "With arrow",
  parameters: {
    docs: {
      description: {
        story:
          "Arrow renders an SVG triangle that Base UI rotates per side. " +
          "The triangle picks up `--zs-surface` via `currentColor` so it " +
          "reads as a continuation of the popup surface.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With arrow">
      <Popover>
        <Popover.Trigger
          render={
            <Button data-testid="popover-arrow-trigger">Open with arrow</Button>
          }
        />
        <Popover.Portal>
          <Popover.Popup data-testid="popover-arrow-popup">
            <Popover.Arrow data-testid="popover-arrow-glyph" />
            <Popover.Title>Tip</Popover.Title>
            <Popover.Description>
              The arrow points at the trigger so the relationship reads at
              a glance.
            </Popover.Description>
          </Popover.Popup>
        </Popover.Portal>
      </Popover>
    </div>
  ),
};

/* ─── 4. WithBackdrop ───────────────────────────────────────────────── */
export const WithBackdrop: Story = {
  name: "With backdrop (modal-feel)",
  parameters: {
    docs: {
      description: {
        story:
          "Opt-in Backdrop turns the Popover into a modal-feel panel — " +
          "darkens the page and traps outside-click. Default Popover " +
          "skips the Backdrop entirely to keep the popover-feel.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With backdrop">
      <Popover modal>
        <Popover.Trigger
          render={
            <Button data-testid="popover-backdrop-trigger">Open modal popover</Button>
          }
        />
        <Popover.Portal>
          <Popover.Backdrop data-testid="popover-backdrop-scrim" />
          <Popover.Popup data-testid="popover-backdrop-popup">
            <Popover.Title>Confirm change</Popover.Title>
            <Popover.Description>
              Backdrop scrim makes this popover modal-feel — interactions
              outside the panel are blocked.
            </Popover.Description>
            <Popover.Close>Got it</Popover.Close>
          </Popover.Popup>
        </Popover.Portal>
      </Popover>
    </div>
  ),
};

/* ─── 5. WithClose ──────────────────────────────────────────────────── */
export const WithClose: Story = {
  name: "With close button",
  parameters: {
    docs: {
      description: {
        story:
          "Popover.Close mirrors Dialog.Close — `asChild` Slot composition " +
          "for custom button styling, default path renders our Button. " +
          "Activation closes the popup; caller onClick composes via the " +
          "shared composedOnClick pattern (Phase 2.B fix 1).",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With close">
      <Popover>
        <Popover.Trigger
          render={
            <Button data-testid="popover-close-trigger">Open with close</Button>
          }
        />
        <Popover.Portal>
          <Popover.Popup data-testid="popover-close-popup">
            <Popover.Title>Quick action</Popover.Title>
            <Popover.Description>
              Use the close button below to dismiss the panel.
            </Popover.Description>
            <Popover.Close data-testid="popover-close-button">Close</Popover.Close>
          </Popover.Popup>
        </Popover.Portal>
      </Popover>
    </div>
  ),
};

/* ─── 6. PlacementSide ──────────────────────────────────────────────── */
export const PlacementSide: Story = {
  name: "Placement: side",
  parameters: {
    docs: {
      description: {
        story:
          "Four placement sides (top / right / bottom / left). Base UI " +
          "auto-flips on collision so the rendered side may differ from " +
          "the requested one near the viewport edge.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Placement sides"
      style={{ flexWrap: "wrap", gap: "1.5rem" }}
    >
      {(["top", "right", "bottom", "left"] as const).map((side) => (
        <Popover key={side}>
          <Popover.Trigger
            render={
              <Button
                variant="tinted"
                data-testid={`popover-side-${side}-trigger`}
              >
                Open {side}
              </Button>
            }
          />
          <Popover.Portal>
            <Popover.Popup
              side={side}
              data-testid={`popover-side-${side}-popup`}
            >
              <Popover.Arrow />
              <Popover.Description>
                Anchored to the {side} side.
              </Popover.Description>
            </Popover.Popup>
          </Popover.Portal>
        </Popover>
      ))}
    </div>
  ),
};

/* ─── 7. AlignStartCenterEnd ────────────────────────────────────────── */
export const AlignStartCenterEnd: Story = {
  name: "Align: start / center / end",
  parameters: {
    docs: {
      description: {
        story:
          "Three alignments along the anchored side. Combined with the " +
          "trigger's inline-size, alignment lets the popup track the " +
          "trigger's leading edge / midpoint / trailing edge.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Align"
      style={{ flexDirection: "column", alignItems: "stretch", gap: "1rem" }}
    >
      {(["start", "center", "end"] as const).map((align) => (
        <Popover key={align}>
          <Popover.Trigger
            render={
              <Button
                variant="tinted"
                data-testid={`popover-align-${align}-trigger`}
              >
                Align {align}
              </Button>
            }
          />
          <Popover.Portal>
            <Popover.Popup
              align={align}
              data-testid={`popover-align-${align}-popup`}
            >
              <Popover.Description>
                Align="{align}" on the trigger axis.
              </Popover.Description>
            </Popover.Popup>
          </Popover.Portal>
        </Popover>
      ))}
    </div>
  ),
};

/* ─── 8. NestedInDialog ─────────────────────────────────────────────── */
export const NestedInDialog: Story = {
  name: "Nested in dialog",
  parameters: {
    docs: {
      description: {
        story:
          "A Popover mounted inside a Dialog. The Popover's positioner " +
          "uses `calc(var(--zs-z-modal) + 10)` so it stacks above the " +
          "Dialog's popup. ESC closes the inner Popover first, then a " +
          "second ESC closes the Dialog (Base UI's overlay stack).",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Nested in dialog">
      <Dialog>
        <Dialog.Trigger
          render={
            <Button data-testid="popover-nested-dialog-trigger">
              Open dialog
            </Button>
          }
        />
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup data-testid="popover-nested-dialog-popup">
            <Dialog.Header>
              <Dialog.Title>Dialog with nested popover</Dialog.Title>
              <Dialog.Description>
                The popover trigger lives inside the dialog body.
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Body>
              <Popover>
                <Popover.Trigger
                  render={
                    <Button
                      variant="tinted"
                      data-testid="popover-nested-popover-trigger"
                    >
                      Open popover
                    </Button>
                  }
                />
                <Popover.Portal>
                  <Popover.Popup data-testid="popover-nested-popover-popup">
                    <Popover.Title>Nested</Popover.Title>
                    <Popover.Description>
                      Stacks above the dialog popup via the z-modal bump.
                    </Popover.Description>
                  </Popover.Popup>
                </Popover.Portal>
              </Popover>
            </Dialog.Body>
            <Dialog.Footer>
              <Dialog.Close>Close dialog</Dialog.Close>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  ),
};

/* ─── 9. Disabled ───────────────────────────────────────────────────── */
export const Disabled: Story = {
  name: "Disabled trigger",
  parameters: {
    docs: {
      description: {
        story:
          "Disabled trigger never opens the popover; pointer cursor reads " +
          "not-allowed; focus ring still paints if the trigger receives " +
          "keyboard focus.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled">
      <Popover>
        <Popover.Trigger
          disabled
          render={
            <Button
              disabled
              variant="tinted"
              data-testid="popover-disabled-trigger"
            >
              Disabled
            </Button>
          }
        />
        <Popover.Portal>
          <Popover.Popup data-testid="popover-disabled-popup">
            This should never appear.
          </Popover.Popup>
        </Popover.Portal>
      </Popover>
    </div>
  ),
};

/* ─── 10. RTL ───────────────────────────────────────────────────────── */
export const Rtl: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew content flows right-to-left; align=`start` / `end` flips " +
          "physical sides under `direction: rtl`. Logical properties on " +
          "the popup keep padding axis-correct.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="RTL"
      dir="rtl"
      lang="he"
    >
      <Popover>
        <Popover.Trigger
          render={
            <Button data-testid="popover-rtl-trigger">פתח חלון</Button>
          }
        />
        <Popover.Portal>
          <Popover.Popup data-testid="popover-rtl-popup" align="start">
            <Popover.Title>הגדרות</Popover.Title>
            <Popover.Description>
              חלון מעוגן הזורם מימין לשמאל.
            </Popover.Description>
          </Popover.Popup>
        </Popover.Portal>
      </Popover>
    </div>
  ),
};
