import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { useRef, type ReactNode } from "react";
import {
  Button,
  Toast,
  useToast,
  useToastManager,
  type ToastPayload,
} from "../components";

/* Storybook 8.6 doesn't expose a global decorator slot for arbitrary
 * providers. `Wrap` mounts the Toast.Provider + the default Viewport
 * so every story emits real toasts through the same imperative path
 * the docs advertise. The Viewport position can be overridden per
 * story via the `position` prop. */
function Wrap({
  children,
  position,
}: {
  children: ReactNode;
  position?: React.ComponentProps<typeof Toast.Viewport>["position"];
}) {
  return (
    <Toast.Provider>
      <div className="zs-story-row" role="group" aria-label="Toast demo">
        {children}
      </div>
      <Toast.Viewport position={position} />
    </Toast.Provider>
  );
}

const meta: Meta<typeof Toast.Provider> = {
  title: "Components/Toast",
  component: Toast.Provider,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Toast.Provider>;

function getDocument(canvasElement: HTMLElement) {
  return within(canvasElement.ownerDocument.body);
}

async function findToastByTitle(
  canvasElement: HTMLElement,
  title: string | RegExp,
  role: "status" | "alert" = "status",
) {
  const body = getDocument(canvasElement);
  let toast: HTMLElement | null = null;

  // Wait for BOTH the element to be in the DOM AND its enter transition
  // to land. Base UI's Toast.Root carries `data-starting-style` during
  // the enter motion; our CSS sets `opacity: 0` while that attribute is
  // present and `transition: opacity` to ramp back to 1 once it's gone.
  // Testing-library's `toBeVisible` walks ancestors and fails on any
  // opacity === "0" / 0; pairing it with `findToastByTitle` would race
  // the motion. We wait for the entering attribute to drop AND for the
  // computed opacity to land at the resting value (> 0.99 — strictly
  // greater than the floating-point quirk of mid-transition reads).
  await waitFor(() => {
    const match = body
      .getAllByText(title)
      .map((node) => node.closest(`[role="${role}"]`))
      .find((node): node is HTMLElement => node instanceof HTMLElement);

    expect(match).toBeTruthy();
    expect(match?.hasAttribute("data-starting-style")).toBeFalsy();
    const opacity = match ? Number(getComputedStyle(match).opacity) : 0;
    expect(opacity).toBeGreaterThan(0.99);
    toast = match ?? null;
  });

  return toast as HTMLElement;
}

/* Assert a toast surface is rendered. We can't use `.toBeVisible()` on
 * Toast.Root because Base UI deliberately stamps `aria-hidden="true"`
 * on high-priority toasts (warning/error) until they're keyboard-
 * focused — see @base-ui/react/toast/root/ToastRoot.js:462. The
 * accessibility intent is "the live region already announced this
 * via aria-live='assertive'; don't ALSO traverse into the toast DOM."
 * Testing Library + jest-dom treat aria-hidden=true as not-visible
 * per the WAI-ARIA spec, so the obvious `expect(toast).toBeVisible()`
 * always fails for warning/error toasts.
 *
 * The Title element is NOT aria-hidden and carries the same paint as
 * the surface, so checking its visibility is the right contract:
 * "the toast's headline is on screen." */
function expectToastShown(toast: HTMLElement) {
  expect(toast).toBeInTheDocument();
  const title = toast.querySelector(".zs-toast-title") as HTMLElement | null;
  expect(title).not.toBeNull();
  expect(title as HTMLElement).toBeVisible();
}

async function waitForToastGone(
  canvasElement: HTMLElement,
  title: string | RegExp,
) {
  const body = getDocument(canvasElement);
  await waitFor(() => {
    expect(body.queryByText(title)).not.toBeInTheDocument();
  });
}

/** Expand the toast viewport so Base UI lifts the `aria-hidden="true"`
 * it parks on Toast.Close / Toast.Action until the user interacts with
 * the stack. Without this, role-based queries (`getByRole("button",
 * { name: /dismiss/ })`) can't see those buttons and the play() races
 * fail. We hover the viewport, which mirrors the real desktop gesture
 * that expands the stack (focus is the keyboard equivalent). */
async function expandViewport(canvasElement: HTMLElement) {
  const viewport = canvasElement.ownerDocument.querySelector(
    ".zs-toast-viewport",
  );
  if (!(viewport instanceof HTMLElement)) return;
  viewport.dispatchEvent(
    new MouseEvent("mouseenter", { bubbles: true, cancelable: true }),
  );
  viewport.dispatchEvent(
    new MouseEvent("mouseover", { bubbles: true, cancelable: true }),
  );
  // Base UI's expansion logic flips on next frame — wait for it.
  await waitFor(() => {
    const close = canvasElement.ownerDocument.querySelector(
      ".zs-toast-close",
    );
    expect(close?.getAttribute("aria-hidden")).not.toBe("true");
  });
}

/* ─── 1. Basic ──────────────────────────────────────────────────────── */
export const Basic: Story = {
  name: "Basic (default variant)",
  parameters: {
    docs: {
      description: {
        story:
          "Fire-and-forget toast with a title only. Default variant uses " +
          "`role=\"status\"` + `aria-live=\"polite\"` so the announcement " +
          "lands when the user is idle. Auto-dismisses after the " +
          "Provider's `duration` (5000ms default).",
      },
    },
  },
  render: () => {
    function Trigger() {
      const { toast } = useToast();
      return (
        <Button
          data-testid="toast-basic-trigger"
          onClick={() =>
            toast({ id: "basic-demo", title: "Notification" })
          }
        >
          Show toast
        </Button>
      );
    }
    return (
      <Wrap>
        <Trigger />
      </Wrap>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = getDocument(canvasElement);

    await userEvent.click(canvas.getByRole("button", { name: /show toast/i }));
    const toast = await findToastByTitle(canvasElement, /notification/i);
    expectToastShown(toast);
    await expect(toast).toHaveAttribute("aria-live", "polite");

    await expandViewport(canvasElement);
    await userEvent.click(
      body.getByRole("button", { name: /dismiss notification/i }),
    );
    await waitForToastGone(canvasElement, /notification/i);
  },
};

/* ─── 2. WithDescription ────────────────────────────────────────────── */
export const WithDescription: Story = {
  name: "With description",
  parameters: {
    docs: {
      description: {
        story:
          "Two-line toast: a title plus a supporting description. The " +
          "description is wired as the toast's `aria-describedby` " +
          "target, so screen readers announce title then description.",
      },
    },
  },
  render: () => {
    function Trigger() {
      const { toast } = useToast();
      return (
        <Button
          data-testid="toast-description-trigger"
          onClick={() =>
            toast({
              id: "desc-demo",
              title: "Settings saved",
              description: "Your preferences will sync to all devices.",
            })
          }
        >
          Show description toast
        </Button>
      );
    }
    return (
      <Wrap>
        <Trigger />
      </Wrap>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    await userEvent.click(
      canvas.getByRole("button", { name: /show description toast/i }),
    );
    const toast = await findToastByTitle(canvasElement, /settings saved/i);
    expectToastShown(toast);
    await expect(
      getDocument(canvasElement).getByText(/sync to all devices/i),
    ).toBeVisible();
  },
};

/* ─── 3. WithAction ─────────────────────────────────────────────────── */

/** Window-level slot the WithAction story bumps on every action-button
 * click. The aria-wiring runner reads this from the page evaluate so the
 * regression for fix F2 can assert the callback fired EXACTLY once
 * (pre-fix it fired twice because both `entry.actionProps.onClick` and
 * the spread-through `elementProps.onClick` reached `mergeProps`). */
declare global {
  interface Window {
    __zsToastActionCalls?: number;
  }
}

export const WithAction: Story = {
  name: "With action",
  parameters: {
    docs: {
      description: {
        story:
          "Toast carrying an inline action button. Clicking the action " +
          "runs the callback AND dismisses the toast (Base UI's `Action` " +
          "subpart wires both behaviors).",
      },
    },
  },
  render: () => {
    function Trigger() {
      const { toast } = useToast();
      const actionCountRef = useRef(0);
      return (
        <div style={{ display: "flex", gap: "0.5rem" }}>
          <Button
            data-testid="toast-action-trigger"
            onClick={() => {
              // Reset the window-level counter on every fresh trigger so
              // a re-mount story run starts from zero — the regression
              // for F2 asserts the post-click value is EXACTLY 1.
              if (typeof window !== "undefined") {
                window.__zsToastActionCalls = 0;
              }
              toast({
                id: "action-demo",
                title: "Message deleted",
                description: "You can recover it within 30 days.",
                action: {
                  label: "Undo",
                  onClick: () => {
                    actionCountRef.current += 1;
                    if (typeof window !== "undefined") {
                      window.__zsToastActionCalls =
                        (window.__zsToastActionCalls ?? 0) + 1;
                    }
                  },
                },
              });
            }}
          >
            Show action toast
          </Button>
        </div>
      );
    }
    return (
      <Wrap>
        <Trigger />
      </Wrap>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = getDocument(canvasElement);

    await userEvent.click(
      canvas.getByRole("button", { name: /show action toast/i }),
    );
    await expect(
      await findToastByTitle(canvasElement, /message deleted/i),
    ).toBeVisible();
    await expandViewport(canvasElement);
    await userEvent.click(body.getByRole("button", { name: /undo/i }));
    await waitForToastGone(canvasElement, /message deleted/i);
    // F2 regression: action callback must fire EXACTLY once. Pre-fix
    // the default render loop spread `{...entry.actionProps}` AND Base
    // UI's `ToastAction` consumed `toast.actionProps` from root context,
    // so `mergeProps` chained the same `onClick` twice.
    await expect(window.__zsToastActionCalls).toBe(1);
  },
};

/* ─── 4. Success variant ────────────────────────────────────────────── */
export const Success: Story = {
  name: "Variant: success",
  parameters: {
    docs: {
      description: {
        story:
          "Success variant — green leading dot. `role=\"status\"` + " +
          "`aria-live=\"polite\"` (positive feedback is non-urgent).",
      },
    },
  },
  render: () => {
    function Trigger() {
      const { toast } = useToast();
      return (
        <Button
          data-testid="toast-success-trigger"
          onClick={() =>
            toast.success({
              id: "success-demo",
              title: "Backup complete",
              description: "32 files were uploaded successfully.",
            })
          }
        >
          Show success
        </Button>
      );
    }
    return (
      <Wrap>
        <Trigger />
      </Wrap>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    await userEvent.click(
      canvas.getByRole("button", { name: /show success/i }),
    );
    const toast = await findToastByTitle(canvasElement, /backup complete/i);
    expectToastShown(toast);
    await expect(toast).toHaveAttribute("aria-live", "polite");
    await expect(toast).toHaveAttribute("data-variant", "success");
  },
};

/* ─── 5. Error variant ──────────────────────────────────────────────── */
export const ErrorVariant: Story = {
  name: "Variant: error",
  parameters: {
    docs: {
      description: {
        story:
          "Error variant — red leading dot. `role=\"alert\"` + " +
          "`aria-live=\"assertive\"`; screen readers interrupt the " +
          "current speech queue to announce the error.",
      },
    },
  },
  render: () => {
    function Trigger() {
      const { toast } = useToast();
      return (
        <Button
          data-testid="toast-error-trigger"
          onClick={() =>
            toast.error({
              id: "error-demo",
              title: "Upload failed",
              description: "Check your network connection and try again.",
              action: {
                label: "Retry",
                onClick: () => {},
              },
            })
          }
        >
          Show error
        </Button>
      );
    }
    return (
      <Wrap>
        <Trigger />
      </Wrap>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = getDocument(canvasElement);

    await userEvent.click(canvas.getByRole("button", { name: /show error/i }));
    const toast = await findToastByTitle(canvasElement, /upload failed/i, "alert");
    expectToastShown(toast);
    await expect(toast).toHaveAttribute("aria-live", "assertive");
    await expect(toast).toHaveAttribute("data-variant", "error");

    await expandViewport(canvasElement);
    await userEvent.click(body.getByRole("button", { name: /retry/i }));
    await waitForToastGone(canvasElement, /upload failed/i);
  },
};

/* ─── 6. Warning variant ────────────────────────────────────────────── */
export const Warning: Story = {
  name: "Variant: warning",
  parameters: {
    docs: {
      description: {
        story:
          "Warning variant — orange leading dot. `role=\"status\"` + " +
          "`aria-live=\"assertive\"` so the announcement interrupts " +
          "but the toast doesn't carry alert-level urgency.",
      },
    },
  },
  render: () => {
    function Trigger() {
      const { toast } = useToast();
      return (
        <Button
          data-testid="toast-warning-trigger"
          onClick={() =>
            toast.warning({
              id: "warning-demo",
              title: "Storage almost full",
              description: "You're using 96% of your plan.",
            })
          }
        >
          Show warning
        </Button>
      );
    }
    return (
      <Wrap>
        <Trigger />
      </Wrap>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    await userEvent.click(
      canvas.getByRole("button", { name: /show warning/i }),
    );
    const toast = await findToastByTitle(canvasElement, /storage almost full/i);
    expectToastShown(toast);
    await expect(toast).toHaveAttribute("aria-live", "assertive");
    await expect(toast).toHaveAttribute("data-variant", "warning");
  },
};

/* ─── 7. Info variant ───────────────────────────────────────────────── */
export const Info: Story = {
  name: "Variant: info",
  parameters: {
    docs: {
      description: {
        story:
          "Info variant — accent-blue leading dot. `role=\"status\"` + " +
          "`aria-live=\"polite\"`.",
      },
    },
  },
  render: () => {
    function Trigger() {
      const { toast } = useToast();
      return (
        <Button
          data-testid="toast-info-trigger"
          onClick={() =>
            toast.info({
              id: "info-demo",
              title: "New build available",
              description: "Restart to apply version 4.2.0.",
            })
          }
        >
          Show info
        </Button>
      );
    }
    return (
      <Wrap>
        <Trigger />
      </Wrap>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    await userEvent.click(canvas.getByRole("button", { name: /show info/i }));
    const toast = await findToastByTitle(canvasElement, /new build available/i);
    expectToastShown(toast);
    await expect(toast).toHaveAttribute("aria-live", "polite");
    await expect(toast).toHaveAttribute("data-variant", "info");
  },
};

/* ─── 8. LongDuration ───────────────────────────────────────────────── */
export const LongDuration: Story = {
  name: "Long duration (10s)",
  parameters: {
    docs: {
      description: {
        story:
          "Override the per-toast duration to 10 seconds. Useful when a " +
          "toast's content needs a deliberate read (longer description " +
          "or an action the user should consider).",
      },
    },
  },
  render: () => {
    function Trigger() {
      const { toast } = useToast();
      return (
        <Button
          data-testid="toast-long-trigger"
          onClick={() =>
            toast({
              id: "long-demo",
              title: "Scheduled maintenance",
              description:
                "Workers will pause briefly between 14:00 and 14:05 UTC.",
              duration: 10_000,
            })
          }
        >
          Show 10s toast
        </Button>
      );
    }
    return (
      <Wrap>
        <Trigger />
      </Wrap>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    await userEvent.click(
      canvas.getByRole("button", { name: /show 10s toast/i }),
    );
    await expect(
      await findToastByTitle(canvasElement, /scheduled maintenance/i),
    ).toBeVisible();
    await expect(
      getDocument(canvasElement).getByText(/workers will pause briefly/i),
    ).toBeVisible();
  },
};

/* ─── 9. Persistent ─────────────────────────────────────────────────── */
export const Persistent: Story = {
  name: "Persistent (duration: 0)",
  parameters: {
    docs: {
      description: {
        story:
          "`duration: 0` makes the toast persistent — the auto-dismiss " +
          "timer never fires. Manual dismissal (close button, swipe, or " +
          "`dismiss(id)`) is the only way to close it.",
      },
    },
  },
  render: () => {
    function Trigger() {
      const { toast, dismiss } = useToast();
      return (
        <div style={{ display: "flex", gap: "0.5rem" }}>
          <Button
            data-testid="toast-persistent-trigger"
            onClick={() =>
              toast({
                id: "persistent-demo",
                title: "Connection lost",
                description: "Reconnecting…",
                duration: 0,
                variant: "warning",
              })
            }
          >
            Show persistent
          </Button>
          <Button
            variant="tinted"
            data-testid="toast-persistent-dismiss"
            onClick={() => dismiss("persistent-demo")}
          >
            Dismiss persistent
          </Button>
        </div>
      );
    }
    return (
      <Wrap>
        <Trigger />
      </Wrap>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    await userEvent.click(
      canvas.getByRole("button", { name: /show persistent/i }),
    );
    expectToastShown(
      await findToastByTitle(canvasElement, /connection lost/i),
    );

    await userEvent.click(
      canvas.getByRole("button", { name: /dismiss persistent/i }),
    );
    await waitForToastGone(canvasElement, /connection lost/i);
  },
};

/* ─── 10. ImperativeUpdate ──────────────────────────────────────────── */
export const ImperativeUpdate: Story = {
  name: "Imperative update (same id)",
  parameters: {
    docs: {
      description: {
        story:
          "Calling `toast({ id: \"x\" })` twice UPDATES the existing " +
          "toast in place (no second mount, no flicker). Base UI bumps " +
          "the toast's `updateKey` and resets the auto-dismiss timer.",
      },
    },
  },
  render: () => {
    function Trigger() {
      const { toast } = useToast();
      return (
        <div style={{ display: "flex", gap: "0.5rem" }}>
          <Button
            data-testid="toast-update-start"
            onClick={() =>
              toast({
                id: "update-demo",
                title: "Uploading…",
                description: "Processing 12 files.",
                duration: 0,
                variant: "info",
              })
            }
          >
            Start upload
          </Button>
          <Button
            data-testid="toast-update-finish"
            onClick={() =>
              toast({
                id: "update-demo",
                title: "Upload complete",
                description: "12 files synced.",
                variant: "success",
              })
            }
          >
            Finish upload
          </Button>
        </div>
      );
    }
    return (
      <Wrap>
        <Trigger />
      </Wrap>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = getDocument(canvasElement);

    await userEvent.click(canvas.getByRole("button", { name: /start upload/i }));
    expectToastShown(await findToastByTitle(canvasElement, /uploading/i));

    await userEvent.click(
      canvas.getByRole("button", { name: /finish upload/i }),
    );
    await expect(body.getByText(/upload complete/i)).toBeVisible();
    await expect(body.queryByText(/processing 12 files/i)).not.toBeInTheDocument();
    // Same-id contract: the manager upserts by id (Base UI's `add` with
    // an existing id resets fields + restarts the auto-dismiss timer
    // instead of mounting a second toast), so the stack must still
    // contain EXACTLY one root after the second emit.
    const rootCount = canvasElement.ownerDocument.querySelectorAll(
      ".zs-toast-root",
    ).length;
    await expect(rootCount).toBe(1);
  },
};

/* ─── 11. Stacked ───────────────────────────────────────────────────── */
export const Stacked: Story = {
  name: "Stacked (3 simultaneous)",
  parameters: {
    docs: {
      description: {
        story:
          "Three toasts emitted in quick succession. The Provider's " +
          "`limit` (default 3) caps how many are visible at once; " +
          "exceeding the limit closes the oldest.",
      },
    },
  },
  render: () => {
    function Trigger() {
      const { toast } = useToast();
      return (
        <Button
          data-testid="toast-stacked-trigger"
          onClick={() => {
            toast({ id: "stack-1", title: "First", variant: "info" });
            toast({ id: "stack-2", title: "Second", variant: "success" });
            toast({ id: "stack-3", title: "Third", variant: "warning" });
          }}
        >
          Show three
        </Button>
      );
    }
    return (
      <Wrap>
        <Trigger />
      </Wrap>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    await userEvent.click(canvas.getByRole("button", { name: /show three/i }));
    // Use the shared helper rather than a bare `getByText`. Two reasons,
    // both faithful to how every other Toast story queries:
    //   1. Base UI mirrors each toast's title into a visually-hidden
    //      aria-live announcer, so `getByText("Third")` matches BOTH the
    //      visible `<h2 class="zs-toast-title">` and the hidden announcer
    //      node — an ambiguous-match throw. `findToastByTitle` scopes to
    //      the `[role="status"]` toast surface.
    //   2. It waits out the enter motion (data-starting-style → resting
    //      opacity) instead of racing it synchronously.
    // `expectToastShown` then asserts the headline is on screen (the
    // Title isn't aria-hidden even when the high-priority root is).
    expectToastShown(await findToastByTitle(canvasElement, /^first$/i));
    expectToastShown(await findToastByTitle(canvasElement, /^second$/i));
    expectToastShown(await findToastByTitle(canvasElement, /^third$/i));
  },
};

/* ─── 12. PositionTop ───────────────────────────────────────────────── */
export const PositionTop: Story = {
  name: "Position: top-end",
  parameters: {
    docs: {
      description: {
        story:
          "Viewport anchored to the top-end (top-right under LTR). " +
          "Toasts slide in from the trailing inline edge.",
      },
    },
  },
  render: () => {
    function Trigger() {
      const { toast } = useToast();
      return (
        <Button
          data-testid="toast-position-top-trigger"
          onClick={() =>
            toast({
              id: "position-top",
              title: "Pinned to top",
              variant: "info",
            })
          }
        >
          Show top toast
        </Button>
      );
    }
    return (
      <Wrap position="top-end">
        <Trigger />
      </Wrap>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    await userEvent.click(
      canvas.getByRole("button", { name: /show top toast/i }),
    );
    await expect(
      await findToastByTitle(canvasElement, /pinned to top/i),
    ).toBeVisible();
  },
};

/* ─── 13. PositionBottom ────────────────────────────────────────────── */
export const PositionBottom: Story = {
  name: "Position: bottom-start",
  parameters: {
    docs: {
      description: {
        story:
          "Viewport anchored to the bottom-start (bottom-left under " +
          "LTR). Toasts slide in from the leading inline edge.",
      },
    },
  },
  render: () => {
    function Trigger() {
      const { toast } = useToast();
      return (
        <Button
          data-testid="toast-position-bottom-trigger"
          onClick={() =>
            toast({
              id: "position-bottom",
              title: "Pinned to bottom-start",
              variant: "success",
            })
          }
        >
          Show bottom-start toast
        </Button>
      );
    }
    return (
      <Wrap position="bottom-start">
        <Trigger />
      </Wrap>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    await userEvent.click(
      canvas.getByRole("button", { name: /show bottom-start toast/i }),
    );
    await expect(
      await findToastByTitle(canvasElement, /pinned to bottom-start/i),
    ).toBeVisible();
  },
};

/* ─── 14. SwipeToDismiss ────────────────────────────────────────────── */
export const SwipeToDismiss: Story = {
  name: "Swipe to dismiss",
  parameters: {
    docs: {
      description: {
        story:
          "Toasts can be swiped along the anchored axis to dismiss. " +
          "Default swipe directions are end + down so a bottom-end " +
          "viewport accepts swipe-right or swipe-down. Pointer drag " +
          "is followed via the `--toast-swipe-movement-*` CSS vars.",
      },
    },
  },
  render: () => {
    function Trigger() {
      const { toast } = useToast();
      return (
        <Button
          data-testid="toast-swipe-trigger"
          onClick={() =>
            toast({
              id: "swipe-demo",
              title: "Swipe me away",
              description: "Drag right or down to dismiss.",
              duration: 0,
            })
          }
        >
          Show swipeable toast
        </Button>
      );
    }
    return (
      <Wrap>
        <Trigger />
      </Wrap>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = getDocument(canvasElement);

    await userEvent.click(
      canvas.getByRole("button", { name: /show swipeable toast/i }),
    );
    await expect(
      await findToastByTitle(canvasElement, /swipe me away/i),
    ).toBeVisible();
    await expandViewport(canvasElement);
    await userEvent.click(
      body.getByRole("button", { name: /dismiss notification/i }),
    );
    await waitForToastGone(canvasElement, /swipe me away/i);
  },
};

/* ─── 15. RTL ───────────────────────────────────────────────────────── */
export const Rtl: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew toast content under `direction: rtl`. Logical " +
          "properties keep padding axis-correct; the bottom-end " +
          "Viewport flips to bottom-LEFT under RTL.",
      },
    },
  },
  render: () => {
    function Trigger() {
      const { toast } = useToast();
      return (
        <Button
          data-testid="toast-rtl-trigger"
          onClick={() =>
            toast({
              id: "rtl-demo",
              title: "התראה",
              description: "השינויים שלך נשמרו.",
              variant: "success",
            })
          }
        >
          הצג הודעה
        </Button>
      );
    }
    return (
      <div dir="rtl" lang="he">
        <Wrap>
          <Trigger />
        </Wrap>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    await userEvent.click(canvas.getByRole("button", { name: /הצג הודעה/i }));
    const toast = await findToastByTitle(canvasElement, /התראה/i);
    expectToastShown(toast);
    await expect(toast).toHaveAttribute("data-variant", "success");
  },
};

/* ─── 16. AriaOverrideAttempt ───────────────────────────────────────── *
 *
 * Regression coverage for the role/aria-live lock (review fix F3).
 *
 * Reaches in via a runtime cast and tries to pass `role="navigation"`
 * + `aria-live="off"` directly to `Toast.Root`. The internal contract
 * — variant-resolved role and aria-live applied AFTER the spread, with
 * the keys also stripped from `...rest` — must keep the live-region
 * mapping intact so screen-reader semantics can't be silently
 * downgraded by a careless compound-API consumer. */
export const AriaOverrideAttempt: Story = {
  name: "ARIA override attempt (custom Viewport)",
  parameters: {
    docs: {
      description: {
        story:
          "Custom Viewport child reaches in via `as unknown as never` and " +
          "passes `role=\"navigation\"` + `aria-live=\"off\"`. The " +
          "rendered toast must still carry `role=\"alert\"` + " +
          "`aria-live=\"assertive\"` from the error variant — locked " +
          "props win regardless of caller intent.",
      },
    },
  },
  render: () => {
    function CustomList() {
      const manager = useToastManager();
      return (
        <>
          {manager.toasts.map((entry: ToastPayload) => (
            <Toast.Root
              key={entry.id}
              toast={entry}
              // Force-feed the override path: a consumer with a stray
              // cast (or a wrapper that re-emits arbitrary attrs) MUST
              // NOT be able to clobber the brief-mandated ARIA mapping.
              {...({
                role: "navigation",
                "aria-live": "off",
              } as unknown as Record<string, never>)}
            >
              <div className="zs-toast-content">
                {entry.title ? (
                  <Toast.Title>{entry.title}</Toast.Title>
                ) : null}
                {entry.description ? (
                  <Toast.Description>{entry.description}</Toast.Description>
                ) : null}
              </div>
              <Toast.Close />
            </Toast.Root>
          ))}
        </>
      );
    }
    function Trigger() {
      const { toast } = useToast();
      return (
        <Button
          data-testid="toast-aria-override-trigger"
          onClick={() =>
            toast.error({
              id: "aria-override-demo",
              title: "Override blocked",
              description: "Internal ARIA contract wins.",
            })
          }
        >
          Trigger override attempt
        </Button>
      );
    }
    return (
      <Toast.Provider>
        <div className="zs-story-row" role="group" aria-label="Toast demo">
          <Trigger />
        </div>
        <Toast.Viewport position="bottom-end">
          <CustomList />
        </Toast.Viewport>
      </Toast.Provider>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    await userEvent.click(
      canvas.getByRole("button", { name: /trigger override attempt/i }),
    );
    const toast = await findToastByTitle(canvasElement, /override blocked/i, "alert");
    // Variant-locked role/aria-live MUST win regardless of the override
    // attempt above. Pre-fix the consumer override survived because
    // `{...rest}` was spread after the internal props on `<BaseToast.Root>`.
    await expect(toast).toHaveAttribute("role", "alert");
    await expect(toast).toHaveAttribute("aria-live", "assertive");
  },
};

/* ─── 17. CloseLabelDefault ─────────────────────────────────────────── *
 *
 * Regression coverage for two locked-together fixes on Toast.Close:
 *
 *   F-aria-label: the default `aria-label="Dismiss notification"`
 *     must survive the common spread pattern `aria-label={maybeLabel}`
 *     when `maybeLabel` is `undefined`. Pre-fix the wrapper applied the
 *     default BEFORE `{...rest}`, so spreading an undefined value blew
 *     the default away and the rendered button had no accessible name.
 *
 *   F-aria-hidden: Base UI parks `aria-hidden="true"` on the close
 *     button until the viewport is expanded (see Base UI
 *     close/ToastClose.js:49). The button itself is focusable, so axe
 *     fires `aria-hidden-focus` (critical). Our wrapper forces
 *     `aria-hidden={undefined}` AFTER the spread to mirror the same
 *     treatment Toast.Root applies on high-priority toasts (the brief
 *     covers AT discoverability via the live region; the redundant
 *     aria-hidden costs us testability + axe-cleanliness for no gain
 *     on modern screen readers).
 *
 * Story renders a custom-Viewport child that passes
 * `aria-label={undefined}` directly through to `<Toast.Close>`.
 * `play()` asserts both the default label AND the absence of
 * `aria-hidden="true"` at rest. */
export const CloseLabelDefault: Story = {
  name: "Close button — default label + axe-clean at rest",
  parameters: {
    docs: {
      description: {
        story:
          "Regression: `Toast.Close` keeps its default " +
          "`aria-label=\"Dismiss notification\"` when a consumer " +
          "spreads `aria-label={undefined}`, AND it never carries " +
          "`aria-hidden=\"true\"` (which would make the button a " +
          "focusable element inside an aria-hidden subtree — an " +
          "`aria-hidden-focus` axe violation).",
      },
    },
  },
  render: () => {
    function CustomList() {
      const manager = useToastManager();
      return (
        <>
          {manager.toasts.map((entry: ToastPayload) => (
            <Toast.Root key={entry.id} toast={entry}>
              <div className="zs-toast-content">
                {entry.title ? (
                  <Toast.Title>{entry.title}</Toast.Title>
                ) : null}
              </div>
              {/* The common spread pattern: a caller forwards
               * `aria-label={maybeLabel}` from props where `maybeLabel`
               * is `undefined`. Pre-fix this clobbered the default. */}
              <Toast.Close aria-label={undefined} />
            </Toast.Root>
          ))}
        </>
      );
    }
    function Trigger() {
      const { toast } = useToast();
      return (
        <Button
          data-testid="toast-close-default-label-trigger"
          onClick={() =>
            toast({
              id: "close-default-label-demo",
              title: "Default label survives",
            })
          }
        >
          Show toast (custom close)
        </Button>
      );
    }
    return (
      <Toast.Provider>
        <div className="zs-story-row" role="group" aria-label="Toast demo">
          <Trigger />
        </div>
        <Toast.Viewport position="bottom-end">
          <CustomList />
        </Toast.Viewport>
      </Toast.Provider>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = getDocument(canvasElement);

    await userEvent.click(
      canvas.getByRole("button", { name: /show toast \(custom close\)/i }),
    );
    const toast = await findToastByTitle(
      canvasElement,
      /default label survives/i,
    );
    expectToastShown(toast);

    // The close button must be findable by its DEFAULT accessible
    // name even after the caller passed `aria-label={undefined}`.
    // Pre-fix the spread blew the default away and this query failed.
    const closeBtn = body.getByRole("button", {
      name: /dismiss notification/i,
    });
    await expect(closeBtn).toHaveAttribute(
      "aria-label",
      "Dismiss notification",
    );
    // The close button must not carry aria-hidden="true" at rest —
    // pre-fix Base UI parked it there until viewport expansion, which
    // axe flags as `aria-hidden-focus` because the button itself is
    // focusable. We force `aria-hidden={undefined}` so the attribute
    // is omitted entirely.
    await expect(closeBtn).not.toHaveAttribute("aria-hidden", "true");
  },
};
