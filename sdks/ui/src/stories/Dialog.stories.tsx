import type { Meta, StoryObj } from "@storybook/react";
import { useRef } from "react";
import { Button, Dialog, Field, Input } from "../components";

const meta: Meta<typeof Dialog> = {
  title: "Components/Dialog",
  component: Dialog,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Dialog>;

/* ─── 1. Default ─────────────────────────────────────────────────────── */
export const Default: Story = {
  name: "Default",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Default dialog">
      <Dialog defaultOpen>
        <Dialog.Trigger render={<Button>Open dialog</Button>} />
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup data-testid="dialog-default-popup">
            <Dialog.Header>
              <Dialog.Title>Are you sure?</Dialog.Title>
              <Dialog.Description>
                This action will affect your account immediately.
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Body>
              The standard Dialog renders Header / Body / Footer as
              layout-only divs; Title and Description carry the ARIA.
            </Dialog.Body>
            <Dialog.Footer>
              <Dialog.Close>Cancel</Dialog.Close>
              <Button>Continue</Button>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  ),
};

/* ─── 2. Sizes ───────────────────────────────────────────────────────── */
export const Sizes: Story = {
  name: "Sizes",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All dialog sizes">
      {(["sm", "md", "lg", "full"] as const).map((size) => (
        <Dialog key={size}>
          <Dialog.Trigger render={<Button variant="tinted">{size}</Button>} />
          <Dialog.Portal>
            <Dialog.Backdrop />
            <Dialog.Popup size={size} data-testid={`dialog-size-${size}`}>
              <Dialog.Header>
                <Dialog.Title>Size: {size}</Dialog.Title>
                <Dialog.Description>
                  Max-width preset for the popup chrome.
                </Dialog.Description>
              </Dialog.Header>
              <Dialog.Body>
                <p>Content scales to the preset max-width.</p>
              </Dialog.Body>
              <Dialog.Footer>
                <Dialog.Close>Close</Dialog.Close>
              </Dialog.Footer>
            </Dialog.Popup>
          </Dialog.Portal>
        </Dialog>
      ))}
    </div>
  ),
};

/* ─── 3. Placement top ───────────────────────────────────────────────── */
export const PlacementTop: Story = {
  name: "Placement: top",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Dialog top placement">
      <Dialog defaultOpen>
        <Dialog.Trigger render={<Button>Open top sheet</Button>} />
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup placement="top" data-testid="dialog-placement-top">
            <Dialog.Header>
              <Dialog.Title>iPad form-sheet feel</Dialog.Title>
              <Dialog.Description>
                Anchored near the top safe area; spacing respects env(safe-area-inset-top).
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Body>
              Useful for form-sheets on larger screens where center
              placement reads as overlay-heavy.
            </Dialog.Body>
            <Dialog.Footer>
              <Dialog.Close>Done</Dialog.Close>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  ),
};

/* ─── 4. Backdrop tints ──────────────────────────────────────────────── */
export const BackdropTints: Story = {
  name: "Backdrop tints",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Backdrop tints">
      {(["scrim", "material", "none"] as const).map((tint) => (
        <Dialog key={tint}>
          <Dialog.Trigger render={<Button variant="tinted">tint: {tint}</Button>} />
          <Dialog.Portal>
            <Dialog.Backdrop tint={tint} />
            <Dialog.Popup data-testid={`dialog-tint-${tint}`}>
              <Dialog.Header>
                <Dialog.Title>Backdrop tint = {tint}</Dialog.Title>
                <Dialog.Description>
                  scrim is the HIG default; material is glass; none is
                  a transparent click-blocker.
                </Dialog.Description>
              </Dialog.Header>
              <Dialog.Footer>
                <Dialog.Close>Close</Dialog.Close>
              </Dialog.Footer>
            </Dialog.Popup>
          </Dialog.Portal>
        </Dialog>
      ))}
    </div>
  ),
};

/* ─── 5. With form ───────────────────────────────────────────────────── */
export const WithForm: Story = {
  name: "With form",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Dialog containing a form">
      <Dialog defaultOpen>
        <Dialog.Trigger render={<Button>Edit profile</Button>} />
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup data-testid="dialog-with-form">
            <Dialog.Header>
              <Dialog.Title>Edit profile</Dialog.Title>
              <Dialog.Description>
                Focus is trapped while the dialog is open.
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Body>
              <Field>
                <Field.Label>Display name</Field.Label>
                <Input
                  defaultValue="Ada Lovelace"
                  data-testid="dialog-form-name"
                />
              </Field>
              <Field>
                <Field.Label>Email</Field.Label>
                <Input type="email" defaultValue="ada@example.com" />
              </Field>
            </Dialog.Body>
            <Dialog.Footer>
              <Dialog.Close>Cancel</Dialog.Close>
              <Button>Save</Button>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  ),
};

/* ─── 6. Non-dismissible ─────────────────────────────────────────────── */
export const NonDismissible: Story = {
  name: "Non-dismissible",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Non-dismissible dialog">
      <Dialog defaultOpen dismissible={false}>
        <Dialog.Trigger render={<Button>Open</Button>} />
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup data-testid="dialog-non-dismissible">
            <Dialog.Header showClose={false}>
              <Dialog.Title>Required action</Dialog.Title>
              <Dialog.Description>
                Outside-click and ESC are both ignored — you must press
                Acknowledge.
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Body>
              The dismissible=false setting disables BOTH outside-click
              AND escape-key (anti-pattern #12: never disable one without
              the other).
            </Dialog.Body>
            <Dialog.Footer>
              <Dialog.Close variant="filled">Acknowledge</Dialog.Close>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  ),
};

/* ─── 7. Initial focus ───────────────────────────────────────────────── */
function InitialFocusStory() {
  const usernameRef = useRef<HTMLInputElement | null>(null);
  return (
    <div className="zs-story-row" role="group" aria-label="Initial focus dialog">
      <Dialog defaultOpen>
        <Dialog.Trigger render={<Button>Open</Button>} />
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup
            initialFocus={usernameRef}
            data-testid="dialog-initial-focus"
          >
            <Dialog.Header>
              <Dialog.Title>Sign in</Dialog.Title>
              <Dialog.Description>
                Focus lands on the username field via initialFocus.
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Body>
              <Field>
                <Field.Label>Username</Field.Label>
                <Input
                  ref={usernameRef}
                  data-testid="dialog-initial-focus-input"
                  defaultValue=""
                />
              </Field>
              <Field>
                <Field.Label>Password</Field.Label>
                <Input type="password" />
              </Field>
            </Dialog.Body>
            <Dialog.Footer>
              <Dialog.Close>Cancel</Dialog.Close>
              <Button>Sign in</Button>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  );
}
export const InitialFocus: Story = {
  name: "Initial focus",
  render: () => <InitialFocusStory />,
};

/* ─── 8. Nested ──────────────────────────────────────────────────────── */
export const Nested: Story = {
  name: "Nested",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Nested dialogs">
      <Dialog defaultOpen>
        <Dialog.Trigger render={<Button>Open outer</Button>} />
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup data-testid="dialog-nested-outer">
            <Dialog.Header>
              <Dialog.Title>Outer dialog</Dialog.Title>
              <Dialog.Description>
                Open the inner dialog to see the nested-dialogs scale
                transform on this popup.
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Body>
              The --nested-dialogs CSS var Base UI sets drives the scale
              transform.
            </Dialog.Body>
            <Dialog.Footer>
              <Dialog.Close>Cancel</Dialog.Close>
              <Dialog defaultOpen>
                <Dialog.Trigger render={<Button>Open inner</Button>} />
                <Dialog.Portal>
                  <Dialog.Backdrop />
                  <Dialog.Popup data-testid="dialog-nested-inner">
                    <Dialog.Header>
                      <Dialog.Title>Inner dialog</Dialog.Title>
                      <Dialog.Description>
                        Stacked on top; outer is scaled down.
                      </Dialog.Description>
                    </Dialog.Header>
                    <Dialog.Footer>
                      <Dialog.Close>Close inner</Dialog.Close>
                    </Dialog.Footer>
                  </Dialog.Popup>
                </Dialog.Portal>
              </Dialog>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  ),
};
