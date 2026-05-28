import type { Meta, StoryObj } from "@storybook/react";
import { useRef, useState } from "react";
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
      <Dialog>
        <Dialog.Trigger
          render={<Button data-testid="dialog-trigger">Open dialog</Button>}
        />
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
          <Dialog.Trigger
            render={
              <Button
                variant="tinted"
                data-testid={`dialog-trigger-${size}`}
              >
                Open {size}
              </Button>
            }
          />
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
      <Dialog>
        <Dialog.Trigger
          render={<Button data-testid="dialog-trigger">Open top sheet</Button>}
        />
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
      {(["scrim", "material", "invisible"] as const).map((tint) => (
        <Dialog key={tint} modal={tint === "invisible" ? false : true}>
          <Dialog.Trigger
            render={
              <Button
                variant="tinted"
                data-testid={`dialog-trigger-${tint}`}
              >
                Open tint: {tint}
              </Button>
            }
          />
          <Dialog.Portal>
            <Dialog.Backdrop tint={tint} />
            <Dialog.Popup data-testid={`dialog-tint-${tint}`}>
              <Dialog.Header>
                <Dialog.Title>Backdrop tint = {tint}</Dialog.Title>
                <Dialog.Description>
                  scrim is the default; material is glass; invisible is
                  a transparent click-blocker (paired with modal=false
                  to avoid the invisible-trap dev-warn).
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
      <Dialog>
        <Dialog.Trigger
          render={<Button data-testid="dialog-trigger">Edit profile</Button>}
        />
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
      <Dialog dismissible={false}>
        <Dialog.Trigger
          render={<Button data-testid="dialog-trigger">Open required action</Button>}
        />
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
      <Dialog>
        <Dialog.Trigger
          render={<Button data-testid="dialog-trigger">Sign in</Button>}
        />
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

/* ─── 8a. Close with caller onClick — composes side-effect + close ──── */
function CloseWithSaveOnClickStory() {
  // Status lives OUTSIDE the dialog so it survives the close animation
  // — letting the aria-wiring assertion verify the side-effect ran
  // AFTER the dialog closed.
  const [saved, setSaved] = useState<string>("not-saved");
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Dialog close with caller onClick"
    >
      <p data-testid="dialog-close-onclick-status">Status: {saved}</p>
      <Dialog>
        <Dialog.Trigger
          render={<Button data-testid="dialog-trigger">Open save form</Button>}
        />
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup data-testid="dialog-close-onclick-popup">
            <Dialog.Header>
              <Dialog.Title>Save changes</Dialog.Title>
              <Dialog.Description>
                Pressing Save fires the caller onClick AND closes the
                dialog. Both must happen — clobbering one is a bug
                Dialog.Close now guards against (review-fix item 1).
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Body>
              <p>
                After pressing Save, the outer status line flips to
                &ldquo;saved&rdquo; AND this popup closes.
              </p>
            </Dialog.Body>
            <Dialog.Footer>
              <Dialog.Close>Cancel</Dialog.Close>
              <Dialog.Close
                variant="filled"
                data-testid="dialog-close-save"
                onClick={() => setSaved("saved")}
              >
                Save
              </Dialog.Close>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  );
}
export const CloseWithSaveOnClick: Story = {
  name: "Close — onClick composes with close",
  render: () => <CloseWithSaveOnClickStory />,
};

/* ─── 8b. Close asChild — Slot routing ───────────────────────────────── */
export const CloseAsChild: Story = {
  name: "Close — asChild (Slot)",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Dialog close asChild"
    >
      <Dialog>
        <Dialog.Trigger
          render={<Button data-testid="dialog-trigger">Open</Button>}
        />
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup data-testid="dialog-close-aschild-popup">
            <Dialog.Header>
              <Dialog.Title>Custom close target</Dialog.Title>
              <Dialog.Description>
                The asChild Slot routes className, style, refs, and
                onClick composition through the shared `_slot.ts`
                helper (review-fix item 3).
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Footer>
              <Dialog.Close asChild>
                <button
                  type="button"
                  className="zs-button zs-button--gray zs-button--medium"
                  data-testid="dialog-close-aschild-target"
                >
                  Done
                </button>
              </Dialog.Close>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  ),
};

/* ─── 8c. RTL — popup stays centred in right-to-left ─────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew / Arabic right-to-left. The Popup centers via " +
          "PHYSICAL `top` + `left` paired with `translate(-50%, -50%)` " +
          "(review-fix item 4) — logical inset-inline-start flips in " +
          "RTL and shifts the popup off-center, so we anchor in " +
          "physical coordinates while the inner content uses logical " +
          "properties for natural flip.",
      },
    },
  },
  render: () => (
    <div dir="rtl" className="zs-story-row" role="group" aria-label="Dialog RTL">
      <Dialog>
        <Dialog.Trigger
          render={<Button data-testid="dialog-trigger">פתח דיאלוג</Button>}
        />
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup data-testid="dialog-rtl-popup">
            <Dialog.Header>
              <Dialog.Title>שינויים נשמרו</Dialog.Title>
              <Dialog.Description>
                הדיאלוג ממורכז גם בכיוון מימין לשמאל.
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Body>
              The popup must remain centred horizontally — physical
              `left: 50%` + `translateX(-50%)` keeps the centring math
              direction-invariant.
            </Dialog.Body>
            <Dialog.Footer>
              <Dialog.Close>ביטול</Dialog.Close>
              <Dialog.Close variant="filled">אישור</Dialog.Close>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  ),
};

/* ─── 8d. Long footer labels — wraps gracefully ──────────────────────── */
export const LongFooterLabels: Story = {
  name: "Long footer labels (wrap)",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Dialog long footer labels"
    >
      <Dialog>
        <Dialog.Trigger
          render={<Button data-testid="dialog-trigger">Open verbose</Button>}
        />
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup
            size="sm"
            data-testid="dialog-long-footer-popup"
          >
            <Dialog.Header>
              <Dialog.Title>Review your reservation</Dialog.Title>
              <Dialog.Description>
                Footer wraps when many or long-labelled action buttons
                exceed the popup width (review-fix item 8).
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Footer>
              <Dialog.Close>Dismiss</Dialog.Close>
              <Dialog.Close>Save for later</Dialog.Close>
              <Dialog.Close variant="filled">
                Confirm reservation now
              </Dialog.Close>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  ),
};

/* ─── 8e. Unlabeled Popup — dev-warn (negative test) ─────────────────── */
export const UnlabeledPopupWarns: Story = {
  name: "Unlabeled Popup (dev warns)",
  parameters: {
    docs: {
      description: {
        story:
          "Popup without Dialog.Title and without aria-label fires a " +
          "dev console.warn (review-fix item 7). This story exists so " +
          "we can verify the warning behavior end-to-end; the missing " +
          "label is INTENTIONAL.",
      },
    },
    // axe will rightly flag this story; disable the a11y check on it.
    a11y: { disable: true },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Unlabeled popup dev warn"
    >
      <Dialog>
        <Dialog.Trigger
          render={
            <Button data-testid="dialog-trigger">Open unlabeled</Button>
          }
        />
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup data-testid="dialog-unlabeled-popup">
            <Dialog.Body>
              No Title, no aria-label — Dialog.Popup logs a console
              warning in dev so this gap doesn't ship silently.
            </Dialog.Body>
            <Dialog.Footer>
              <Dialog.Close>Close</Dialog.Close>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  ),
};

/* ─── 8f. Non-modal — outside interaction allowed ────────────────────── */
export const NonModal: Story = {
  name: "Non-modal",
  parameters: {
    docs: {
      description: {
        story:
          "modal={false} keeps the dialog open but does NOT trap focus " +
          "or lock scroll. Outside interactions remain active — useful " +
          "for floating tool palettes alongside primary content.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Non-modal dialog"
    >
      <Dialog modal={false}>
        <Dialog.Trigger
          render={
            <Button data-testid="dialog-trigger">Open non-modal</Button>
          }
        />
        <Dialog.Portal>
          <Dialog.Backdrop tint="invisible" />
          <Dialog.Popup data-testid="dialog-non-modal-popup">
            <Dialog.Header>
              <Dialog.Title>Floating tool</Dialog.Title>
              <Dialog.Description>
                Background content remains scrollable and clickable.
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Footer>
              <Dialog.Close>Close</Dialog.Close>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  ),
};

/* ─── 9. Nested ──────────────────────────────────────────────────────── */
export const Nested: Story = {
  name: "Nested",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Nested dialogs">
      <Dialog>
        <Dialog.Trigger
          render={<Button data-testid="dialog-trigger-outer">Open outer</Button>}
        />
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
              <Dialog>
                <Dialog.Trigger
                  render={<Button data-testid="dialog-trigger-inner">Open inner</Button>}
                />
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
