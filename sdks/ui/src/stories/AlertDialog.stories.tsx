import type { Meta, StoryObj } from "@storybook/react";
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
      <AlertDialog defaultOpen>
        <AlertDialog.Trigger render={<Button>Open alert</Button>} />
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
      <AlertDialog defaultOpen>
        <AlertDialog.Trigger render={<Button>Open alert</Button>} />
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
      <AlertDialog defaultOpen>
        <AlertDialog.Trigger
          render={<Button intent="destructive">Delete account</Button>}
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
      <AlertDialog defaultOpen>
        <AlertDialog.Trigger render={<Button>Open</Button>} />
        <AlertDialog.Portal>
          <AlertDialog.Backdrop />
          <AlertDialog.Popup data-testid="alertdialog-three-buttons">
            <AlertDialog.Header>
              <AlertDialog.Title>Unsaved changes</AlertDialog.Title>
              <AlertDialog.Description>
                You have three options. Stacked vertically per HIG with the
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
      <AlertDialog defaultOpen>
        <AlertDialog.Trigger
          render={<Button intent="destructive">Delete project</Button>}
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
      <AlertDialog defaultOpen>
        <AlertDialog.Trigger render={<Button>Open</Button>} />
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
