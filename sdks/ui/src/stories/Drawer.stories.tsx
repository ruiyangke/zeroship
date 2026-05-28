import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { useState } from "react";
import { DirectionProvider } from "@base-ui/react/direction-provider";
import { Button, Drawer, Field, Input } from "../components";

const meta: Meta<typeof Drawer> = {
  title: "Components/Drawer",
  component: Drawer,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Drawer>;

function getDocument(canvasElement: HTMLElement) {
  return within(canvasElement.ownerDocument.body);
}

async function openDrawer(canvasElement: HTMLElement, name: RegExp) {
  const canvas = within(canvasElement);
  await userEvent.click(canvas.getByRole("button", { name }));
  return getDocument(canvasElement);
}

async function waitForDrawerClosed(
  canvasElement: HTMLElement,
  name: RegExp,
) {
  const body = getDocument(canvasElement);
  await waitFor(() => {
    expect(body.queryByRole("dialog", { name })).not.toBeInTheDocument();
  });
}

/* ─── 1. Basic — right-side default ──────────────────────────────────── */
export const Basic: Story = {
  name: "Basic (end side)",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic drawer">
      <Drawer>
        <Drawer.Trigger
          render={<Button data-testid="drawer-trigger">Open drawer</Button>}
        />
        <Drawer.Portal>
          <Drawer.Backdrop />
          <Drawer.Content data-testid="drawer-basic-content">
            <Drawer.Header>
              <Drawer.Title>Notifications</Drawer.Title>
              <Drawer.Description>
                Side-anchored panel — default `side="end"` pins to the
                trailing edge (right in LTR, left in RTL).
              </Drawer.Description>
            </Drawer.Header>
            <Drawer.Body>
              The drawer reuses Base UI Dialog under the hood — it&rsquo;s a
              side-anchored Dialog with custom positioning. Logical
              sides flip naturally in RTL.
            </Drawer.Body>
            <Drawer.Footer>
              <Drawer.Close>Dismiss</Drawer.Close>
              <Button>Mark all read</Button>
            </Drawer.Footer>
          </Drawer.Content>
        </Drawer.Portal>
      </Drawer>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const body = await openDrawer(canvasElement, /open drawer/i);
    await expect(
      await body.findByRole("dialog", { name: /notifications/i }),
    ).toBeVisible();
    await userEvent.click(body.getByRole("button", { name: /dismiss/i }));
    await waitForDrawerClosed(canvasElement, /notifications/i);
  },
};

/* ─── 2. Left side (logical start) ───────────────────────────────────── */
export const LeftSide: Story = {
  name: "Start side (left in LTR)",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Start-side drawer">
      <Drawer>
        <Drawer.Trigger
          render={
            <Button data-testid="drawer-trigger">Open navigation</Button>
          }
        />
        <Drawer.Portal>
          <Drawer.Backdrop />
          <Drawer.Content side="start" data-testid="drawer-start-content">
            <Drawer.Header>
              <Drawer.Title>Navigation</Drawer.Title>
              <Drawer.Description>
                The canonical mobile-nav pattern — leading edge.
              </Drawer.Description>
            </Drawer.Header>
            <Drawer.Body>
              <nav aria-label="Primary">
                <ul>
                  <li>Dashboard</li>
                  <li>Projects</li>
                  <li>Settings</li>
                </ul>
              </nav>
            </Drawer.Body>
            <Drawer.Footer>
              <Drawer.Close>Close</Drawer.Close>
            </Drawer.Footer>
          </Drawer.Content>
        </Drawer.Portal>
      </Drawer>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const body = await openDrawer(canvasElement, /open navigation/i);
    await expect(
      await body.findByRole("dialog", { name: /navigation/i }),
    ).toBeVisible();
    await userEvent.keyboard("{Escape}");
    await waitForDrawerClosed(canvasElement, /navigation/i);
  },
};

/* ─── 3. Top side ────────────────────────────────────────────────────── */
export const Top: Story = {
  name: "Top",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Top drawer">
      <Drawer>
        <Drawer.Trigger
          render={
            <Button data-testid="drawer-trigger">Open from top</Button>
          }
        />
        <Drawer.Portal>
          <Drawer.Backdrop />
          <Drawer.Content side="top" data-testid="drawer-top-content">
            <Drawer.Header>
              <Drawer.Title>Quick search</Drawer.Title>
              <Drawer.Description>
                A top-anchored sheet for command palettes and search
                overlays.
              </Drawer.Description>
            </Drawer.Header>
            <Drawer.Body>
              <Field>
                <Field.Label>Search</Field.Label>
                <Input
                  placeholder="Type to search…"
                  data-testid="drawer-top-search"
                />
              </Field>
            </Drawer.Body>
            <Drawer.Footer>
              <Drawer.Close>Close</Drawer.Close>
            </Drawer.Footer>
          </Drawer.Content>
        </Drawer.Portal>
      </Drawer>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const body = await openDrawer(canvasElement, /open from top/i);
    await expect(
      await body.findByRole("dialog", { name: /quick search/i }),
    ).toBeVisible();
    const search = body.getByRole("textbox", { name: /search/i });
    await userEvent.type(search, "logs");
    await expect(search).toHaveValue("logs");
    await userEvent.keyboard("{Escape}");
    await waitForDrawerClosed(canvasElement, /quick search/i);
  },
};

/* ─── 4. Bottom side ─────────────────────────────────────────────────── */
export const Bottom: Story = {
  name: "Bottom",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Bottom drawer">
      <Drawer>
        <Drawer.Trigger
          render={
            <Button data-testid="drawer-trigger">Open from bottom</Button>
          }
        />
        <Drawer.Portal>
          <Drawer.Backdrop />
          <Drawer.Content side="bottom" data-testid="drawer-bottom-content">
            <Drawer.Header>
              <Drawer.Title>Share to&hellip;</Drawer.Title>
              <Drawer.Description>
                Bottom sheet — the iOS / Android action-sheet metaphor.
              </Drawer.Description>
            </Drawer.Header>
            <Drawer.Body>
              Pick a target to send this item.
            </Drawer.Body>
            <Drawer.Footer>
              <Drawer.Close>Cancel</Drawer.Close>
              <Button>Send</Button>
            </Drawer.Footer>
          </Drawer.Content>
        </Drawer.Portal>
      </Drawer>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const body = await openDrawer(canvasElement, /open from bottom/i);
    await expect(
      await body.findByRole("dialog", { name: /share to/i }),
    ).toBeVisible();
    await userEvent.click(body.getByRole("button", { name: /cancel/i }));
    await waitForDrawerClosed(canvasElement, /share to/i);
  },
};

/* ─── 5. Sizes ───────────────────────────────────────────────────────── */
export const Sizes: Story = {
  name: "Sizes",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All drawer sizes">
      {(["sm", "md", "lg", "full"] as const).map((size) => (
        <Drawer key={size}>
          <Drawer.Trigger
            render={
              <Button
                variant="tinted"
                data-testid={`drawer-trigger-${size}`}
              >
                Open {size}
              </Button>
            }
          />
          <Drawer.Portal>
            <Drawer.Backdrop />
            <Drawer.Content
              size={size}
              data-testid={`drawer-size-${size}`}
            >
              <Drawer.Header>
                <Drawer.Title>Size: {size}</Drawer.Title>
                <Drawer.Description>
                  Cross-axis size preset for the panel chrome.
                </Drawer.Description>
              </Drawer.Header>
              <Drawer.Body>
                <p>Content fills the preset cross-axis dimension.</p>
              </Drawer.Body>
              <Drawer.Footer>
                <Drawer.Close>Close</Drawer.Close>
              </Drawer.Footer>
            </Drawer.Content>
          </Drawer.Portal>
        </Drawer>
      ))}
    </div>
  ),
  play: async ({ canvasElement }) => {
    const body = getDocument(canvasElement);
    const canvas = within(canvasElement);

    for (const size of ["sm", "md", "lg", "full"]) {
      await userEvent.click(
        canvas.getByRole("button", { name: new RegExp(`open ${size}`, "i") }),
      );
      await expect(
        await body.findByRole("dialog", {
          name: new RegExp(`size: ${size}`, "i"),
        }),
      ).toBeVisible();
      await userEvent.keyboard("{Escape}");
      await waitForDrawerClosed(
        canvasElement,
        new RegExp(`size: ${size}`, "i"),
      );
    }
  },
};

/* ─── 5b. SizesVertical ──────────────────────────────────────────────── *
 *
 * Sizes (above) pins side="end" for all four sizes, so the cross-axis
 * `inline-size` rules get exercised but the vertical (top/bottom)
 * `block-size` rules at Drawer.css [data-side="top"|"bottom"] are
 * untested. SizesVertical covers `top × sm` and `bottom × lg` — the
 * minimum pair that touches the data-side="top" sm block and the
 * data-side="bottom" lg block independently.
 */
export const SizesVertical: Story = {
  name: "Sizes (vertical: top × sm, bottom × lg)",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Vertical drawer sizes"
    >
      <Drawer>
        <Drawer.Trigger
          render={
            <Button variant="tinted" data-testid="drawer-trigger-top-sm">
              Open top sm
            </Button>
          }
        />
        <Drawer.Portal>
          <Drawer.Backdrop />
          <Drawer.Content
            side="top"
            size="sm"
            data-testid="drawer-size-top-sm"
          >
            <Drawer.Header>
              <Drawer.Title>Top × sm</Drawer.Title>
              <Drawer.Description>
                Block-size preset for the top-anchored sheet.
              </Drawer.Description>
            </Drawer.Header>
            <Drawer.Body>
              <p>Block-axis size is constrained to the `sm` preset.</p>
            </Drawer.Body>
            <Drawer.Footer>
              <Drawer.Close>Close</Drawer.Close>
            </Drawer.Footer>
          </Drawer.Content>
        </Drawer.Portal>
      </Drawer>
      <Drawer>
        <Drawer.Trigger
          render={
            <Button variant="tinted" data-testid="drawer-trigger-bottom-lg">
              Open bottom lg
            </Button>
          }
        />
        <Drawer.Portal>
          <Drawer.Backdrop />
          <Drawer.Content
            side="bottom"
            size="lg"
            data-testid="drawer-size-bottom-lg"
          >
            <Drawer.Header>
              <Drawer.Title>Bottom × lg</Drawer.Title>
              <Drawer.Description>
                Block-size preset for the bottom-anchored sheet.
              </Drawer.Description>
            </Drawer.Header>
            <Drawer.Body>
              <p>Block-axis size is constrained to the `lg` preset.</p>
            </Drawer.Body>
            <Drawer.Footer>
              <Drawer.Close>Close</Drawer.Close>
            </Drawer.Footer>
          </Drawer.Content>
        </Drawer.Portal>
      </Drawer>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const body = getDocument(canvasElement);
    const canvas = within(canvasElement);

    await userEvent.click(
      canvas.getByRole("button", { name: /open top sm/i }),
    );
    await expect(
      await body.findByRole("dialog", { name: /top × sm/i }),
    ).toBeVisible();
    await userEvent.keyboard("{Escape}");
    await waitForDrawerClosed(canvasElement, /top × sm/i);

    await userEvent.click(
      canvas.getByRole("button", { name: /open bottom lg/i }),
    );
    await expect(
      await body.findByRole("dialog", { name: /bottom × lg/i }),
    ).toBeVisible();
    await userEvent.keyboard("{Escape}");
    await waitForDrawerClosed(canvasElement, /bottom × lg/i);
  },
};

/* ─── 6. Controlled ──────────────────────────────────────────────────── */
function ControlledStory() {
  const [open, setOpen] = useState(false);
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Controlled drawer"
    >
      <p data-testid="drawer-controlled-status">
        Status: {open ? "open" : "closed"}
      </p>
      <Button
        data-testid="drawer-trigger"
        onClick={() => setOpen(true)}
      >
        Open programmatically
      </Button>
      <Drawer open={open} onOpenChange={setOpen}>
        <Drawer.Portal>
          <Drawer.Backdrop />
          <Drawer.Content data-testid="drawer-controlled-content">
            <Drawer.Header>
              <Drawer.Title>Controlled drawer</Drawer.Title>
              <Drawer.Description>
                Open state lives in React state; closing flows through
                `onOpenChange`.
              </Drawer.Description>
            </Drawer.Header>
            <Drawer.Body>
              The Trigger is OUTSIDE the Drawer subtree — typical for
              imperative open flows (a row click that opens a detail
              panel, a hotkey, etc).
            </Drawer.Body>
            <Drawer.Footer>
              <Drawer.Close>Done</Drawer.Close>
            </Drawer.Footer>
          </Drawer.Content>
        </Drawer.Portal>
      </Drawer>
    </div>
  );
}
export const Controlled: Story = {
  name: "Controlled",
  render: () => <ControlledStory />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = getDocument(canvasElement);
    const status = canvas.getByText(/status: closed/i);

    await userEvent.click(
      canvas.getByRole("button", { name: /open programmatically/i }),
    );
    await expect(status).toHaveTextContent("Status: open");
    await expect(
      await body.findByRole("dialog", { name: /controlled drawer/i }),
    ).toBeVisible();

    await userEvent.click(body.getByRole("button", { name: /done/i }));
    await expect(status).toHaveTextContent("Status: closed");
    await waitForDrawerClosed(canvasElement, /controlled drawer/i);
  },
};

/* ─── 7. With form ───────────────────────────────────────────────────── */
export const WithForm: Story = {
  name: "With form",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Drawer containing a form"
    >
      <Drawer>
        <Drawer.Trigger
          render={<Button data-testid="drawer-trigger">Edit profile</Button>}
        />
        <Drawer.Portal>
          <Drawer.Backdrop />
          <Drawer.Content size="lg" data-testid="drawer-with-form">
            <Drawer.Header>
              <Drawer.Title>Edit profile</Drawer.Title>
              <Drawer.Description>
                Focus is trapped while the drawer is open.
              </Drawer.Description>
            </Drawer.Header>
            <Drawer.Body>
              <Field>
                <Field.Label>Display name</Field.Label>
                <Input
                  defaultValue="Ada Lovelace"
                  data-testid="drawer-form-name"
                />
              </Field>
              <Field>
                <Field.Label>Email</Field.Label>
                <Input type="email" defaultValue="ada@example.com" />
              </Field>
              <Field>
                <Field.Label>Bio</Field.Label>
                <Input defaultValue="" />
              </Field>
            </Drawer.Body>
            <Drawer.Footer>
              <Drawer.Close>Cancel</Drawer.Close>
              <Button>Save</Button>
            </Drawer.Footer>
          </Drawer.Content>
        </Drawer.Portal>
      </Drawer>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const body = await openDrawer(canvasElement, /edit profile/i);
    await expect(
      await body.findByRole("dialog", { name: /edit profile/i }),
    ).toBeVisible();

    const displayName = body.getByRole("textbox", {
      name: /display name/i,
    });
    await expect(displayName).toHaveValue("Ada Lovelace");
    await userEvent.clear(displayName);
    await userEvent.type(displayName, "Grace Hopper");
    await expect(displayName).toHaveValue("Grace Hopper");

    await userEvent.click(body.getByRole("button", { name: /cancel/i }));
    await waitForDrawerClosed(canvasElement, /edit profile/i);
  },
};

/* ─── 8. With long content (scroll) ──────────────────────────────────── */
export const WithLongContent: Story = {
  name: "With long content (scrolls)",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Drawer with scrolling body"
    >
      <Drawer>
        <Drawer.Trigger
          render={<Button data-testid="drawer-trigger">Open terms</Button>}
        />
        <Drawer.Portal>
          <Drawer.Backdrop />
          <Drawer.Content data-testid="drawer-long-content">
            <Drawer.Header>
              <Drawer.Title>Terms of service</Drawer.Title>
              <Drawer.Description>
                Body scrolls independently while Header + Footer stay
                pinned.
              </Drawer.Description>
            </Drawer.Header>
            <Drawer.Body>
              {Array.from({ length: 24 }, (_, i) => (
                <p key={i}>
                  Section {i + 1}. The body region uses `overflow-y: auto`
                  with a `min-block-size: 0` so it can scroll
                  independently of the Header / Footer pinned chrome.
                  This block exists to make the scroll behavior
                  visually obvious in the captured screenshots.
                </p>
              ))}
            </Drawer.Body>
            <Drawer.Footer>
              <Drawer.Close>I&rsquo;ve read it</Drawer.Close>
            </Drawer.Footer>
          </Drawer.Content>
        </Drawer.Portal>
      </Drawer>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const body = await openDrawer(canvasElement, /open terms/i);
    await expect(
      await body.findByRole("dialog", { name: /terms of service/i }),
    ).toBeVisible();
    await userEvent.click(body.getByRole("button", { name: /i.ve read it/i }));
    await waitForDrawerClosed(canvasElement, /terms of service/i);
  },
};

/* ─── 9. Nested ──────────────────────────────────────────────────────────
 *
 * The brief calls "Nested" out as "drawer inside main content" — we
 * render a card-shell that contains content alongside a Drawer, so the
 * captured PNG shows the drawer co-existing with the main column.
 */
export const Nested: Story = {
  name: "Nested (drawer alongside main content)",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Drawer inside main content"
    >
      <main
        style={{
          padding: "var(--zs-space-6)",
          minBlockSize: "60dvh",
          background: "var(--zs-fill-secondary)",
          color: "var(--zs-label)",
          display: "flex",
          flexDirection: "column",
          gap: "var(--zs-space-4)",
        }}
      >
        <h2 style={{ margin: 0 }}>Main content</h2>
        <p>
          The drawer renders into a Portal — it overlays the main content
          while leaving the document layout untouched.
        </p>
        <Drawer>
          <Drawer.Trigger
            render={
              <Button data-testid="drawer-trigger">
                Open detail drawer
              </Button>
            }
          />
          <Drawer.Portal>
            <Drawer.Backdrop />
            <Drawer.Content data-testid="drawer-nested-content">
              <Drawer.Header>
                <Drawer.Title>Detail</Drawer.Title>
                <Drawer.Description>
                  Drawers are portaled to the document body so they
                  overlay any positioned ancestor.
                </Drawer.Description>
              </Drawer.Header>
              <Drawer.Body>
                Inspect the selected row without leaving the main
                content.
              </Drawer.Body>
              <Drawer.Footer>
                <Drawer.Close>Close</Drawer.Close>
              </Drawer.Footer>
            </Drawer.Content>
          </Drawer.Portal>
        </Drawer>
      </main>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const body = await openDrawer(canvasElement, /open detail drawer/i);
    await expect(
      await body.findByRole("dialog", { name: /detail/i }),
    ).toBeVisible();
    await userEvent.keyboard("{Escape}");
    await waitForDrawerClosed(canvasElement, /detail/i);
  },
};

/* ─── 10. RTL ────────────────────────────────────────────────────────────
 *
 * Hebrew / Arabic right-to-left. `side="start"` flips to the right
 * edge automatically via `inset-inline-start`. The translate is
 * physical-axis, but our CSS mirrors it under `[dir="rtl"]` so the
 * closed-state drawer flies off the correct edge.
 */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew / Arabic right-to-left. The Drawer side=\"start\" " +
          "pins to the right edge automatically (logical inset). The " +
          "translate is physical, but a [dir=\"rtl\"] mirror inverts " +
          "the X component so the closed-state panel flies off the " +
          "correct edge.",
      },
    },
  },
  render: () => (
    // DirectionProvider tells Base UI to propagate `dir="rtl"`
    // through its component tree. Because Portal renders into
    // document.body (which is LTR), we ALSO stamp `dir="rtl"`
    // directly on Backdrop + Content so the portaled subtree carries
    // the direction — `inset-inline-start` resolves against the
    // nearest `dir` ancestor, and our RTL transform mirror keys off
    // the same attribute.
    <DirectionProvider direction="rtl">
      <div
        dir="rtl"
        className="zs-story-row"
        role="group"
        aria-label="Drawer RTL"
      >
        <Drawer>
          <Drawer.Trigger
            render={<Button data-testid="drawer-trigger">פתח מגירה</Button>}
          />
          <Drawer.Portal>
            <Drawer.Backdrop dir="rtl" />
            <Drawer.Content
              dir="rtl"
              side="start"
              data-testid="drawer-rtl-content"
            >
              <Drawer.Header>
                <Drawer.Title>ניווט</Drawer.Title>
                <Drawer.Description>
                  המגירה מעוגנת בקצה ההתחלה לפי כיוון הטקסט.
                </Drawer.Description>
              </Drawer.Header>
              <Drawer.Body>
                The start side flips to the RIGHT edge under RTL via
                `inset-inline-start`; the slide-in translate mirrors so
                the panel always enters from the anchored edge.
              </Drawer.Body>
              <Drawer.Footer>
                <Drawer.Close>סגור</Drawer.Close>
                <Drawer.Close variant="filled">אישור</Drawer.Close>
              </Drawer.Footer>
            </Drawer.Content>
          </Drawer.Portal>
        </Drawer>
      </div>
    </DirectionProvider>
  ),
  play: async ({ canvasElement }) => {
    const body = await openDrawer(canvasElement, /פתח מגירה/i);
    await expect(
      await body.findByRole("dialog", { name: /ניווט/i }),
    ).toBeVisible();
    await userEvent.click(body.getByRole("button", { name: /אישור/i }));
    await waitForDrawerClosed(canvasElement, /ניווט/i);
  },
};

/* ─── 11. Close asChild ─────────────────────────────────────────────── *
 *
 * Proves the full asChild contract that aria-wiring #87 measures:
 *   - the wrapper's `...rest` (className, data-*, aria-*, style)
 *     reaches the child via Slot;
 *   - the wrapper's onClick AND the child's onClick BOTH run, in
 *     order: child first → wrapper → Base UI close;
 *   - a status side-effect mutation proves both handlers fired before
 *     the dialog tore down.
 */
function CloseAsChildStory() {
  const [clicked, setClicked] = useState<string>("not-clicked");
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Drawer close asChild"
    >
      <p
        role="status"
        aria-label="Drawer close asChild status"
        data-testid="drawer-close-aschild-status"
      >
        Status: {clicked}
      </p>
      <Drawer>
        <Drawer.Trigger
          render={<Button data-testid="drawer-trigger">Open</Button>}
        />
        <Drawer.Portal>
          <Drawer.Backdrop />
          <Drawer.Content data-testid="drawer-close-aschild-content">
            <Drawer.Header>
              <Drawer.Title>Custom close target</Drawer.Title>
              <Drawer.Description>
                The asChild Slot routes className, style, refs, AND
                onClick composition through the shared `_slot.ts`
                helper. Wrapper `...rest` + child onClick BOTH reach
                the child.
              </Drawer.Description>
            </Drawer.Header>
            <Drawer.Footer>
              <Drawer.Close
                asChild
                // Wrapper `...rest` props that MUST forward to the
                // child via Slot (review-fix item 4): a custom
                // className suffix and a data-* hook.
                className="zs-drawer-close-aschild-extra"
                data-side-effect="wrapper-rest-forwarded"
                // Wrapper-level onClick — composes with the child's
                // own onClick below and with Base UI's close handler.
                onClick={() => setClicked((s) =>
                  s === "child-onclick-ran" ? "both-handlers-ran" : "wrapper-only"
                )}
              >
                <button
                  type="button"
                  className="zs-button zs-button--gray zs-button--medium"
                  data-testid="drawer-close-aschild-target"
                  onClick={() => setClicked("child-onclick-ran")}
                >
                  Done
                </button>
              </Drawer.Close>
            </Drawer.Footer>
          </Drawer.Content>
        </Drawer.Portal>
      </Drawer>
    </div>
  );
}
export const CloseAsChild: Story = {
  name: "Close — asChild (Slot)",
  render: () => <CloseAsChildStory />,
  play: async ({ canvasElement }) => {
    const body = await openDrawer(canvasElement, /^open$/i);
    await expect(
      await body.findByRole("dialog", { name: /custom close target/i }),
    ).toBeVisible();

    const done = body.getByRole("button", { name: /done/i });
    await expect(done.tagName).toBe("BUTTON");
    // Wrapper rest props forwarded by Slot.
    await expect(done).toHaveAttribute(
      "data-side-effect",
      "wrapper-rest-forwarded",
    );
    await expect(done).toHaveClass("zs-drawer-close-aschild-extra");
    await userEvent.click(done);
    await waitForDrawerClosed(canvasElement, /custom close target/i);
    // Both child + wrapper onClick fired and composed in order.
    const canvas = within(canvasElement);
    const status = canvas.getByRole("status", {
      name: /drawer close aschild status/i,
    });
    await expect(status).toHaveTextContent("Status: both-handlers-ran");
  },
};
