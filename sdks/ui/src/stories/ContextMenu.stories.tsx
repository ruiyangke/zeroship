import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { DirectionProvider } from "@base-ui/react/direction-provider";
import { Card, ContextMenu } from "../components";

const meta: Meta<typeof ContextMenu> = {
  title: "Components/ContextMenu",
  component: ContextMenu,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof ContextMenu>;

/* ─── 1. BasicRightClickArea ────────────────────────────────────────── */
export const BasicRightClickArea: Story = {
  name: "Basic right-click area",
  parameters: {
    docs: {
      description: {
        story:
          "Right-click anywhere on the trigger area (the bordered box) to " +
          "open the menu at the pointer coords. Shift+F10 on a focused " +
          "trigger also opens it; ESC closes.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic context menu">
      <ContextMenu>
        <ContextMenu.Trigger
          data-testid="contextmenu-basic-trigger"
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            inlineSize: "16rem",
            blockSize: "8rem",
            border: "0.0625rem dashed var(--zs-separator)",
            borderRadius: "var(--zs-radius-3)",
            padding: "var(--zs-space-4)",
            color: "var(--zs-label-secondary)",
            textAlign: "center",
          }}
        >
          Right-click here
        </ContextMenu.Trigger>
        <ContextMenu.Portal>
          <ContextMenu.Popup data-testid="contextmenu-basic-popup">
            <ContextMenu.Item data-testid="contextmenu-basic-item-open">
              Open
            </ContextMenu.Item>
            <ContextMenu.Item>Rename…</ContextMenu.Item>
            <ContextMenu.Separator />
            <ContextMenu.Item>Delete</ContextMenu.Item>
          </ContextMenu.Popup>
        </ContextMenu.Portal>
      </ContextMenu>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByText(/right-click here/i);

    await userEvent.pointer([{ target: trigger, keys: "[MouseRight]" }]);
    await expect(
      await body.findByRole("menuitem", { name: /open/i }),
    ).toBeVisible();
    await userEvent.keyboard("{Escape}");
    await waitFor(() =>
      expect(body.queryByRole("menuitem", { name: /open/i })).not.toBeInTheDocument(),
    );
  },
};

/* ─── 2. WithCheckboxItem ───────────────────────────────────────────── */
export const WithCheckboxItem: Story = {
  name: "With checkbox item",
  parameters: {
    docs: {
      description: {
        story:
          "ContextMenu reuses Menu's CheckboxItem — same indicator gutter, " +
          "same accent-fill visual, same hit-target floor. The state lives " +
          "in the host component; ContextMenu only renders.",
      },
    },
  },
  render: function CheckboxStory() {
    const [pinned, setPinned] = useState(false);
    const [favorited, setFavorited] = useState(true);
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Checkbox context menu"
      >
        <ContextMenu>
          <ContextMenu.Trigger
            data-testid="contextmenu-checkbox-trigger"
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
              inlineSize: "16rem",
              blockSize: "8rem",
              border: "0.0625rem dashed var(--zs-separator)",
              borderRadius: "var(--zs-radius-3)",
              color: "var(--zs-label-secondary)",
            }}
          >
            Right-click for options
          </ContextMenu.Trigger>
          <ContextMenu.Portal>
            <ContextMenu.Popup data-testid="contextmenu-checkbox-popup">
              <ContextMenu.CheckboxItem
                checked={pinned}
                onCheckedChange={(next) => setPinned(next)}
                data-testid="contextmenu-checkbox-pin"
              >
                Pinned
              </ContextMenu.CheckboxItem>
              <ContextMenu.CheckboxItem
                checked={favorited}
                onCheckedChange={(next) => setFavorited(next)}
              >
                Favorited
              </ContextMenu.CheckboxItem>
            </ContextMenu.Popup>
          </ContextMenu.Portal>
        </ContextMenu>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.pointer([
      { target: canvas.getByText(/right-click for options/i), keys: "[MouseRight]" },
    ]);
    const pinned = await body.findByRole("menuitemcheckbox", {
      name: /pinned/i,
    });
    const favorited = body.getByRole("menuitemcheckbox", {
      name: /favorited/i,
    });

    await expect(pinned).toHaveAttribute("aria-checked", "false");
    await expect(favorited).toHaveAttribute("aria-checked", "true");
    await userEvent.click(pinned);
    await waitFor(() =>
      expect(pinned).toHaveAttribute("aria-checked", "true"),
    );
    await userEvent.keyboard("{Escape}");
  },
};

/* ─── 3. NestedSubmenu ──────────────────────────────────────────────── */
export const NestedSubmenu: Story = {
  name: "Nested submenu",
  parameters: {
    docs: {
      description: {
        story:
          "Submenus inside a ContextMenu use the same Menu.Submenu sugar — " +
          "ArrowRight opens, ArrowLeft closes, the chevron tail points " +
          "toward the anchored side.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Submenu context menu"
    >
      <ContextMenu>
        <ContextMenu.Trigger
          data-testid="contextmenu-submenu-trigger"
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            inlineSize: "16rem",
            blockSize: "8rem",
            border: "0.0625rem dashed var(--zs-separator)",
            borderRadius: "var(--zs-radius-3)",
            color: "var(--zs-label-secondary)",
          }}
        >
          Right-click for nested
        </ContextMenu.Trigger>
        <ContextMenu.Portal>
          <ContextMenu.Popup data-testid="contextmenu-submenu-popup">
            <ContextMenu.Item>Open</ContextMenu.Item>
            <ContextMenu.Submenu
              trigger="Move to…"
              data-testid="contextmenu-submenu-move"
            >
              <ContextMenu.Item>Inbox</ContextMenu.Item>
              <ContextMenu.Item>Archive</ContextMenu.Item>
              <ContextMenu.Item>Trash</ContextMenu.Item>
            </ContextMenu.Submenu>
            <ContextMenu.Item>Share</ContextMenu.Item>
          </ContextMenu.Popup>
        </ContextMenu.Portal>
      </ContextMenu>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.pointer([
      { target: canvas.getByText(/right-click for nested/i), keys: "[MouseRight]" },
    ]);
    const move = await body.findByRole("menuitem", { name: /move to/i });
    await userEvent.hover(move);
    await expect(
      await body.findByRole("menuitem", { name: /archive/i }),
    ).toBeVisible();
    await userEvent.keyboard("{Escape}");
  },
};

/* ─── 4. WithDisabledItem ───────────────────────────────────────────── */
export const WithDisabledItem: Story = {
  name: "With disabled item",
  parameters: {
    docs: {
      description: {
        story:
          "Disabled items keep their gutter slot; keyboard focus skips " +
          "them. Pointer cursor reads not-allowed.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Disabled context menu"
    >
      <ContextMenu>
        <ContextMenu.Trigger
          data-testid="contextmenu-disabled-trigger"
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            inlineSize: "16rem",
            blockSize: "8rem",
            border: "0.0625rem dashed var(--zs-separator)",
            borderRadius: "var(--zs-radius-3)",
            color: "var(--zs-label-secondary)",
          }}
        >
          Right-click for actions
        </ContextMenu.Trigger>
        <ContextMenu.Portal>
          <ContextMenu.Popup data-testid="contextmenu-disabled-popup">
            <ContextMenu.Item>Open</ContextMenu.Item>
            <ContextMenu.Item disabled>Duplicate (read-only)</ContextMenu.Item>
            <ContextMenu.Separator />
            <ContextMenu.Item disabled>Delete (no permission)</ContextMenu.Item>
          </ContextMenu.Popup>
        </ContextMenu.Portal>
      </ContextMenu>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.pointer([
      { target: canvas.getByText(/right-click for actions/i), keys: "[MouseRight]" },
    ]);
    const duplicate = await body.findByRole("menuitem", {
      name: /duplicate/i,
    });

    await expect(duplicate).toHaveAttribute("aria-disabled", "true");
    // Clicking a disabled item must NOT activate it: the popup stays
    // open and the item remains aria-disabled (no onSelect / close).
    // Base UI 1.5.0 DOES highlight/focus a disabled item on pointer
    // press — disabled menu rows stay focusable (aria-disabled, not the
    // `disabled` attribute) so AT users can perceive them — so we assert
    // the non-activation contract, not the absence of focus.
    await userEvent.click(duplicate);
    await expect(duplicate).toHaveAttribute("aria-disabled", "true");
    await expect(duplicate).toBeVisible();
    await userEvent.keyboard("{Escape}");
  },
};

/* ─── 5. CustomAnchor ───────────────────────────────────────────────── */
export const CustomAnchor: Story = {
  name: "Custom anchor (Card)",
  parameters: {
    docs: {
      description: {
        story:
          "The trigger wraps a Card so right-clicking anywhere inside the " +
          "card surface opens the menu. The trigger is layout-neutral — it " +
          "doesn't push the card around.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Custom anchor context menu"
    >
      <ContextMenu>
        <ContextMenu.Trigger data-testid="contextmenu-card-trigger">
          <Card variant="elevated" style={{ inlineSize: "18rem" }}>
            <Card.Header>
              <Card.Title>release-notes.md</Card.Title>
              <Card.Description>Edited 4 minutes ago</Card.Description>
            </Card.Header>
            <Card.Content>
              Right-click anywhere on this card to see file actions.
            </Card.Content>
          </Card>
        </ContextMenu.Trigger>
        <ContextMenu.Portal>
          <ContextMenu.Popup data-testid="contextmenu-card-popup">
            <ContextMenu.Item>Open</ContextMenu.Item>
            <ContextMenu.Item>Reveal in finder</ContextMenu.Item>
            <ContextMenu.Separator />
            <ContextMenu.Item>Duplicate</ContextMenu.Item>
            <ContextMenu.Item>Move to trash</ContextMenu.Item>
          </ContextMenu.Popup>
        </ContextMenu.Portal>
      </ContextMenu>
    </div>
  ),
};

/* ─── 6. Rtl ────────────────────────────────────────────────────────── */
export const Rtl: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Right-to-left layout. The popup position tracks the pointer " +
          "coords; logical-axis padding keeps the row layout correct in " +
          "Arabic content.",
      },
    },
  },
  render: () => (
    // DirectionProvider seeds Base UI's DirectionContext for the
    // portal-rendered popup; see Menu Rtl story for the rationale.
    <DirectionProvider direction="rtl">
    <div
      className="zs-story-row"
      role="group"
      aria-label="RTL context menu"
      dir="rtl"
      lang="ar"
    >
      <ContextMenu>
        <ContextMenu.Trigger
          data-testid="contextmenu-rtl-trigger"
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            inlineSize: "16rem",
            blockSize: "8rem",
            border: "0.0625rem dashed var(--zs-separator)",
            borderRadius: "var(--zs-radius-3)",
            color: "var(--zs-label-secondary)",
          }}
        >
          انقر بالزر الأيمن هنا
        </ContextMenu.Trigger>
        <ContextMenu.Portal>
          <ContextMenu.Popup data-testid="contextmenu-rtl-popup">
            <ContextMenu.Item>فتح</ContextMenu.Item>
            <ContextMenu.Item>إعادة تسمية…</ContextMenu.Item>
            <ContextMenu.Separator />
            <ContextMenu.Item>حذف</ContextMenu.Item>
          </ContextMenu.Popup>
        </ContextMenu.Portal>
      </ContextMenu>
    </div>
    </DirectionProvider>
  ),
};

/* ─── 7. PositionedPopup ───────────────────────────────────────────── */
export const PositionedPopup: Story = {
  name: "Positioned popup override",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Positioned context menu"
    >
      <ContextMenu>
        <ContextMenu.Trigger
          role="button"
          aria-label="Open positioned context menu"
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            inlineSize: "16rem",
            blockSize: "8rem",
            border: "0.0625rem dashed var(--zs-separator)",
            borderRadius: "var(--zs-radius-3)",
            color: "var(--zs-label-secondary)",
          }}
        >
          Right-click for positioned actions
        </ContextMenu.Trigger>
        <ContextMenu.Portal>
          <ContextMenu.Popup side="right" align="start" sideOffset={12}>
            <ContextMenu.Arrow>
              <svg width="16" height="8" viewBox="0 0 16 8" aria-hidden="true">
                <path d="M 0,0 L 8,8 L 16,0 Z" fill="currentColor" />
              </svg>
            </ContextMenu.Arrow>
            <ContextMenu.Group>
              <ContextMenu.GroupLabel>File</ContextMenu.GroupLabel>
              <ContextMenu.LinkItem href="https://example.com/open">
                Open in browser
              </ContextMenu.LinkItem>
              <ContextMenu.Item shortcut="R">Rename</ContextMenu.Item>
            </ContextMenu.Group>
          </ContextMenu.Popup>
        </ContextMenu.Portal>
      </ContextMenu>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);

    await userEvent.pointer([
      {
        target: canvas.getByRole("button", {
          name: /open positioned context menu/i,
        }),
        keys: "[MouseRight]",
      },
    ]);
    await expect(
      await body.findByRole("menuitem", { name: /open in browser/i }),
    ).toHaveAttribute("href", "https://example.com/open");
    await expect(body.getByText(/file/i)).toBeVisible();
    await userEvent.keyboard("{Escape}");
  },
};

/* ─── 8. DisabledTrigger ───────────────────────────────────────────── */
export const DisabledTrigger: Story = {
  name: "Disabled trigger",
  parameters: {
    docs: {
      description: {
        story:
          "`disabled` lives on the Root. Base UI's ContextMenu short-" +
          "circuits its `contextmenu` / touch handlers AND the document-" +
          "level contextmenu listener, so a right-click on a disabled " +
          "trigger does NOT open the popup. Wave-9 fix.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Disabled context menu trigger"
    >
      <ContextMenu disabled>
        <ContextMenu.Trigger
          data-testid="contextmenu-disabled-trigger"
          role="button"
          aria-label="Unavailable context menu"
        >
          Unavailable actions
        </ContextMenu.Trigger>
        <ContextMenu.Portal>
          <ContextMenu.Popup data-testid="contextmenu-disabled-popup">
            <ContextMenu.Item>Hidden action</ContextMenu.Item>
          </ContextMenu.Popup>
        </ContextMenu.Portal>
      </ContextMenu>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("button", {
      name: /unavailable context menu/i,
    });

    await expect(trigger).toHaveAttribute("aria-disabled", "true");
    await expect(trigger).toHaveAttribute("tabindex", "-1");
    // Right-click MUST NOT open the popup. Pre-fix: trigger-level
    // `disabled` only stamped ARIA/tabindex; Base UI still routed the
    // contextmenu event through and the popup opened. Post-fix: the
    // Root provides a disabled context the Trigger reads and uses to
    // capture-stop the `contextmenu` event before Base UI's handler.
    await userEvent.pointer([{ target: trigger, keys: "[MouseRight]" }]);
    await expect(
      body.queryByRole("menuitem", { name: /hidden action/i }),
    ).not.toBeInTheDocument();
  },
};

/* ─── 9. AsChild ────────────────────────────────────────────────────── */
export const AsChild: Story = {
  name: "asChild (Card root is the trigger)",
  parameters: {
    docs: {
      description: {
        story:
          "`asChild` lets a semantic element BE the right-click target — " +
          "no wrapper `<div>`. The Card root receives Base UI's contextmenu " +
          "binding via the shared Slot helper; className, refs, and event " +
          "handlers compose. Useful when the visible bounding box should " +
          "match the consumer's intrinsic layout (no extra wrapper height).",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="asChild context menu"
    >
      <ContextMenu>
        <ContextMenu.Trigger asChild>
          <Card
            variant="elevated"
            style={{ inlineSize: "18rem" }}
            data-testid="contextmenu-aschild-card"
          >
            <Card.Header>
              <Card.Title>asChild — Card is the trigger</Card.Title>
              <Card.Description>
                Right-click anywhere on this Card to open the menu.
              </Card.Description>
            </Card.Header>
          </Card>
        </ContextMenu.Trigger>
        <ContextMenu.Portal>
          <ContextMenu.Popup data-testid="contextmenu-aschild-popup">
            <ContextMenu.Item data-testid="contextmenu-aschild-item-open">
              Open
            </ContextMenu.Item>
            <ContextMenu.Item>Rename…</ContextMenu.Item>
          </ContextMenu.Popup>
        </ContextMenu.Portal>
      </ContextMenu>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const card = canvas.getByTestId("contextmenu-aschild-card");

    // The Slot fan-out must put the Base UI contextmenu binding on the
    // Card itself, so right-clicking inside the Card opens the popup.
    await userEvent.pointer([{ target: card, keys: "[MouseRight]" }]);
    await expect(
      await body.findByRole("menuitem", { name: /open/i }),
    ).toBeVisible();
    await userEvent.keyboard("{Escape}");
    await waitFor(() =>
      expect(body.queryByRole("menuitem", { name: /open/i })).not.toBeInTheDocument(),
    );
  },
};
