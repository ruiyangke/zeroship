import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { useState } from "react";
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
          {/* Bare-content popups (no Title) MUST carry their own
              accessible name. Base UI exposes the popup as
              `role="dialog"`, and an unnamed dialog announces as just
              "dialog" to screen readers. `aria-label` satisfies the
              missing-name warning the wrapper logs in dev. */}
          <Popover.Popup
            data-testid="popover-basic-popup"
            aria-label="Helper hint"
          >
            A short helper hint anchored below the trigger.
          </Popover.Popup>
        </Popover.Portal>
      </Popover>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", { name: /open popover/i }));
    // Look up by accessible name — the popup must be a NAMED dialog.
    const dialog = await body.findByRole("dialog", { name: /helper hint/i });
    await expect(dialog).toHaveTextContent(
      "A short helper hint anchored below the trigger.",
    );
    await userEvent.keyboard("{Escape}");
    await waitFor(() =>
      expect(
        body.queryByRole("dialog", { name: /helper hint/i }),
      ).not.toBeInTheDocument(),
    );
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", { name: /show details/i }));
    const dialog = await body.findByRole("dialog", {
      name: /account settings/i,
    });
    await expect(dialog).toHaveTextContent(
      /configure how the platform handles deploys/i,
    );
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(
      canvas.getByRole("button", { name: /open with arrow/i }),
    );
    const dialog = await body.findByRole("dialog", { name: /tip/i });
    await expect(dialog).toHaveTextContent(/relationship reads/i);
    await expect(dialog.querySelector(".zs-popover-arrow")).toBeInTheDocument();
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(
      canvas.getByRole("button", { name: /open modal popover/i }),
    );
    await expect(
      await body.findByRole("dialog", { name: /confirm change/i }),
    ).toBeInTheDocument();
    await userEvent.click(body.getByRole("button", { name: /got it/i }));
    await waitFor(() =>
      expect(
        body.queryByRole("dialog", { name: /confirm change/i }),
      ).not.toBeInTheDocument(),
    );
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(
      canvas.getByRole("button", { name: /open with close/i }),
    );
    await expect(
      await body.findByRole("dialog", { name: /quick action/i }),
    ).toBeInTheDocument();
    await userEvent.click(body.getByRole("button", { name: /^close$/i }));
    await waitFor(() =>
      expect(
        body.queryByRole("dialog", { name: /quick action/i }),
      ).not.toBeInTheDocument(),
    );
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    for (const side of ["top", "right", "bottom", "left"]) {
      await userEvent.click(
        canvas.getByRole("button", { name: new RegExp(`open ${side}`, "i") }),
      );
      await expect(
        await body.findByText(new RegExp(`anchored to the ${side} side`, "i")),
      ).toBeInTheDocument();
      await userEvent.keyboard("{Escape}");
      await waitFor(() =>
        expect(
          body.queryByText(new RegExp(`anchored to the ${side} side`, "i")),
        ).not.toBeInTheDocument(),
      );
    }
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", { name: /align start/i }));
    await expect(
      await body.findByText(/align="start" on the trigger axis/i),
    ).toBeInTheDocument();
    await userEvent.keyboard("{Escape}");
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", { name: /open dialog/i }));
    await expect(
      await body.findByRole("dialog", { name: /dialog with nested popover/i }),
    ).toBeInTheDocument();
    await userEvent.click(body.getByRole("button", { name: /open popover/i }));
    await expect(
      await body.findByRole("dialog", { name: /^nested$/i }),
    ).toBeInTheDocument();
    await userEvent.keyboard("{Escape}");
    await waitFor(() =>
      expect(body.queryByRole("dialog", { name: /^nested$/i })).not.toBeInTheDocument(),
    );
    await expect(
      body.getByRole("dialog", { name: /dialog with nested popover/i }),
    ).toBeInTheDocument();
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("button", { name: /disabled/i });

    await expect(trigger).toBeDisabled();
    await userEvent.click(trigger);
    await expect(body.queryByRole("dialog")).not.toBeInTheDocument();
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", { name: /פתח חלון/i }));
    await expect(
      await body.findByRole("dialog", { name: /הגדרות/i }),
    ).toBeInTheDocument();
  },
};

/* ─── 11. Close asChild composition ────────────────────────────────── */
export const CloseAsChildComposition: Story = {
  name: "Close asChild composition",
  parameters: {
    docs: {
      description: {
        story:
          "Two asChild close paths: a native button whose caller onClick " +
          "prevents dismissal, and a link that lets Base UI's close " +
          "handler proceed.",
      },
    },
  },
  render: function CloseAsChildCompositionRender() {
    const [prevented, setPrevented] = useState(false);
    return (
      <div className="zs-story-row" role="group" aria-label="Close asChild">
        <Popover>
          <Popover.Trigger render={<Button>Open child close</Button>} />
          <Popover.Portal>
            <Popover.Popup>
              <Popover.Title>Child close</Popover.Title>
              <Popover.Description>
                Custom elements receive the close behavior through Slot.
              </Popover.Description>
              <Popover.Close
                asChild
                onClick={(event) => {
                  event.preventDefault();
                  setPrevented(true);
                }}
              >
                <button type="button">Keep open</button>
              </Popover.Close>
              <Popover.Close asChild>
                <a href="#popover-child-closed">Close link</a>
              </Popover.Close>
              <span style={{ fontSize: "0.8125rem" }}>
                prevented: {prevented ? "yes" : "no"}
              </span>
            </Popover.Popup>
          </Popover.Portal>
        </Popover>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(
      canvas.getByRole("button", { name: /open child close/i }),
    );
    await expect(
      await body.findByRole("dialog", { name: /child close/i }),
    ).toBeInTheDocument();
    await userEvent.click(body.getByRole("button", { name: /keep open/i }));
    await expect(body.getByText("prevented: yes")).toBeInTheDocument();
    await expect(
      body.getByRole("dialog", { name: /child close/i }),
    ).toBeInTheDocument();

    await userEvent.click(body.getByRole("link", { name: /close link/i }));
    await waitFor(() =>
      expect(
        body.queryByRole("dialog", { name: /child close/i }),
      ).not.toBeInTheDocument(),
    );
  },
};

/* ─── 12. CloseAsChildForwardsRest — Round 5 fix #3 (Popover) ─────────── *
 *
 * Regression for Round 5 fix #3 (Popover.Close). Pre-fix, the asChild
 * branch only forwarded `closeProps` + `ref` + `onClick` to the Slot,
 * dropping wrapper-level `rest` props (className, data-*, aria-*,
 * disabled, style). Post-fix, `{...rest}` reaches the Slot so the
 * rendered child carries the className AND data-side-effect attribute
 * the caller set on `<Popover.Close>`. */
export const CloseAsChildForwardsRest: Story = {
  name: "Close asChild forwards wrapper rest (Round 5 regression)",
  parameters: {
    docs: {
      description: {
        story:
          "Wrapper-level `className`, `data-side-effect`, and " +
          "`aria-keyshortcuts` set on `<Popover.Close asChild>` must " +
          "land on the rendered child via Slot.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Close asChild rest">
      <Popover>
        <Popover.Trigger render={<Button>Open close-with-rest</Button>} />
        <Popover.Portal>
          <Popover.Popup data-testid="popover-close-rest-popup">
            <Popover.Title>Close forwards rest</Popover.Title>
            <Popover.Description>
              The wrapper-level className and data-attribute must reach
              the child button through Slot.
            </Popover.Description>
            <Popover.Close
              asChild
              className="custom-close-class"
              data-side-effect="logged"
              aria-keyshortcuts="Escape"
            >
              <button
                type="button"
                data-testid="popover-close-rest-target"
              >
                Got it
              </button>
            </Popover.Close>
          </Popover.Popup>
        </Popover.Portal>
      </Popover>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(
      canvas.getByRole("button", { name: /open close-with-rest/i }),
    );
    const target = await body.findByTestId("popover-close-rest-target");
    await expect(target).toHaveClass("custom-close-class");
    await expect(target).toHaveAttribute("data-side-effect", "logged");
    await expect(target).toHaveAttribute("aria-keyshortcuts", "Escape");
  },
};

/* ─── 13. PayloadRender — Round 6 fix #1 (PopoverProps payload) ─────── *
 *
 * Regression for the `PopoverProps.children` narrowing bug. Pre-fix,
 * the wrapper typed `children?: ReactNode`, which excluded Base UI's
 * `PayloadChildRenderFunction<Payload>` — a render function that
 * receives the active trigger's `payload`. TypeScript would reject the
 * function child outright (build-time regression), and at runtime
 * React would either render a function as text or throw "Functions
 * are not valid as a React child".
 *
 * Post-fix, `PopoverProps<Payload>` is generic and re-exposes Base
 * UI's `ReactNode | PayloadChildRenderFunction<Payload>` union, so the
 * function child compiles AND Base UI invokes it with `{ payload }`.
 *
 * The story declares two triggers carrying distinct payloads; the
 * Root's function child renders the payload's `label` field inside
 * the popup. Clicking each trigger swaps the rendered label, proving
 * the payload channel survives the wrapper. */
type PopoverPayload = { label: string };

// Stable payload references — Base UI's trigger-data-forwarding effect
// spreads `payload` into its dep array; an inline object literal would
// rebuild every render and tip the effect into an update loop.
const ALPHA_PAYLOAD: PopoverPayload = { label: "Alpha" };
const BETA_PAYLOAD: PopoverPayload = { label: "Beta" };

export const PayloadRender: Story = {
  name: "Payload render-function child (Round 6 regression)",
  parameters: {
    docs: {
      description: {
        story:
          "Base UI's `Popover.Root` accepts a render function child " +
          "receiving the active trigger's payload. The wrapper must " +
          "forward this API verbatim; pre-fix, the wrapper narrowed " +
          "`children` to `ReactNode` and silently rejected the " +
          "function form.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Payload render">
      {/* Single function-child carries the entire subtree so the Root's
          `PayloadChildRenderFunction` is invoked instead of being
          mixed with sibling React nodes (JSX would otherwise widen
          children to ReactNode[] and reject the function form). */}
      <Popover<PopoverPayload>>
        {({ payload }) => (
          <>
            <Popover.Trigger
              payload={ALPHA_PAYLOAD}
              render={
                <Button data-testid="popover-payload-trigger-alpha">
                  Open alpha
                </Button>
              }
            />
            <Popover.Trigger
              payload={BETA_PAYLOAD}
              render={
                <Button data-testid="popover-payload-trigger-beta">
                  Open beta
                </Button>
              }
            />
            <Popover.Portal>
              <Popover.Popup
                aria-label="Payload render popup"
                data-testid="popover-payload-popup"
              >
                <Popover.Title>Payload</Popover.Title>
                <Popover.Description data-testid="popover-payload-label">
                  {payload?.label ?? "no-payload"}
                </Popover.Description>
              </Popover.Popup>
            </Popover.Portal>
          </>
        )}
      </Popover>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", { name: /open alpha/i }));
    const alpha = await body.findByTestId("popover-payload-label");
    await expect(alpha).toHaveTextContent("Alpha");
    await userEvent.keyboard("{Escape}");
    await waitFor(() =>
      expect(body.queryByTestId("popover-payload-label")).not.toBeInTheDocument(),
    );

    await userEvent.click(canvas.getByRole("button", { name: /open beta/i }));
    const beta = await body.findByTestId("popover-payload-label");
    await expect(beta).toHaveTextContent("Beta");
  },
};
