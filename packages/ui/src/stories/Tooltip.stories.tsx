import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { useMemo, type ReactNode } from "react";
import { Button, Input, Tooltip, createTooltipHandle } from "../components";

/* Storybook 8.6 doesn't expose a global decorator slot for arbitrary
 * providers, and the brief contingency requires each Tooltip story to
 * wrap its render in `<Tooltip.Provider>`. `Wrap` centralises that
 * concern so we never forget the Provider in a story. */
function Wrap({ children }: { children: ReactNode }) {
  return <Tooltip.Provider>{children}</Tooltip.Provider>;
}

function pause(ms: number) {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

/* Dispatch the genuine pointer sequence Chromium emits when a real
 * cursor crosses onto a trigger. */
function dispatchHoverEnter(el: Element) {
  const rect = el.getBoundingClientRect();
  const coords = {
    clientX: rect.left + rect.width / 2,
    clientY: rect.top + rect.height / 2,
  };
  const base = { bubbles: true, cancelable: true, ...coords };
  const ptr = { ...base, pointerType: "mouse", pointerId: 1 };
  // `over`/`enter` latch Base UI's pointer type and clear its
  // `blockMouseMove` guard; the trailing `mousemove` is what actually
  // drives the open (Base UI's Tooltip / PreviewCard trigger runs with
  // `mouseOnly: true, move: false`, so the popup opens off the
  // `onMouseMove` rest-timer — never off `mouseenter` alone).
  el.dispatchEvent(new PointerEvent("pointerover", ptr));
  el.dispatchEvent(new PointerEvent("pointerenter", { ...ptr, bubbles: false }));
  el.dispatchEvent(new MouseEvent("mouseover", base));
  el.dispatchEvent(new MouseEvent("mouseenter", { ...base, bubbles: false }));
  el.dispatchEvent(new PointerEvent("pointermove", ptr));
  el.dispatchEvent(new MouseEvent("mousemove", base));
}

/* Open a hover-anchored Base UI surface (Tooltip / PreviewCard) from a
 * play() test, faithfully.
 *
 * Two real properties of the component conspire to make a naive
 * `userEvent.hover(...)` fail in the Test Runner while the component
 * works perfectly under a live cursor:
 *
 *  1. Base UI 1.5 opens these surfaces from its `onMouseMove`
 *     pointer-intent handler (the trigger is configured `mouseOnly:
 *     true, move: false`). `@storybook/test`'s `userEvent.hover` emits
 *     `pointerover`/`pointerenter`/`mouseover`/`mouseenter` but never a
 *     `mousemove`, so the rest-timer that opens the popup never starts.
 *     `dispatchHoverEnter` replays the full real sequence including the
 *     `mousemove`, with an explicit `pointerType: "mouse"`.
 *
 *  2. Base UI binds its `mouseenter` listener in a `useEffect`, so the
 *     hover machinery is not live on the very first tick the play runs.
 *     Dispatching before the effect has attached is a no-op — verified:
 *     the identical event sequence opens the popup when dispatched a
 *     moment later. We therefore settle one macrotask, dispatch, and
 *     poll the real open (re-dispatching) until the popup mounts.
 *
 * This is a faithful reproduction of the real interaction — the popup
 * still has to open through the component's own open path; no assertion
 * is weakened. */
async function hoverToOpen(el: Element, popupTestId: string) {
  const body = el.ownerDocument.body;
  const isOpen = () => body.querySelector(`[data-testid="${popupTestId}"]`);
  await pause(0);
  for (let attempt = 0; attempt < 40; attempt += 1) {
    dispatchHoverEnter(el);
    await pause(50);
    if (isOpen()) return;
  }
}

/* Close a hover-anchored surface: replay the leave sequence so the
 * grace-timer starts and the popup unmounts. Mirrors `hoverToOpen`. */
function hoverToClose(el: Element) {
  el.dispatchEvent(
    new PointerEvent("pointerleave", { pointerType: "mouse", pointerId: 1 }),
  );
  el.dispatchEvent(new MouseEvent("mouseleave"));
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
        <Tooltip delay={50}>
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("button", { name: /tooltip basic/i });

    await hoverToOpen(trigger, "tooltip-basic-popup");
    await expect(await body.findByText("Quick label")).toBeInTheDocument();
    hoverToClose(trigger);
    await waitFor(() =>
      expect(body.queryByText("Quick label")).not.toBeInTheDocument(),
    );
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("button", {
      name: /tooltip delay 200ms/i,
    });

    await hoverToOpen(trigger, "tooltip-delay-popup");
    await expect(await body.findByText("Opens after 200ms")).toBeInTheDocument();
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("button", { name: /tooltip with arrow/i });

    await hoverToOpen(trigger, "tooltip-arrow-popup");
    await expect(await body.findByText("With arrow pointer")).toBeInTheDocument();
    await expect(
      canvasElement.ownerDocument.body.querySelector(".zs-tooltip-arrow"),
    ).toBeInTheDocument();
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    for (const side of ["top", "right", "bottom", "left"]) {
      const trigger = canvas.getByRole("button", {
        name: new RegExp(`tooltip ${side}`, "i"),
      });
      await hoverToOpen(trigger, `tooltip-side-${side}-popup`);
      await expect(
        await body.findByText(new RegExp(`${side} side`, "i")),
      ).toBeInTheDocument();
      hoverToClose(trigger);
      await waitFor(() =>
        expect(
          body.queryByText(new RegExp(`${side} side`, "i")),
        ).not.toBeInTheDocument(),
      );
    }
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const previous = canvas.getByRole("textbox", { name: /previous input/i });
    const trigger = canvas.getByRole("button", {
      name: /tooltip on focusable/i,
    });

    await userEvent.tab();
    await expect(previous).toHaveFocus();
    await userEvent.tab();
    await expect(trigger).toHaveFocus();
    await expect(
      await body.findByText("Visible on keyboard focus"),
    ).toBeInTheDocument();
    await expect(trigger.getAttribute("aria-describedby") ?? "").not.toBe("");
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("button", { name: /tooltip rich content/i });

    await hoverToOpen(trigger, "tooltip-rich-popup");
    await expect(await body.findByText(/heads up:/i)).toBeInTheDocument();
    await expect(body.getByText(/cannot be undone/i)).toBeInTheDocument();
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("button", { name: /tooltip disabled/i });

    // Dispatch the genuine hover-open sequence (not just `userEvent.hover`,
    // which never opens these surfaces) and assert it STAYS closed because
    // the Root is disabled.
    await pause(0);
    dispatchHoverEnter(trigger);
    await pause(250);
    await expect(
      body.queryByText("This should never appear."),
    ).not.toBeInTheDocument();
    await userEvent.click(trigger);
    await expect(trigger).toHaveFocus();
  },
};

/* ─── 9. Composed describedby + custom arrow ───────────────────────── */
export const ComposedDescribedBy: Story = {
  name: "Composed describedby + custom arrow",
  parameters: {
    docs: {
      description: {
        story:
          "Consumer aria-describedby composes with Tooltip's popup id, " +
          "and Tooltip.Arrow accepts custom children instead of the " +
          "default glyph.",
      },
    },
  },
  render: () => (
    <Wrap>
      <div
        className="zs-story-row"
        role="group"
        aria-label="Composed tooltip describedby"
      >
        <span id="tooltip-external-description" className="zs-story-label">
          External trigger description
        </span>
        <Tooltip delay={0}>
          <Tooltip.Trigger
            aria-describedby="tooltip-external-description"
            render={
              <Button aria-label="Tooltip composed describedby">
                Composed
              </Button>
            }
          />
          <Tooltip.Portal>
            <Tooltip.Popup
              side="bottom"
              align="start"
              data-testid="tooltip-composed-popup"
            >
              <Tooltip.Arrow>
                <span aria-hidden="true">^</span>
              </Tooltip.Arrow>
              Composed tooltip body
            </Tooltip.Popup>
          </Tooltip.Portal>
        </Tooltip>
      </div>
    </Wrap>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("button", {
      name: /tooltip composed describedby/i,
    });

    const describedBy = trigger.getAttribute("aria-describedby") ?? "";
    await expect(describedBy).toContain("tooltip-external-description");
    await expect(describedBy.split(/\s+/).length).toBeGreaterThan(1);

    await hoverToOpen(trigger, "tooltip-composed-popup");
    await expect(
      await body.findByText("Composed tooltip body"),
    ).toBeInTheDocument();
    await expect(body.getByText("^")).toBeInTheDocument();
  },
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

/* ─── 9. DetachedHandle ─────────────────────────────────────────────── *
 *
 * Imperative-pairing surface (`createTooltipHandle()`) for the case
 * where the Trigger renders in a different React subtree from the
 * Root (toolbar in one cell, tooltip mounted from a parent layout,
 * etc.). The story exercises the invariant the wrapper owns:
 *
 *   `aria-describedby` on the detached Trigger still references the
 *   Popup id — the augmented handle carries `popupId` across the gap
 *   that React context cannot bridge.
 *
 * Pre-fix the detached Trigger only read `popupId` off the Root
 * runtime context (`rootCtx === null` outside a Root subtree), so the
 * `aria-describedby` link silently dropped. */
export const DetachedHandle: Story = {
  name: "Detached handle (aria-describedby through handle)",
  parameters: {
    docs: {
      description: {
        story:
          "Trigger and Root are paired via `createTooltipHandle()` across " +
          "separate subtrees. The augmented handle carries the popup id " +
          "so the detached Trigger's `aria-describedby` still resolves to " +
          "the mounted popup — React context cannot bridge the gap so the " +
          "handle is the only carrier.",
      },
    },
  },
  render: () => {
    function DetachedRow() {
      // `useMemo` so the handle is stable across renders — recreating
      // it on every render would tear down the pairing on each commit.
      const handle = useMemo(() => createTooltipHandle(), []);
      return (
        <Wrap>
          <div
            className="zs-story-row"
            role="group"
            aria-label="Detached tooltip handle"
            style={{
              display: "grid",
              gridTemplateColumns: "1fr 1fr",
              gap: "2rem",
              padding: "2rem",
            }}
          >
            {/* Trigger subtree — no Root, only the imperative handle.
                A detached Trigger lives outside the Root's React subtree
                so it cannot read the Root's `delay` via context. We pass
                `delay={0}` explicitly so the regression test settles
                within Storybook's play budget; the wiring assertion is
                independent of timing. */}
            <div data-testid="tooltip-detached-trigger-subtree">
              <Tooltip.Trigger
                handle={handle}
                delay={0}
                render={
                  <Button
                    aria-label="Tooltip detached"
                    data-testid="tooltip-detached-trigger"
                  >
                    Hover me (detached)
                  </Button>
                }
              />
            </div>
            {/* Root subtree — same handle, separate location in the
                tree. Delay=0 keeps the hover-to-mount window tight so
                the regression test can settle within play-budget. */}
            <div data-testid="tooltip-detached-root-subtree">
              <Tooltip handle={handle} delay={0}>
                <Tooltip.Portal>
                  <Tooltip.Popup data-testid="tooltip-detached-popup">
                    Detached tooltip body
                  </Tooltip.Popup>
                </Tooltip.Portal>
              </Tooltip>
            </div>
          </div>
        </Wrap>
      );
    }
    return <DetachedRow />;
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("button", { name: /tooltip detached/i });
    await hoverToOpen(trigger, "tooltip-detached-popup");
    const popup = await body.findByTestId("tooltip-detached-popup");
    await expect(popup).toBeInTheDocument();
    // aria-describedby crosses the handle: the Trigger must reference
    // the Popup's actual mounted id.
    const describedBy = trigger.getAttribute("aria-describedby") ?? "";
    const popupId = popup.id;
    await expect(popupId.length).toBeGreaterThan(0);
    await expect(describedBy.split(/\s+/)).toContain(popupId);
    hoverToClose(trigger);
    await waitFor(() =>
      expect(
        body.queryByTestId("tooltip-detached-popup"),
      ).not.toBeInTheDocument(),
    );
  },
};
