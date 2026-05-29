import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, within } from "@storybook/test";
import { useState } from "react";
import { AlertDialog, Button, Input } from "../components";

const meta: Meta<typeof AlertDialog> = {
  title: "Components/AlertDialog",
  component: AlertDialog,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof AlertDialog>;

/* ─── 1. One button ──────────────────────────────────────────────────── */
export const OneButton: Story = {
  name: "One button",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="AlertDialog one button">
      <AlertDialog>
        <AlertDialog.Trigger
          render={<Button data-testid="alertdialog-trigger">Show confirmation</Button>}
        />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup data-testid="alertdialog-one-button">
            <AlertDialog.Header>
              <AlertDialog.Title>Saved</AlertDialog.Title>
              <AlertDialog.Description>
                Your changes were saved successfully.
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Footer>
              <AlertDialog.Action>OK</AlertDialog.Action>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  ),
};

/* ─── 2. Two buttons ─────────────────────────────────────────────────── */
export const TwoButtons: Story = {
  name: "Two buttons",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="AlertDialog two buttons">
      <AlertDialog>
        <AlertDialog.Trigger
          render={<Button data-testid="alertdialog-trigger">Discard changes</Button>}
        />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup data-testid="alertdialog-two-buttons">
            <AlertDialog.Header>
              <AlertDialog.Title>Discard changes?</AlertDialog.Title>
              <AlertDialog.Description>
                Your edits will be lost. This can't be undone.
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Footer>
              <AlertDialog.Cancel>Keep editing</AlertDialog.Cancel>
              <AlertDialog.Action>Discard</AlertDialog.Action>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", {
      name: /discard changes/i,
    }));
    await expect(await page.findByRole("alertdialog", {
      name: /discard changes/i,
    })).toBeVisible();

    await userEvent.click(page.getByRole("button", { name: /keep editing/i }));
  },
};

/* ─── 3. Destructive ─────────────────────────────────────────────────── */
export const Destructive: Story = {
  name: "Destructive",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Destructive AlertDialog">
      <AlertDialog>
        <AlertDialog.Trigger
          render={
            <Button intent="destructive" data-testid="alertdialog-trigger">
              Delete account
            </Button>
          }
        />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup data-testid="alertdialog-destructive">
            <AlertDialog.Header>
              <AlertDialog.Title>Delete account?</AlertDialog.Title>
              <AlertDialog.Description>
                All of your projects, sessions, and identity data will be
                permanently removed.
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Footer>
              <AlertDialog.Cancel>Cancel</AlertDialog.Cancel>
              <AlertDialog.Action tone="destructive">
                Delete forever
              </AlertDialog.Action>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  ),
};

/* ─── 4. Three buttons ───────────────────────────────────────────────── */
export const ThreeButtons: Story = {
  name: "Three buttons",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="AlertDialog three buttons">
      <AlertDialog>
        <AlertDialog.Trigger
          render={<Button data-testid="alertdialog-trigger">Review unsaved changes</Button>}
        />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup data-testid="alertdialog-three-buttons">
            <AlertDialog.Header>
              <AlertDialog.Title>Unsaved changes</AlertDialog.Title>
              <AlertDialog.Description>
                You have three options. Stacked vertically with the
                destructive option at the bottom.
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Footer>
              <AlertDialog.Action>Save and continue</AlertDialog.Action>
              <AlertDialog.Cancel>Keep editing</AlertDialog.Cancel>
              <AlertDialog.Action tone="destructive">
                Discard changes
              </AlertDialog.Action>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  ),
};

/* ─── 5. With body ───────────────────────────────────────────────────── */
export const WithBody: Story = {
  name: "With body",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="AlertDialog with body input">
      <AlertDialog>
        <AlertDialog.Trigger
          render={
            <Button intent="destructive" data-testid="alertdialog-trigger">
              Delete project
            </Button>
          }
        />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup data-testid="alertdialog-with-body">
            <AlertDialog.Header>
              <AlertDialog.Title>Delete this project?</AlertDialog.Title>
              <AlertDialog.Description>
                Type the project name to confirm.
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Body>
              <Input
                type="text"
                aria-label="Type project name"
                placeholder="my-app"
                data-testid="alertdialog-confirm-input"
              />
            </AlertDialog.Body>
            <AlertDialog.Footer>
              <AlertDialog.Cancel>Cancel</AlertDialog.Cancel>
              <AlertDialog.Action tone="destructive">Delete</AlertDialog.Action>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", {
      name: /delete project/i,
    }));
    await page.findByRole("alertdialog", { name: /delete this project/i });

    const confirmation = page.getByRole("textbox", {
      name: /type project name/i,
    });
    await userEvent.type(confirmation, "my-app");
    await expect(confirmation).toHaveValue("my-app");

    await userEvent.click(page.getByRole("button", { name: /^delete$/i }));
  },
};

/* ─── 6. Outside click ignored ───────────────────────────────────────── */
export const OutsideClickIgnored: Story = {
  name: "Outside click ignored",
  parameters: {
    docs: {
      description: {
        story:
          "Try clicking outside the popup — nothing happens. AlertDialog ignores outside-press by design (anti-pattern #6).",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="AlertDialog outside click ignored">
      <AlertDialog>
        <AlertDialog.Trigger
          render={<Button data-testid="alertdialog-trigger">Confirm exit</Button>}
        />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup data-testid="alertdialog-outside-click">
            <AlertDialog.Header>
              <AlertDialog.Title>Confirm exit</AlertDialog.Title>
              <AlertDialog.Description>
                Click anywhere outside this popup. The alert stays open —
                outside-click never dismisses an alert.
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Footer>
              <AlertDialog.Cancel>Stay</AlertDialog.Cancel>
              <AlertDialog.Action>Exit</AlertDialog.Action>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", { name: /confirm exit/i }));
    const alert = await page.findByRole("alertdialog", {
      name: /confirm exit/i,
    });

    await userEvent.click(canvasElement.ownerDocument.body);
    await expect(alert).toBeVisible();
    await userEvent.click(page.getByRole("button", { name: /stay/i }));
  },
};

/* ─── 7. ESC closes via Cancel (review-fix item 1) ───────────────────── */
function EscClosesCancelStory() {
  // Status lives OUTSIDE the popup so it survives the close animation —
  // letting the aria-wiring assertion verify Cancel's onClick fired
  // AFTER ESC routed through.
  const [clicked, setClicked] = useState<string>("not-clicked");
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="AlertDialog ESC closes via Cancel"
    >
      <p
        role="status"
        aria-label="Cancel click status"
        data-testid="cancel-clicked-status"
      >
        Status: {clicked}
      </p>
      <AlertDialog>
        <AlertDialog.Trigger
          render={
            <Button data-testid="alertdialog-trigger">Press ESC to cancel</Button>
          }
        />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup data-testid="alertdialog-esc-closes-cancel">
            <AlertDialog.Header>
              <AlertDialog.Title>Discard changes?</AlertDialog.Title>
              <AlertDialog.Description>
                Press ESC. The Cancel button's onClick fires (status
                line below the trigger flips to &ldquo;cancelled&rdquo;)
                AND the popup closes (review-fix item 1).
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Footer>
              <AlertDialog.Cancel onClick={() => setClicked("cancelled")}>
                Keep editing
              </AlertDialog.Cancel>
              <AlertDialog.Action>Discard</AlertDialog.Action>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  );
}
export const EscClosesCancel: Story = {
  name: "ESC closes via Cancel",
  render: () => <EscClosesCancelStory />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);
    const status = canvas.getByRole("status", { name: /cancel click status/i });

    await userEvent.click(canvas.getByRole("button", {
      name: /press esc to cancel/i,
    }));
    await page.findByRole("alertdialog", { name: /discard changes/i });

    await userEvent.keyboard("{Escape}");
    await expect(status).toHaveTextContent("Status: cancelled");
  },
};

/* ─── 8. ESC no-ops when no Cancel (review-fix item 1) ───────────────── */
export const EscNoOpsWithoutCancel: Story = {
  name: "ESC no-ops without Cancel",
  parameters: {
    docs: {
      description: {
        story:
          "An alert with no Cancel is hard-modal: ESC does nothing. The user must press the Action to dismiss (review-fix item 1).",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="AlertDialog ESC no-ops without Cancel"
    >
      <AlertDialog>
        <AlertDialog.Trigger
          render={
            <Button data-testid="alertdialog-trigger">Open hard alert</Button>
          }
        />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup data-testid="alertdialog-esc-noop">
            <AlertDialog.Header>
              <AlertDialog.Title>Action required</AlertDialog.Title>
              <AlertDialog.Description>
                Press ESC — nothing happens. There is no Cancel, so the
                alert is hard-modal. Use the Action to dismiss.
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Footer>
              <AlertDialog.Action>Acknowledge</AlertDialog.Action>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", {
      name: /open hard alert/i,
    }));
    const alert = await page.findByRole("alertdialog", {
      name: /action required/i,
    });

    await userEvent.keyboard("{Escape}");
    await expect(alert).toBeVisible();
    await userEvent.click(page.getByRole("button", { name: /acknowledge/i }));
  },
};

/* ─── 9. ESC ignores a disabled Cancel (review-fix item 1) ──────────── */
export const EscIgnoresDisabledCancel: Story = {
  name: "ESC ignores disabled Cancel",
  parameters: {
    docs: {
      description: {
        story:
          "A disabled Cancel is not a valid dismissal target — ESC stays a no-op (review-fix item 1).",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="AlertDialog ESC ignores disabled Cancel"
    >
      <AlertDialog>
        <AlertDialog.Trigger
          render={
            <Button data-testid="alertdialog-trigger">
              Open with disabled Cancel
            </Button>
          }
        />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup data-testid="alertdialog-esc-disabled-cancel">
            <AlertDialog.Header>
              <AlertDialog.Title>Processing…</AlertDialog.Title>
              <AlertDialog.Description>
                Cancel is disabled while work is in flight. ESC should
                stay a no-op until Cancel becomes enabled again.
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Footer>
              <AlertDialog.Cancel disabled>Cancel (disabled)</AlertDialog.Cancel>
              <AlertDialog.Action>Wait</AlertDialog.Action>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", {
      name: /open with disabled cancel/i,
    }));
    const alert = await page.findByRole("alertdialog", {
      name: /processing/i,
    });
    await expect(page.getByRole("button", {
      name: /cancel \(disabled\)/i,
    })).toBeDisabled();

    await userEvent.keyboard("{Escape}");
    await expect(alert).toBeVisible();
    await userEvent.click(page.getByRole("button", { name: /wait/i }));
  },
};

/* ─── 10. Cancel onClick composes (review-fix item 2) ────────────────── */
function CancelWithCleanupOnClickStory() {
  const [cleanup, setCleanup] = useState<string>("not-run");
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="AlertDialog Cancel onClick composes with close"
    >
      <p
        role="status"
        aria-label="Cancel cleanup status"
        data-testid="cancel-cleanup-status"
      >
        Status: {cleanup}
      </p>
      <AlertDialog>
        <AlertDialog.Trigger
          render={
            <Button data-testid="alertdialog-trigger">
              Open cleanup alert
            </Button>
          }
        />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup data-testid="alertdialog-cancel-cleanup-popup">
            <AlertDialog.Header>
              <AlertDialog.Title>Discard changes?</AlertDialog.Title>
              <AlertDialog.Description>
                Pressing Cancel fires the caller onClick (flips the
                status line to &ldquo;cleanup-ran&rdquo;) AND closes
                the popup. Both must happen — clobbering one is the
                bug review-fix item 2 guards against.
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Footer>
              <AlertDialog.Cancel
                data-testid="alertdialog-cancel-cleanup-btn"
                onClick={() => setCleanup("cleanup-ran")}
              >
                Cancel
              </AlertDialog.Cancel>
              <AlertDialog.Action tone="destructive">Discard</AlertDialog.Action>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  );
}
export const CancelWithCleanupOnClick: Story = {
  name: "Cancel — onClick composes with close",
  render: () => <CancelWithCleanupOnClickStory />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);
    const status = canvas.getByRole("status", {
      name: /cancel cleanup status/i,
    });

    await userEvent.click(canvas.getByRole("button", {
      name: /open cleanup alert/i,
    }));
    await page.findByRole("alertdialog", { name: /discard changes/i });
    await userEvent.click(page.getByRole("button", { name: /^cancel$/i }));
    await expect(status).toHaveTextContent("Status: cleanup-ran");
  },
};

/* ─── 11. Cancel asChild (review-fix item 7) ─────────────────────────── */
function CancelAsChildStory() {
  const [clicked, setClicked] = useState<string>("not-clicked");
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="AlertDialog Cancel asChild"
    >
      <p
        role="status"
        aria-label="Cancel asChild status"
        data-testid="cancel-aschild-status"
      >
        Status: {clicked}
      </p>
      <AlertDialog>
        <AlertDialog.Trigger
          render={
            <Button data-testid="alertdialog-trigger">
              Open custom-cancel alert
            </Button>
          }
        />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup data-testid="alertdialog-cancel-aschild-popup">
            <AlertDialog.Header>
              <AlertDialog.Title>Custom Cancel target</AlertDialog.Title>
              <AlertDialog.Description>
                The asChild Slot routes className, style, refs, AND
                onClick composition through the shared `_slot.ts`
                helper (review-fix item 7). The child's onClick AND the
                close handler both run.
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Footer>
              <AlertDialog.Cancel asChild>
                <button
                  type="button"
                  className="zs-button zs-button--gray zs-button--medium"
                  data-testid="alertdialog-cancel-aschild-target"
                  onClick={() => setClicked("child-onclick-ran")}
                >
                  Done
                </button>
              </AlertDialog.Cancel>
              <AlertDialog.Action>Proceed</AlertDialog.Action>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  );
}
export const CancelAsChild: Story = {
  name: "Cancel — asChild (Slot)",
  render: () => <CancelAsChildStory />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);
    const status = canvas.getByRole("status", {
      name: /cancel aschild status/i,
    });

    await userEvent.click(canvas.getByRole("button", {
      name: /open custom-cancel alert/i,
    }));
    await page.findByRole("alertdialog", { name: /custom cancel target/i });
    const done = page.getByRole("button", { name: /done/i });

    await expect(done.tagName).toBe("BUTTON");
    await userEvent.click(done);
    await expect(status).toHaveTextContent("Status: child-onclick-ran");
  },
};

function ActionPreventCloseStory() {
  const [status, setStatus] = useState("idle");
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="AlertDialog action preventClose"
    >
      <p role="status" aria-label="Alert action status">
        Status: {status}
      </p>
      <AlertDialog>
        <AlertDialog.Trigger render={<Button>Open async alert</Button>} />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup>
            <AlertDialog.Header>
              <AlertDialog.Title>Check availability?</AlertDialog.Title>
              <AlertDialog.Description>
                The action stays open while the caller handles async work.
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Footer>
              <AlertDialog.Cancel>Cancel</AlertDialog.Cancel>
              <AlertDialog.Action
                preventClose
                onClick={() => setStatus("checked")}
              >
                Check availability
              </AlertDialog.Action>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  );
}
export const ActionPreventClose: Story = {
  name: "Action preventClose (play)",
  render: () => <ActionPreventCloseStory />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);
    const status = canvas.getByRole("status", { name: /alert action status/i });

    await userEvent.click(canvas.getByRole("button", {
      name: /open async alert/i,
    }));
    const alert = await page.findByRole("alertdialog", {
      name: /check availability/i,
    });

    await userEvent.click(page.getByRole("button", {
      name: /check availability/i,
    }));
    await expect(status).toHaveTextContent("Status: checked");
    await expect(alert).toBeVisible();

    await userEvent.click(page.getByRole("button", { name: /^cancel$/i }));
  },
};

function ActionPreventDefaultStory() {
  const [status, setStatus] = useState("idle");
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="AlertDialog action preventDefault"
    >
      <p role="status" aria-label="Alert guarded action status">
        Status: {status}
      </p>
      <AlertDialog>
        <AlertDialog.Trigger render={<Button>Open guarded action</Button>} />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup>
            <AlertDialog.Header>
              <AlertDialog.Title>Guarded action</AlertDialog.Title>
              <AlertDialog.Description>
                The caller prevents the first action close.
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Footer>
              <AlertDialog.Cancel>Cancel</AlertDialog.Cancel>
              <AlertDialog.Action
                onClick={(event) => {
                  event.preventDefault();
                  setStatus("prevented");
                }}
              >
                Try action
              </AlertDialog.Action>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  );
}
export const ActionPreventDefault: Story = {
  name: "Action preventDefault (play)",
  render: () => <ActionPreventDefaultStory />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);
    const status = canvas.getByRole("status", {
      name: /alert guarded action status/i,
    });

    await userEvent.click(canvas.getByRole("button", {
      name: /open guarded action/i,
    }));
    const alert = await page.findByRole("alertdialog", {
      name: /guarded action/i,
    });

    await userEvent.click(page.getByRole("button", { name: /try action/i }));
    await expect(status).toHaveTextContent("Status: prevented");
    await expect(alert).toBeVisible();

    await userEvent.click(page.getByRole("button", { name: /^cancel$/i }));
  },
};

export const FragmentFooterButtons: Story = {
  name: "Fragment footer buttons (play)",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="AlertDialog fragment footer buttons"
    >
      <AlertDialog>
        <AlertDialog.Trigger render={<Button>Open fragment footer</Button>} />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup>
            <AlertDialog.Header>
              <AlertDialog.Title>Archive project?</AlertDialog.Title>
              <AlertDialog.Description>
                Footer child walking handles arrays and fragments.
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Footer>
              {[
                <AlertDialog.Cancel key="cancel">Cancel</AlertDialog.Cancel>,
                null,
              ]}
              <>
                <AlertDialog.Action>Archive</AlertDialog.Action>
              </>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", {
      name: /open fragment footer/i,
    }));
    await expect(await page.findByRole("alertdialog", {
      name: /archive project/i,
    })).toBeVisible();
    await userEvent.click(page.getByRole("button", { name: /^cancel$/i }));
  },
};

/* ─── 12. 3-button destructive at bottom (positive; review-fix item 5) */
export const ThreeButtonsDestructiveBottom: Story = {
  name: "Three buttons — destructive last (correct)",
  parameters: {
    docs: {
      description: {
        story:
          "The destructive Action is last in source order — the contract review-fix item 5 enforces via dev-warn.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="AlertDialog three buttons destructive bottom"
    >
      <AlertDialog>
        <AlertDialog.Trigger
          render={
            <Button data-testid="alertdialog-trigger">
              Three buttons (correct order)
            </Button>
          }
        />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup data-testid="alertdialog-three-destructive-bottom">
            <AlertDialog.Header>
              <AlertDialog.Title>Unsaved changes</AlertDialog.Title>
              <AlertDialog.Description>
                Save, Cancel, then Discard — destructive last. No
                dev-warn fires for this layout.
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Footer>
              <AlertDialog.Action>Save and continue</AlertDialog.Action>
              <AlertDialog.Cancel>Keep editing</AlertDialog.Cancel>
              <AlertDialog.Action tone="destructive">
                Discard changes
              </AlertDialog.Action>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  ),
};

/* ─── 13. 3-button destructive misplaced (negative; review-fix item 5) */
export const ThreeButtonsDestructiveMisplaced: Story = {
  name: "Three buttons — destructive misplaced (warn)",
  parameters: {
    docs: {
      description: {
        story:
          "Negative test: the destructive Action is NOT last → dev-warn fires. axe disabled (the layout is intentional for the warn assertion).",
      },
    },
    // axe will flag the destructive-misplaced layout's intent — but this
    // story exists purely to exercise the dev-warn, so we skip a11y.
    a11y: { disable: true },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="AlertDialog three buttons destructive misplaced"
    >
      <AlertDialog>
        <AlertDialog.Trigger
          render={
            <Button data-testid="alertdialog-trigger">
              Three buttons (warn)
            </Button>
          }
        />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup data-testid="alertdialog-three-destructive-misplaced">
            <AlertDialog.Header>
              <AlertDialog.Title>Misplaced destructive</AlertDialog.Title>
              <AlertDialog.Description>
                Destructive at index 0 — dev-warn fires.
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Footer>
              <AlertDialog.Action tone="destructive">
                Discard changes
              </AlertDialog.Action>
              <AlertDialog.Cancel>Keep editing</AlertDialog.Cancel>
              <AlertDialog.Action>Save and continue</AlertDialog.Action>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  ),
};

/* ─── 14. Destructive without Cancel — warn (review-fix item 6) ─────── */
export const DestructiveWithoutCancelWarns: Story = {
  name: "Destructive without Cancel (warn)",
  parameters: {
    docs: {
      description: {
        story:
          "Destructive action without Cancel = no safe exit. The dev-warn nudges the consumer to add one (review-fix item 6). axe disabled — same reason as ThreeButtonsDestructiveMisplaced.",
      },
    },
    a11y: { disable: true },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="AlertDialog destructive without cancel"
    >
      <AlertDialog>
        <AlertDialog.Trigger
          render={
            <Button intent="destructive" data-testid="alertdialog-trigger">
              No safe exit
            </Button>
          }
        />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup data-testid="alertdialog-destructive-no-cancel">
            <AlertDialog.Header>
              <AlertDialog.Title>Delete forever?</AlertDialog.Title>
              <AlertDialog.Description>
                No Cancel — dev-warn fires.
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Footer>
              <AlertDialog.Action tone="destructive">
                Delete forever
              </AlertDialog.Action>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  ),
};

/* ─── CancelAsChildSingleFire — Round 5 fix #3 (AlertDialog) ─────────── *
 *
 * Regression for Round 5 fix #3 (AlertDialog.Cancel). Pre-fix, the
 * asChild branch manually extracted the child's onClick and called it
 * inside a hand-rolled `slotOnClick`, while ALSO passing the child to
 * Slot — whose `mergeProps` ALREADY composes the child's onClick with
 * `ours`. Result: the child's onClick fired TWICE per click. Post-fix,
 * we drop the manual extraction and let `_slot.ts` own the composition,
 * so the counter increments exactly once. */
function CancelAsChildSingleFireStory() {
  const [count, setCount] = useState(0);
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="AlertDialog Cancel asChild single-fire"
    >
      <p
        role="status"
        aria-label="Cancel single-fire counter"
        data-testid="cancel-singlefire-counter"
      >
        Count: {count}
      </p>
      <AlertDialog>
        <AlertDialog.Trigger
          render={
            <Button data-testid="alertdialog-singlefire-trigger">
              Open single-fire alert
            </Button>
          }
        />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup data-testid="alertdialog-singlefire-popup">
            <AlertDialog.Header>
              <AlertDialog.Title>Single-fire Cancel</AlertDialog.Title>
              <AlertDialog.Description>
                Click the custom Cancel target ONCE. The child onClick
                must fire exactly once (Round 5 fix #3 regression).
              </AlertDialog.Description>
            </AlertDialog.Header>
            <AlertDialog.Footer>
              <AlertDialog.Cancel asChild>
                <button
                  type="button"
                  className="zs-button zs-button--gray zs-button--medium"
                  data-testid="alertdialog-singlefire-target"
                  onClick={() => setCount((value) => value + 1)}
                >
                  Cancel once
                </button>
              </AlertDialog.Cancel>
              <AlertDialog.Action>Proceed</AlertDialog.Action>
            </AlertDialog.Footer>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog>
    </div>
  );
}
export const CancelAsChildSingleFire: Story = {
  name: "Cancel — asChild single-fire (Round 5 regression)",
  render: () => <CancelAsChildSingleFireStory />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);
    const counter = canvas.getByRole("status", {
      name: /cancel single-fire counter/i,
    });

    await expect(counter).toHaveTextContent("Count: 0");
    await userEvent.click(
      canvas.getByRole("button", { name: /open single-fire alert/i }),
    );
    await page.findByRole("alertdialog", { name: /single-fire cancel/i });
    const target = page.getByRole("button", { name: /cancel once/i });
    await userEvent.click(target);
    await expect(counter).toHaveTextContent("Count: 1");
  },
};
