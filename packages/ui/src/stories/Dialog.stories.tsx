import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { useMemo, useRef, useState } from "react";
import { Button, createDialogHandle, Dialog, Field, Input } from "../components";

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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("button", { name: /open dialog/i });

    await userEvent.click(trigger);
    const dialog = await page.findByRole("dialog", { name: /are you sure/i });
    await expect(dialog).toBeVisible();

    await userEvent.click(page.getByRole("button", { name: /^close$/i }));
    // Base UI returns focus to the trigger AFTER the close transition
    // commits and the popup unmounts — that focus move is async, so a
    // bare synchronous assertion races it (and flakes nondeterministically
    // under headless). waitFor polls the SAME assertion until the
    // focus-return lands. Mirrors the InitialFocus story's deflake.
    await waitFor(() => expect(trigger).toHaveFocus());
  },
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
  // Regression guard (#6): top placement is `top`-anchored — its steady
  // transform has NO -50% vertical translate (only --zs-nested-offset,
  // which is 0 for a non-nested dialog). The reduced-motion enter/leave
  // override now MIRRORS this (it previously re-added a -50% it never had,
  // causing a one-frame vertical jump). After open + transition settle,
  // assert the rendered vertical translate is ~0, not -half the popup
  // height (which a -50% translate would produce).
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await userEvent.click(canvas.getByTestId("dialog-trigger"));

    const popup = await waitFor(() => {
      const el = document.querySelector<HTMLElement>(
        '[data-testid="dialog-placement-top"]',
      );
      if (!el) throw new Error("popup not mounted");
      return el;
    });

    // Let the enter transition settle so we read the steady transform, not
    // the data-starting-style frame.
    await waitFor(async () => {
      await expect(popup).not.toHaveAttribute("data-starting-style");
    });

    const matrix = new DOMMatrixReadOnly(getComputedStyle(popup).transform);
    // matrix.f is the resolved vertical translate in px. Top placement is
    // top-anchored: no -50% of the popup height. A -50% translate would put
    // |f| ≈ height/2 (tens of px). Steady top translate is just the nested
    // offset (0 here), so |f| must be small.
    await expect(Math.abs(matrix.f)).toBeLessThan(2);
    // Sanity: the popup actually has a meaningful height, so a -50% would
    // have been large and this assertion is non-trivial.
    await expect(popup.getBoundingClientRect().height).toBeGreaterThan(20);
  },
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
            <Dialog.Body> The dismissible=false setting disables BOTH outside-click AND escape-key. </Dialog.Body>
            <Dialog.Footer>
              <Dialog.Close variant="filled">Acknowledge</Dialog.Close>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", {
      name: /open required action/i,
    }));
    const dialog = await page.findByRole("dialog", {
      name: /required action/i,
    });

    await userEvent.keyboard("{Escape}");
    await expect(dialog).toBeVisible();

    await userEvent.click(page.getByRole("button", { name: /acknowledge/i }));
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", { name: /sign in/i }));
    await page.findByRole("dialog", { name: /sign in/i });
    // Base UI moves initial focus AFTER the open transition commits, so
    // a bare synchronous assertion races the autofocus. waitFor polls
    // until the username field actually holds focus — same assertion,
    // just deflaked.
    await waitFor(() =>
      expect(
        page.getByRole("textbox", { name: /username/i }),
      ).toHaveFocus(),
    );
  },
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
      <p
        role="status"
        aria-label="Dialog save status"
        data-testid="dialog-close-onclick-status"
      >
        Status: {saved}
      </p>
      <Dialog>
        <Dialog.Trigger
          render={<Button data-testid="dialog-trigger">Open save form</Button>}
        />
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup data-testid="dialog-close-onclick-popup">
            <Dialog.Header>
              <Dialog.Title>Save changes</Dialog.Title>
              <Dialog.Description> Pressing Save fires the caller onClick AND closes the dialog. Both must happen — clobbering one is a bug Dialog.Close now guards against. </Dialog.Description>
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);
    const status = canvas.getByRole("status", { name: /dialog save status/i });

    await userEvent.click(canvas.getByRole("button", {
      name: /open save form/i,
    }));
    await page.findByRole("dialog", { name: /save changes/i });
    await userEvent.click(page.getByRole("button", { name: /^save$/i }));
    await expect(status).toHaveTextContent("Status: saved");
  },
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
              <Dialog.Description> The asChild Slot routes className, style, refs, and onClick composition through the shared `_slot.ts` helper. </Dialog.Description>
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", { name: /^open$/i }));
    await page.findByRole("dialog", { name: /custom close target/i });
    const done = page.getByRole("button", { name: /done/i });

    await expect(done.tagName).toBe("BUTTON");
    await userEvent.click(done);
  },
};

/* ─── 8b-bis. Close asChild — wrapper props forward through Slot ────── */
/*
 * Regression for the wave6 fix: previously the `asChild` branch
 * rendered `<Slot {...closeProps} … >` and dropped `...rest`, so any
 * `className`, `data-*`, `aria-*`, `style`, or wrapper `onClick`
 * placed on `<Dialog.Close asChild>` silently disappeared. This story
 * asserts every observable prop reaches the child AND that the
 * wrapper's onClick composes with the child's onClick AND Base UI's
 * close handler.
 */
function CloseAsChildWrapperPropsStory() {
  const [order, setOrder] = useState<string[]>([]);
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Dialog close asChild wrapper props"
    >
      <p role="status" aria-label="Dialog close asChild click order">
        Order: {order.join(",") || "idle"}
      </p>
      <Dialog>
        <Dialog.Trigger render={<Button>Open wrapper-props close</Button>} />
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup>
            <Dialog.Header>
              <Dialog.Title>Wrapper props on Close.asChild</Dialog.Title>
              <Dialog.Description>
                Every prop on `Dialog.Close asChild` must reach the child.
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Footer>
              <Dialog.Close
                asChild
                className="zs-wrapper-cls"
                data-wrapper-flag="present"
                aria-label="Wrapper aria label"
                style={{ outlineStyle: "dotted" }}
                onClick={() => setOrder((prev) => [...prev, "wrapper"])}
              >
                <button
                  type="button"
                  className="zs-child-cls"
                  data-child-flag="present"
                  data-testid="dialog-close-aschild-wrapped"
                  onClick={() => setOrder((prev) => [...prev, "child"])}
                >
                  Done
                </button>
              </Dialog.Close>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  );
}
export const CloseAsChildWrapperProps: Story = {
  name: "Close — asChild wrapper-props forward",
  render: () => <CloseAsChildWrapperPropsStory />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);
    const status = canvas.getByRole("status", {
      name: /dialog close aschild click order/i,
    });

    await userEvent.click(
      canvas.getByRole("button", { name: /open wrapper-props close/i }),
    );
    const dialog = await page.findByRole("dialog", {
      name: /wrapper props on close\.aschild/i,
    });

    // Wrapper's aria-label wins over the child text (the prior
    // implementation dropped aria-label, so getByRole here would have
    // failed with the wrapper name).
    const target = page.getByRole("button", { name: /wrapper aria label/i });
    await expect(target.tagName).toBe("BUTTON");
    await expect(target).toHaveAttribute("data-testid", "dialog-close-aschild-wrapped");
    // Wrapper's data-* and child's data-* both land on the node.
    await expect(target).toHaveAttribute("data-wrapper-flag", "present");
    await expect(target).toHaveAttribute("data-child-flag", "present");
    // Wrapper's className concatenates with the child's className.
    await expect(target).toHaveClass("zs-child-cls");
    await expect(target).toHaveClass("zs-wrapper-cls");
    // Wrapper's style merges onto the child.
    await expect(target).toHaveStyle({ outlineStyle: "dotted" });

    await userEvent.click(target);
    // Child onClick fires first (Slot mergeProps order), then wrapper.
    await expect(status).toHaveTextContent("Order: child,wrapper");
    // Base UI's close handler still ran — the dialog is closing.
    // `data-closed=""` is the Base UI exit-transition flag; presence
    // proves Base UI's close handler fired (pre-fix, dropping `...rest`
    // also dropped the `onClick` we built composedOnClick from when the
    // wrapper-onClick path was the only handler — so Base UI's
    // close-on-press never ran).
    await expect(dialog).toHaveAttribute("data-closed", "");
  },
};

/* ─── 8b-ter. createDialogHandle — payload render-function child ───── */
/*
 * Regression for the wave6 type-narrowing fix: DialogProps.children
 * was `ReactNode`, which compiled away the Base UI payload
 * render-function branch (`(payload) => ReactElement`). With the
 * narrowed type, `<Dialog handle={…}>{(payload) => …}</Dialog>` was a
 * type error AND a runtime no-op (React tried to render a function as
 * a child).
 *
 * Uses Base UI's canonical handle flow: `<Dialog.Trigger handle={h}
 * payload={…}>` opens the dialog and the `(payload) => …` child of
 * `<Dialog>` renders the popup. The Title is rendered FROM the
 * payload, so a regression on either the type narrowing OR the
 * runtime render-function dispatch fails this assertion.
 */
type ConfirmPayload = { itemName: string };

function CreateDialogHandlePayloadStory() {
  // Memoize so re-renders don't break Base UI's handle identity check.
  const handle = useMemo(() => createDialogHandle<ConfirmPayload>(), []);
  const [confirmed, setConfirmed] = useState<string>("idle");
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Dialog createHandle payload"
    >
      <p role="status" aria-label="Dialog handle confirm status">
        Status: {confirmed}
      </p>
      <Dialog.Trigger
        handle={handle}
        payload={{ itemName: "Project Atlas" }}
        render={
          <Button data-testid="dialog-handle-trigger">
            Delete Project Atlas
          </Button>
        }
      />
      <Dialog handle={handle}>
        {/* payload is typed `{}` here: DialogProps extends the non-generic
            BaseRootProps (see the "half-broken createDialogHandle" note in
            Dialog.tsx), so the handle's type param doesn't flow to the
            render-fn. Cast until Dialog.Root is made generic over payload. */}
        {({ payload: rawPayload }) => {
          const payload = rawPayload as ConfirmPayload | undefined;
          return (
          <Dialog.Portal>
            <Dialog.Backdrop />
            <Dialog.Popup data-testid="dialog-handle-popup">
              <Dialog.Header>
                <Dialog.Title>Delete {payload?.itemName}?</Dialog.Title>
                <Dialog.Description>
                  This action cannot be undone.
                </Dialog.Description>
              </Dialog.Header>
              <Dialog.Footer>
                <Dialog.Close>Cancel</Dialog.Close>
                <Dialog.Close
                  variant="filled"
                  intent="destructive"
                  onClick={() =>
                    setConfirmed(`deleted ${payload?.itemName ?? ""}`.trim())
                  }
                >
                  Delete
                </Dialog.Close>
              </Dialog.Footer>
            </Dialog.Popup>
          </Dialog.Portal>
          );
        }}
      </Dialog>
    </div>
  );
}
export const CreateDialogHandlePayload: Story = {
  name: "CreateHandle — payload render-function child",
  render: () => <CreateDialogHandlePayloadStory />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);
    const status = canvas.getByRole("status", {
      name: /dialog handle confirm status/i,
    });

    await userEvent.click(
      canvas.getByRole("button", { name: /delete project atlas/i }),
    );
    // Title is rendered from the payload — proves the render function
    // received the typed payload and React rendered its return value
    // as children (not as a literal function).
    await page.findByRole("dialog", { name: /delete project atlas\?/i });
    await userEvent.click(page.getByRole("button", { name: /^delete$/i }));
    await expect(status).toHaveTextContent("Status: deleted Project Atlas");
  },
};

function ClosePreventDefaultStory() {
  const [status, setStatus] = useState("idle");
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Dialog close preventDefault"
    >
      <p role="status" aria-label="Dialog close guard status">
        Status: {status}
      </p>
      <Dialog>
        <Dialog.Trigger render={<Button>Open guarded close</Button>} />
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup>
            <Dialog.Header>
              <Dialog.Title>Guarded close</Dialog.Title>
              <Dialog.Description>
                The caller prevents the first close attempt.
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Footer>
              <Dialog.Close
                onClick={(event) => {
                  event.preventDefault();
                  setStatus("prevented");
                }}
              >
                Stay open
              </Dialog.Close>
              <Dialog.Close variant="filled">Close now</Dialog.Close>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  );
}
export const ClosePreventDefault: Story = {
  name: "Close preventDefault (play)",
  render: () => <ClosePreventDefaultStory />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);
    const status = canvas.getByRole("status", {
      name: /dialog close guard status/i,
    });

    await userEvent.click(canvas.getByRole("button", {
      name: /open guarded close/i,
    }));
    const dialog = await page.findByRole("dialog", { name: /guarded close/i });

    await userEvent.click(page.getByRole("button", { name: /stay open/i }));
    await expect(status).toHaveTextContent("Status: prevented");
    await expect(dialog).toBeVisible();

    await userEvent.click(page.getByRole("button", { name: /close now/i }));
    await waitFor(() =>
      expect(
        page.queryByRole("dialog", { name: /guarded close/i }),
      ).not.toBeInTheDocument(),
    );
  },
};

export const CloseAsChildLink: Story = {
  name: "Close — asChild link (play)",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Dialog close asChild link"
    >
      <Dialog>
        <Dialog.Trigger render={<Button>Open link close</Button>} />
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup>
            <Dialog.Header>
              <Dialog.Title>Link close target</Dialog.Title>
              <Dialog.Description>
                Dialog.Close can route Base UI close behavior into a
                non-button element. The element keeps its `&lt;a href&gt;`
                tag, but Base UI assigns `role="button"` because a close
                control IS a button semantically.
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Footer>
              <Dialog.Close asChild>
                <a href="#dialog-link-close">Done as link</a>
              </Dialog.Close>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", {
      name: /open link close/i,
    }));
    await page.findByRole("dialog", { name: /link close target/i });
    // Base UI's Close primitive runs the asChild target through useButton
    // with nativeButton={false}; useButton unconditionally assigns
    // role="button" to non-native-button render targets (a close control
    // is a button, not a link). So the accessible role is "button" even
    // though the underlying element stays an <a href>. We query by the
    // real role and assert the tag/href to prove asChild routed onto the
    // anchor element.
    const link = page.getByRole("button", { name: /done as link/i });

    await expect(link.tagName).toBe("A");
    await expect(link).toHaveAttribute("href", "#dialog-link-close");
    await userEvent.click(link);
    await waitFor(() =>
      expect(
        page.queryByRole("dialog", { name: /link close target/i }),
      ).not.toBeInTheDocument(),
    );
  },
};

/* ─── 8c. RTL — popup stays centred in right-to-left ─────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Shows this component behavior with realistic content and keeps the edge case easy to inspect.",
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
              <Dialog.Description> Footer wraps when many or long-labelled action buttons exceed the popup width. </Dialog.Description>
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
          "Shows this component behavior with realistic content and keeps the edge case easy to inspect.",
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

export const FullTopWarns: Story = {
  name: "Full size plus top placement (warn)",
  parameters: {
    docs: {
      description: {
        story:
          "Negative test: size=\"full\" owns placement, so placement=\"top\" " +
          "fires a dev warning and the full-screen sizing wins.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Dialog full plus top placement"
    >
      <Dialog>
        <Dialog.Trigger render={<Button>Open full top</Button>} />
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup size="full" placement="top">
            <Dialog.Header>
              <Dialog.Title>Full takeover</Dialog.Title>
              <Dialog.Description>
                Full size ignores top placement by design.
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Body>Full-viewport takeover content.</Dialog.Body>
            <Dialog.Footer>
              <Dialog.Close>Close</Dialog.Close>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", {
      name: /open full top/i,
    }));
    const dialog = await page.findByRole("dialog", { name: /full takeover/i });

    await expect(dialog).toHaveAttribute("data-size", "full");
    await expect(dialog).toHaveAttribute("data-placement", "top");
    await userEvent.click(page.getAllByRole("button", { name: /^close$/i })[0]);
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const page = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", { name: /open outer/i }));
    await page.findByRole("dialog", { name: /outer dialog/i });

    await userEvent.click(page.getByRole("button", { name: /open inner/i }));
    await expect(await page.findByRole("dialog", {
      name: /inner dialog/i,
    })).toBeVisible();

    await userEvent.click(page.getByRole("button", { name: /close inner/i }));
  },
};
