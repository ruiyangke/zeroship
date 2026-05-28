import type { Meta, StoryObj } from "@storybook/react";
import { useState } from "react";
import { AlertDialog, Button } from "../components";

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
              <input
                type="text"
                aria-label="Type project name"
                placeholder="my-app"
                data-testid="alertdialog-confirm-input"
                style={{
                  inlineSize: "100%",
                  blockSize: "var(--zs-control-h-md)",
                  paddingInline: "var(--zs-control-px-md)",
                  borderRadius: "var(--zs-control-radius-md)",
                  border: "1px solid var(--zs-input-border)",
                  background: "var(--zs-input-bg)",
                  color: "var(--zs-input-ink)",
                  fontFamily: "var(--zs-font-system)",
                }}
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
      <p data-testid="cancel-clicked-status">Status: {clicked}</p>
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
      <p data-testid="cancel-cleanup-status">Status: {cleanup}</p>
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
      <p data-testid="cancel-aschild-status">Status: {clicked}</p>
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
                  className="zs-button zs-button--gray zs-button--md"
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
          "HIG: destructive action without Cancel = no safe exit. The dev-warn nudges the consumer to add one (review-fix item 6). axe disabled — same reason as ThreeButtonsDestructiveMisplaced.",
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
