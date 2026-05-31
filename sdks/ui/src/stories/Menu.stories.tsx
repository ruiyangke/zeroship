import { useCallback, useRef, useState } from "react";
import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { DirectionProvider } from "@base-ui/react/direction-provider";
import { Button, Menu } from "../components";

const meta: Meta<typeof Menu> = {
  title: "Components/Menu",
  component: Menu,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Menu>;

/* ─── 1. BasicItems ─────────────────────────────────────────────────── */
export const BasicItems: Story = {
  name: "Basic items",
  parameters: {
    docs: {
      description: {
        story:
          "Click the trigger to open a popup of plain Items. ArrowDown / " +
          "ArrowUp move the highlight; Enter activates the highlighted item.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic menu">
      <Menu>
        <Menu.Trigger
          render={<Button data-testid="menu-basic-trigger">Actions</Button>}
        />
        <Menu.Portal>
          <Menu.Popup data-testid="menu-basic-popup">
            <Menu.Item data-testid="menu-basic-item-new">New file</Menu.Item>
            <Menu.Item>Open…</Menu.Item>
            <Menu.Item>Save</Menu.Item>
            <Menu.Item>Save as…</Menu.Item>
          </Menu.Popup>
        </Menu.Portal>
      </Menu>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("button", { name: /^actions$/i });

    // Base UI 1.5.0 Menu triggers advertise the popup via
    // aria-haspopup="menu" — they do NOT toggle aria-expanded (that is a
    // Dialog/Popover trigger affordance). Open-state is observed through
    // the menu popup itself appearing, not a trigger attribute.
    await expect(trigger).toHaveAttribute("aria-haspopup", "menu");
    await userEvent.click(trigger);
    // findByRole polls until Base UI commits the open and mounts the
    // popup — this is the faithful "menu is open" signal.
    await expect(
      await body.findByRole("menuitem", { name: /new file/i }),
    ).toBeVisible();

    await userEvent.keyboard("{ArrowDown}{ArrowDown}{Enter}");
    // Enter activates the highlighted item and closes the menu; the
    // popup unmounts, so the menuitem leaves the accessibility tree.
    await waitFor(() =>
      expect(
        body.queryByRole("menuitem", { name: /new file/i }),
      ).not.toBeInTheDocument(),
    );
  },
};

/* ─── 2. WithGroups ─────────────────────────────────────────────────── */
export const WithGroups: Story = {
  name: "With groups + group label",
  parameters: {
    docs: {
      description: {
        story:
          "Group wraps a logical cluster of items; GroupLabel auto-wires " +
          "aria-labelledby so screen readers announce the group on enter.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Grouped menu">
      <Menu>
        <Menu.Trigger
          render={<Button data-testid="menu-groups-trigger">Edit</Button>}
        />
        <Menu.Portal>
          <Menu.Popup data-testid="menu-groups-popup">
            <Menu.Group>
              <Menu.GroupLabel>Clipboard</Menu.GroupLabel>
              <Menu.Item>Cut</Menu.Item>
              <Menu.Item>Copy</Menu.Item>
              <Menu.Item>Paste</Menu.Item>
            </Menu.Group>
            <Menu.Group>
              <Menu.GroupLabel>Selection</Menu.GroupLabel>
              <Menu.Item>Select all</Menu.Item>
              <Menu.Item>Find…</Menu.Item>
            </Menu.Group>
          </Menu.Popup>
        </Menu.Portal>
      </Menu>
    </div>
  ),
};

/* ─── 3. WithSeparator ──────────────────────────────────────────────── */
export const WithSeparator: Story = {
  name: "With separator",
  parameters: {
    docs: {
      description: {
        story:
          "Separator paints a hairline between item clusters. Carries no " +
          "ARIA semantics — purely visual.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Separated menu">
      <Menu>
        <Menu.Trigger
          render={
            <Button data-testid="menu-separator-trigger">File</Button>
          }
        />
        <Menu.Portal>
          <Menu.Popup data-testid="menu-separator-popup">
            <Menu.Item>New</Menu.Item>
            <Menu.Item>Open…</Menu.Item>
            <Menu.Separator />
            <Menu.Item>Save</Menu.Item>
            <Menu.Item>Save as…</Menu.Item>
            <Menu.Separator />
            <Menu.Item>Quit</Menu.Item>
          </Menu.Popup>
        </Menu.Portal>
      </Menu>
    </div>
  ),
};

/* ─── 4. WithCheckboxItem ───────────────────────────────────────────── */
export const WithCheckboxItem: Story = {
  name: "With checkbox items",
  parameters: {
    docs: {
      description: {
        story:
          "CheckboxItem toggles a boolean and emits onCheckedChange. The " +
          "indicator gutter reuses the Checkbox accent-fill visual; rows " +
          "stay aligned with plain Items because every Item reserves the " +
          "gutter column.",
      },
    },
  },
  render: function CheckboxStory() {
    const [bold, setBold] = useState(true);
    const [italic, setItalic] = useState(false);
    const [underline, setUnderline] = useState(false);
    return (
      <div className="zs-story-row" role="group" aria-label="Checkbox menu">
        <Menu>
          <Menu.Trigger
            render={
              <Button data-testid="menu-checkbox-trigger">Format</Button>
            }
          />
          <Menu.Portal>
            <Menu.Popup data-testid="menu-checkbox-popup">
              <Menu.CheckboxItem
                checked={bold}
                onCheckedChange={(next) => setBold(next)}
                data-testid="menu-checkbox-bold"
              >
                Bold
              </Menu.CheckboxItem>
              <Menu.CheckboxItem
                checked={italic}
                onCheckedChange={(next) => setItalic(next)}
                data-testid="menu-checkbox-italic"
              >
                Italic
              </Menu.CheckboxItem>
              <Menu.CheckboxItem
                checked={underline}
                onCheckedChange={(next) => setUnderline(next)}
                data-testid="menu-checkbox-underline"
              >
                Underline
              </Menu.CheckboxItem>
            </Menu.Popup>
          </Menu.Portal>
        </Menu>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", { name: /format/i }));
    const bold = await body.findByRole("menuitemcheckbox", { name: /bold/i });
    const italic = body.getByRole("menuitemcheckbox", { name: /italic/i });

    await expect(bold).toHaveAttribute("aria-checked", "true");
    await expect(italic).toHaveAttribute("aria-checked", "false");
    await userEvent.click(italic);
    await waitFor(() =>
      expect(italic).toHaveAttribute("aria-checked", "true"),
    );
    await userEvent.keyboard("{Escape}");
  },
};

/* ─── 5. WithRadioGroup ─────────────────────────────────────────────── */
export const WithRadioGroup: Story = {
  name: "With radio group",
  parameters: {
    docs: {
      description: {
        story:
          "RadioGroup is the value-keyed parent; RadioItems stamp values " +
          "and inherit selection. ArrowDown / ArrowUp move selection " +
          "within the group.",
      },
    },
  },
  render: function RadioStory() {
    const [theme, setTheme] = useState<string>("system");
    return (
      <div className="zs-story-row" role="group" aria-label="Radio menu">
        <Menu>
          <Menu.Trigger
            render={
              <Button data-testid="menu-radio-trigger">Appearance</Button>
            }
          />
          <Menu.Portal>
            <Menu.Popup data-testid="menu-radio-popup">
              <Menu.RadioGroup
                value={theme}
                onValueChange={(next: string) => setTheme(next)}
              >
                <Menu.GroupLabel>Theme</Menu.GroupLabel>
                <Menu.RadioItem value="light" data-testid="menu-radio-light">
                  Light
                </Menu.RadioItem>
                <Menu.RadioItem value="dark" data-testid="menu-radio-dark">
                  Dark
                </Menu.RadioItem>
                <Menu.RadioItem value="system" data-testid="menu-radio-system">
                  System
                </Menu.RadioItem>
              </Menu.RadioGroup>
            </Menu.Popup>
          </Menu.Portal>
        </Menu>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(
      canvas.getByRole("button", { name: /appearance/i }),
    );
    const system = await body.findByRole("menuitemradio", { name: /system/i });
    const dark = body.getByRole("menuitemradio", { name: /dark/i });

    await expect(system).toHaveAttribute("aria-checked", "true");
    await userEvent.click(dark);
    await waitFor(() => expect(dark).toHaveAttribute("aria-checked", "true"));
    await userEvent.keyboard("{Escape}");
  },
};

/* ─── 6. WithIcons ──────────────────────────────────────────────────── */
export const WithIcons: Story = {
  name: "With icons",
  parameters: {
    docs: {
      description: {
        story:
          "An icon slot lives inside the item's text column. Plain SVGs " +
          "(no fancy icon library) keep the design surface honest — the " +
          "design system carries no opinion on which set the consumer ships.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Icons menu">
      <Menu>
        <Menu.Trigger
          render={<Button data-testid="menu-icons-trigger">Project</Button>}
        />
        <Menu.Portal>
          <Menu.Popup data-testid="menu-icons-popup">
            <Menu.Item>
              <span style={{ display: "inline-flex", gap: "0.5rem", alignItems: "center" }}>
                <IconFile />
                <span>New file</span>
              </span>
            </Menu.Item>
            <Menu.Item>
              <span style={{ display: "inline-flex", gap: "0.5rem", alignItems: "center" }}>
                <IconFolder />
                <span>New folder</span>
              </span>
            </Menu.Item>
            <Menu.Separator />
            <Menu.Item>
              <span style={{ display: "inline-flex", gap: "0.5rem", alignItems: "center" }}>
                <IconArchive />
                <span>Archive</span>
              </span>
            </Menu.Item>
          </Menu.Popup>
        </Menu.Portal>
      </Menu>
    </div>
  ),
};

/* ─── 7. WithKeyboardShortcuts ──────────────────────────────────────── */
export const WithKeyboardShortcuts: Story = {
  name: "With keyboard shortcuts",
  parameters: {
    docs: {
      description: {
        story:
          "Each row passes a `shortcut` prop that projects into the row's " +
          "trailing column. The hint is presentational — Base UI's " +
          "text-navigation matches the row's label, not its shortcut text. " +
          "The grid pins the shortcut to the inline-end edge so multi-word " +
          "labels keep their natural flow.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Keyboard shortcuts menu">
      <Menu>
        <Menu.Trigger
          render={<Button data-testid="menu-kbd-trigger">Edit</Button>}
        />
        <Menu.Portal>
          <Menu.Popup data-testid="menu-kbd-popup">
            <Menu.Item shortcut="⌘X">Cut</Menu.Item>
            <Menu.Item shortcut="⌘C">Copy</Menu.Item>
            <Menu.Item shortcut="⌘V">Paste</Menu.Item>
            <Menu.Separator />
            <Menu.Item shortcut="⇧⌘Z">Redo</Menu.Item>
          </Menu.Popup>
        </Menu.Portal>
      </Menu>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", { name: /^edit$/i }));
    const paste = await body.findByRole("menuitem", { name: /^paste$/i });
    await expect(paste).toBeVisible();
    // Base UI commits the open in two async steps: the popup mounts, then
    // focus moves into the list (the first item gains roving focus and the
    // typeahead key handler binds). A "P" keystroke fired in the window
    // between those steps is dropped — the menu has no focused item to
    // type-match against — and a single dropped key is unrecoverable, so a
    // bare press + assert flakes nondeterministically (the failing run
    // shows focus stranded on the first item, never advancing to Paste).
    // Gate on focus landing inside the list first, THEN press, and re-press
    // inside waitFor so a dropped key self-heals: "P" matches "Paste"
    // regardless of which row currently holds the roving focus, and Base
    // UI's typeahead buffer clears between attempts, so re-pressing is
    // idempotent toward the target.
    await waitFor(() =>
      expect(
        body
          .getAllByRole("menuitem")
          .some((item) => item === document.activeElement),
      ).toBe(true),
    );
    await waitFor(async () => {
      await userEvent.keyboard("P");
      await expect(paste).toHaveFocus();
    });
    await userEvent.keyboard("{Escape}");
  },
};

/* ─── 8. NestedSubmenu ──────────────────────────────────────────────── */
export const NestedSubmenu: Story = {
  name: "Nested submenu",
  parameters: {
    docs: {
      description: {
        story:
          "Menu.Submenu sugars SubmenuRoot + SubmenuTrigger + Portal + " +
          "Positioner + Popup. ArrowRight opens the submenu; ArrowLeft / " +
          "ESC closes it; closeParentOnEsc default keeps the parent open.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Nested menu">
      <Menu>
        <Menu.Trigger
          render={<Button data-testid="menu-submenu-trigger">More</Button>}
        />
        <Menu.Portal>
          <Menu.Popup data-testid="menu-submenu-popup">
            <Menu.Item>Open</Menu.Item>
            <Menu.Item>Rename…</Menu.Item>
            <Menu.Submenu
              trigger="Share with…"
              data-testid="menu-submenu-share"
            >
              <Menu.Item data-testid="menu-submenu-share-email">Email</Menu.Item>
              <Menu.Item>Slack</Menu.Item>
              <Menu.Item>Copy link</Menu.Item>
            </Menu.Submenu>
            <Menu.Separator />
            <Menu.Item>Delete</Menu.Item>
          </Menu.Popup>
        </Menu.Portal>
      </Menu>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", { name: /more/i }));
    const share = await body.findByRole("menuitem", { name: /share with/i });
    await userEvent.hover(share);
    await expect(
      await body.findByRole("menuitem", { name: /email/i }),
    ).toBeVisible();
    // ESC closes the submenu; closeParentOnEsc default keeps the PARENT open.
    await userEvent.keyboard("{Escape}");
    await waitFor(() =>
      expect(body.queryByRole("menuitem", { name: /email/i })).toBeNull(),
    );
    await expect(share).toBeVisible();
    // Second ESC tears the parent down too. We assert the fully-closed
    // end-state so the a11y audit in postVisit runs against a clean DOM:
    // while a non-modal Base UI menu is open it leaves `tabindex="0"`
    // focus-guard sentinels (`data-base-ui-focus-guard`) parked next to
    // the trigger. axe's `aria-hidden-focus` rule flags those guards
    // (they're `aria-hidden` yet focusable — Base UI uses them for
    // focus-wrap detection). They vanish once every menu level closes, so
    // closing fully is the faithful way to keep axe clean without
    // suppressing a rule — and it also exercises the full teardown path.
    await userEvent.keyboard("{Escape}");
    await waitFor(() =>
      expect(body.queryByRole("menuitem", { name: /share with/i })).toBeNull(),
    );
  },
};

/* ─── 9. WithArrow ──────────────────────────────────────────────────── */
export const WithArrow: Story = {
  name: "With arrow",
  parameters: {
    docs: {
      description: {
        story:
          "Arrow renders an SVG triangle that Floating UI rotates per " +
          "side. The path matches Popover's so the design surface stays " +
          "consistent across the popover family.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Arrow menu">
      <Menu>
        <Menu.Trigger
          render={<Button data-testid="menu-arrow-trigger">Options</Button>}
        />
        <Menu.Portal>
          <Menu.Popup data-testid="menu-arrow-popup">
            <Menu.Arrow data-testid="menu-arrow-glyph" />
            <Menu.Item>Edit</Menu.Item>
            <Menu.Item>Duplicate</Menu.Item>
            <Menu.Item>Delete</Menu.Item>
          </Menu.Popup>
        </Menu.Portal>
      </Menu>
    </div>
  ),
};

/* ─── 10. DisabledItem ─────────────────────────────────────────────── */
export const DisabledItem: Story = {
  name: "Disabled item",
  parameters: {
    docs: {
      description: {
        story:
          "Disabled items keep their gutter slot but read as secondary " +
          "ink + not-allowed cursor. Keyboard focus skips disabled rows " +
          "(Base UI handles the roving-focus bookkeeping).",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled item menu">
      <Menu>
        <Menu.Trigger
          render={
            <Button data-testid="menu-disabled-trigger">Actions</Button>
          }
        />
        <Menu.Portal>
          <Menu.Popup data-testid="menu-disabled-popup">
            <Menu.Item>Open</Menu.Item>
            <Menu.Item disabled data-testid="menu-disabled-item">
              Duplicate (read-only)
            </Menu.Item>
            <Menu.Item>Rename…</Menu.Item>
            <Menu.Separator />
            <Menu.Item disabled>Delete (no permission)</Menu.Item>
          </Menu.Popup>
        </Menu.Portal>
      </Menu>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", { name: /actions/i }));
    const duplicate = await body.findByRole("menuitem", {
      name: /duplicate/i,
    });

    await expect(duplicate).toHaveAttribute("aria-disabled", "true");
    // Clicking a disabled item must NOT activate it: Base UI keeps the
    // popup open and the item stays aria-disabled (no onSelect / close).
    // Note Base UI 1.5.0 DOES highlight/focus a disabled item on pointer
    // press — disabled menu rows stay focusable (aria-disabled, not the
    // `disabled` attribute) so AT users can perceive them — so we assert
    // the non-activation contract, not the absence of focus.
    await userEvent.click(duplicate);
    await expect(duplicate).toHaveAttribute("aria-disabled", "true");
    // The popup stays open after the no-op click — the disabled item is
    // still mounted and visible (activating it would unmount the popup).
    // (Base UI Menu triggers expose aria-haspopup, not aria-expanded, so
    // open-state is observed through the popup's own presence.)
    await expect(duplicate).toBeVisible();
    await expect(
      body.getByRole("menuitem", { name: /^open$/i }),
    ).toBeVisible();
    await userEvent.keyboard("{Escape}");
  },
};

/* ─── 11. PlacementSide ─────────────────────────────────────────────── */
export const PlacementSide: Story = {
  name: "Placement: side",
  parameters: {
    docs: {
      description: {
        story:
          "Four placement sides (top / right / bottom / left). Floating " +
          "UI auto-flips on collision so the rendered side may differ " +
          "from the requested one near the viewport edge.",
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
        <Menu key={side}>
          <Menu.Trigger
            render={
              <Button
                variant="tinted"
                data-testid={`menu-side-${side}-trigger`}
              >
                Open {side}
              </Button>
            }
          />
          <Menu.Portal>
            <Menu.Popup
              side={side}
              data-testid={`menu-side-${side}-popup`}
            >
              <Menu.Item>Item one</Menu.Item>
              <Menu.Item>Item two</Menu.Item>
              <Menu.Item>Item three</Menu.Item>
            </Menu.Popup>
          </Menu.Portal>
        </Menu>
      ))}
    </div>
  ),
};

/* ─── 12. Rtl ───────────────────────────────────────────────────────── */
export const Rtl: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Arabic content flows right-to-left; logical-axis padding " +
          "keeps the row layout correct. The submenu chevron flips so it " +
          "still points toward the submenu's anchored side.",
      },
    },
  },
  render: () => (
    // DirectionProvider seeds Base UI's DirectionContext so the
    // Floating UI positioner resolves `inline-start` / `inline-end`
    // against the RTL axis. A bare `dir="rtl"` div is invisible to
    // Base UI — the portal renders elsewhere in the tree and the
    // attribute doesn't propagate.
    <DirectionProvider direction="rtl">
      <div
        className="zs-story-row"
        role="group"
        aria-label="RTL menu"
        dir="rtl"
        lang="ar"
      >
        <Menu>
          <Menu.Trigger
            render={<Button data-testid="menu-rtl-trigger">إجراءات</Button>}
          />
          <Menu.Portal>
            <Menu.Popup data-testid="menu-rtl-popup">
              <Menu.Item>فتح</Menu.Item>
              <Menu.Item>إعادة تسمية…</Menu.Item>
              <Menu.Submenu
                trigger="مشاركة مع…"
                data-testid="menu-rtl-submenu"
              >
                <Menu.Item>البريد الإلكتروني</Menu.Item>
                <Menu.Item>نسخ الرابط</Menu.Item>
              </Menu.Submenu>
              <Menu.Separator />
              <Menu.Item>حذف</Menu.Item>
            </Menu.Popup>
          </Menu.Portal>
        </Menu>
      </div>
    </DirectionProvider>
  ),
};

/* ─── 13. WithLinkItemAsChild ───────────────────────────────────────── *
 *
 * LinkItem renders an `<a>` natively. `asChild` defers row layout to
 * the caller's element so a router `<Link>` (Next.js / TanStack /
 * React Router) renders in its place. Slot owns ref composition — we
 * don't pre-merge the child ref here (Slice 11 review-fix #7). */
export const WithLinkItemAsChild: Story = {
  name: "LinkItem asChild (custom router link)",
  parameters: {
    docs: {
      description: {
        story:
          "LinkItem renders an `<a>` natively. `asChild` defers the " +
          "rendered element to a caller-provided child (e.g. a router " +
          "Link). Slot fans the ref out without double-composition.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="LinkItem asChild menu">
      <Menu>
        <Menu.Trigger
          render={<Button data-testid="menu-link-trigger">Help</Button>}
        />
        <Menu.Portal>
          <Menu.Popup data-testid="menu-link-popup">
            <Menu.Item>Documentation</Menu.Item>
            <Menu.LinkItem
              href="https://example.com/docs"
              data-testid="menu-link-native"
            >
              Read docs
            </Menu.LinkItem>
            <Menu.LinkItem asChild>
              <a
                href="https://example.com/support"
                data-testid="menu-link-aschild"
              >
                Contact support
              </a>
            </Menu.LinkItem>
          </Menu.Popup>
        </Menu.Portal>
      </Menu>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", { name: /help/i }));
    const native = await body.findByRole("menuitem", { name: /read docs/i });
    const support = body.getByRole("menuitem", { name: /contact support/i });

    await expect(native).toHaveAttribute("href", "https://example.com/docs");
    await expect(support).toHaveAttribute(
      "href",
      "https://example.com/support",
    );
    await expect(support.tagName).toBe("A");
    await userEvent.keyboard("{Escape}");
  },
};

/* ─── 13b. LinkItemAsChildSingleAttach (wave9 🔴 2 regression) ──────── *
 *
 * Pre-fix, `Menu.LinkItem` passed the forwarded `ref` to
 * `<BaseMenu.LinkItem>` AND then composed that same ref with
 * `linkProps.ref` inside the render-prop. Base UI round-tripped the
 * outer ref back through `linkProps.ref` (see Base UI's
 * `useRenderElement` — the forwarded ref is in the `[linkRef, buttonRef,
 * forwardedRef, listItem.ref]` merge that becomes `linkProps.ref`), so
 * `composeRefs(outerRef, linkProps.ref)` invoked the caller's callback
 * ref TWICE per attach. The fix drops the outer `ref={…}` on
 * `<BaseMenu.LinkItem>` so `linkProps.ref` is Base UI's internal element
 * ref only — each side of `composeRefs` fires the caller exactly once.
 *
 * Test design — we measure a RATIO, not an absolute count. The Menu
 * popup mounts/remounts a few times during open animation, so the same
 * DOM `<a>` may be attached more than once. The bug doubled the rate
 * per attach; we install:
 *
 *   - `wrapperRef` on `<Menu.LinkItem ref={…}>` — the path the review
 *     flagged. Pre-fix this fires TWICE per attach event.
 *   - `domRef` on the rendered DOM `<a>` (via `asChild`) — fires ONCE
 *     per attach event regardless of the bug.
 *
 * Post-fix: `wrapperRef === domRef`. Pre-fix: `wrapperRef === 2 *
 * domRef`. Aria-wiring reads both counters and asserts equality. */
function LinkItemAsChildSingleAttachImpl() {
  // Keep counts in a useRef so we don't trigger a re-render — Slot
  // recomputes `composeRefs(...)` on every commit, and React treats the
  // new callback identity as detach-old / attach-new. Any setState in
  // the callback would loop the mount. Mirrors the Card story comment.
  const wrapperRefCalls = useRef(0);
  const domRefCalls = useRef(0);
  const statusRef = useRef<HTMLSpanElement | null>(null);
  const writeStatus = () => {
    if (statusRef.current) {
      statusRef.current.textContent =
        `wrapper=${wrapperRefCalls.current} ` +
        `dom=${domRefCalls.current}`;
    }
  };
  const stableWrapperRef = useCallback((node: HTMLAnchorElement | null) => {
    if (node) {
      wrapperRefCalls.current += 1;
      writeStatus();
    }
  }, []);
  const stableDomRef = useCallback((node: HTMLAnchorElement | null) => {
    if (node) {
      domRefCalls.current += 1;
      writeStatus();
    }
  }, []);
  const stableStatusRef = useCallback((node: HTMLSpanElement | null) => {
    statusRef.current = node;
    writeStatus();
  }, []);
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="LinkItem asChild single attach"
    >
      <Menu>
        <Menu.Trigger
          render={
            <Button data-testid="menu-link-attach-trigger">Help</Button>
          }
        />
        <Menu.Portal>
          <Menu.Popup data-testid="menu-link-attach-popup">
            <Menu.LinkItem
              asChild
              // Wrapper-level ref — the surface the review flagged.
              // Pre-fix this fires twice per attach (linkProps.ref
              // round-trips the forwarded ref back through composeRefs);
              // post-fix once per attach. */
              ref={stableWrapperRef}
            >
              <a
                // DOM-level ref — fires once per attach regardless of
                // the wrapper bug. Acts as the denominator in the
                // ratio assertion. */
                ref={stableDomRef}
                href="https://example.com/single-attach"
                data-testid="menu-link-attach-anchor"
              >
                Single attach
              </a>
            </Menu.LinkItem>
          </Menu.Popup>
        </Menu.Portal>
      </Menu>
      {/* Visible outside the popup so aria-wiring can read the snapshot
       *  even after Escape closes the popup. */}
      <p>
        <span
          role="status"
          aria-label="LinkItem attach count"
          data-testid="menu-link-attach-count"
          ref={stableStatusRef}
        >
          wrapper=0 dom=0
        </span>
      </p>
    </div>
  );
}
export const LinkItemAsChildSingleAttach: Story = {
  name: "LinkItem asChild single ref attach",
  parameters: {
    docs: {
      description: {
        story:
          "Regression for the Menu.LinkItem double-ref-compose bug " +
          "(wave9 🔴 2). The wrapper-level ref on `<Menu.LinkItem ref>` " +
          "must fire exactly once per attach event; the DOM-level ref " +
          "is the denominator the wrapper ratio is compared against.",
      },
    },
  },
  render: () => <LinkItemAsChildSingleAttachImpl />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("button", { name: /help/i });
    await userEvent.click(trigger);
    // Wait for the popup → mounts the asChild <a> → fires the callback
    // refs.
    await body.findByTestId("menu-link-attach-popup");
    const status = canvas.getByRole("status", {
      name: /linkitem attach count/i,
    });
    // Post-fix: wrapper == dom (each attaches the wrapper ref once).
    // Pre-fix: wrapper == 2 * dom (linkProps.ref round-tripped the
    // forwarded ref so composeRefs fired it twice per attach event).
    await waitFor(() => {
      const text = (status.textContent ?? "").trim();
      const match = text.match(/wrapper=(\d+)\s+dom=(\d+)/);
      expect(match).not.toBeNull();
      if (match) {
        const wrapper = Number(match[1]);
        const dom = Number(match[2]);
        // dom must have fired at least once; wrapper must equal dom.
        expect(dom).toBeGreaterThan(0);
        expect(wrapper).toBe(dom);
      }
    });
    await userEvent.keyboard("{Escape}");
  },
};

/* ─── 14. ModalBackdrop ────────────────────────────────────────────── */
export const ModalBackdrop: Story = {
  name: "Modal menu with backdrop",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Modal menu">
      <Menu modal>
        <Menu.Trigger render={<Button>Share</Button>} />
        <Menu.Portal>
          <Menu.Backdrop />
          <Menu.Popup align="center" sideOffset={10}>
            <Menu.Arrow>
              <svg width="16" height="8" viewBox="0 0 16 8" aria-hidden="true">
                <path d="M 0,0 L 8,8 L 16,0 Z" fill="currentColor" />
              </svg>
            </Menu.Arrow>
            <Menu.Item shortcut="S">Send invite</Menu.Item>
            <Menu.CheckboxItem checked={false} shortcut="N">
              Notify team
            </Menu.CheckboxItem>
            <Menu.RadioGroup value="viewer">
              <Menu.RadioItem value="viewer" shortcut="V">
                Viewer
              </Menu.RadioItem>
              <Menu.RadioItem value="editor" shortcut="E">
                Editor
              </Menu.RadioItem>
            </Menu.RadioGroup>
            <Menu.LinkItem href="https://example.com/audit" shortcut="A">
              Audit log
            </Menu.LinkItem>
          </Menu.Popup>
        </Menu.Portal>
      </Menu>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.click(canvas.getByRole("button", { name: /share/i }));
    await expect(
      await body.findByRole("menuitem", { name: /send invite/i }),
    ).toBeVisible();
    await expect(
      body.getByRole("menuitemcheckbox", { name: /notify team/i }),
    ).toHaveAttribute("aria-checked", "false");
    await expect(
      body.getByRole("menuitemradio", { name: /viewer/i }),
    ).toHaveAttribute("aria-checked", "true");
    await expect(
      body.getByRole("menuitem", { name: /audit log/i }),
    ).toHaveAttribute("href", "https://example.com/audit");
    await userEvent.keyboard("{Escape}");
  },
};

/* ─── tiny SVG glyphs ─────────────────────────────────────────────── */

function IconFile() {
  return (
    <svg
      width="16"
      height="16"
      viewBox="0 0 16 16"
      aria-hidden="true"
      focusable="false"
    >
      <path
        d="M3 2.5A1.5 1.5 0 0 1 4.5 1h5L13 4.5v9A1.5 1.5 0 0 1 11.5 15h-7A1.5 1.5 0 0 1 3 13.5Z"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.25"
      />
    </svg>
  );
}

function IconFolder() {
  return (
    <svg
      width="16"
      height="16"
      viewBox="0 0 16 16"
      aria-hidden="true"
      focusable="false"
    >
      <path
        d="M2 4.5A1.5 1.5 0 0 1 3.5 3h3l1.25 1.5h5A1.5 1.5 0 0 1 14 6v6.5A1.5 1.5 0 0 1 12.5 14h-9A1.5 1.5 0 0 1 2 12.5Z"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.25"
      />
    </svg>
  );
}

function IconArchive() {
  return (
    <svg
      width="16"
      height="16"
      viewBox="0 0 16 16"
      aria-hidden="true"
      focusable="false"
    >
      <rect
        x="2"
        y="3"
        width="12"
        height="3"
        rx="0.5"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.25"
      />
      <path
        d="M3 6.5v6A1.5 1.5 0 0 0 4.5 14h7a1.5 1.5 0 0 0 1.5-1.5v-6"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.25"
      />
      <path
        d="M6.5 9h3"
        stroke="currentColor"
        strokeWidth="1.25"
        strokeLinecap="round"
      />
    </svg>
  );
}
