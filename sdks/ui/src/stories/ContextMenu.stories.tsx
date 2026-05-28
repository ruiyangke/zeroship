import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react";
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
  ),
};
