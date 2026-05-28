import type { Meta, StoryObj } from "@storybook/react";
import { Button, Toggle, Toolbar } from "../components";

const meta: Meta<typeof Toolbar> = {
  title: "Components/Toolbar",
  component: Toolbar,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Toolbar>;

/* ─── Inline glyphs (icon-only buttons) ─────────────────────────────
 *
 * Same Bold/Italic/Underline marks Toggle.stories uses so the visual
 * vocabulary stays familiar across the rich-text patterns. */
function BoldGlyph() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        fill="currentColor"
        d="M5 3h4.2c2.1 0 3.5 1.05 3.5 2.85 0 1.05-.45 1.95-1.2 2.4 1.05.45 1.65 1.45 1.65 2.7C13.15 12.95 11.7 14 9.4 14H5V3zm2 4.45h2c.95 0 1.5-.45 1.5-1.2 0-.8-.55-1.2-1.5-1.2H7v2.4zm0 4.55h2.3c.95 0 1.5-.45 1.5-1.25 0-.85-.55-1.3-1.5-1.3H7V12z"
      />
    </svg>
  );
}
function ItalicGlyph() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        fill="currentColor"
        d="M6 3h6v1.5h-2L8 11.5h2V13H4v-1.5h2L8 4.5H6V3z"
      />
    </svg>
  );
}
function UnderlineGlyph() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        fill="currentColor"
        d="M4 2h1.5v6c0 1.7 1 2.5 2.5 2.5S10.5 9.7 10.5 8V2H12v6.1c0 2.4-1.6 3.9-4 3.9S4 10.5 4 8.1V2zm-.5 11.5h9V15h-9v-1.5z"
      />
    </svg>
  );
}
function AlignLeftGlyph() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        fill="currentColor"
        d="M2 3h12v1.5H2V3zm0 3h8v1.5H2V6zm0 3h12v1.5H2V9zm0 3h8v1.5H2V12z"
      />
    </svg>
  );
}
function AlignCenterGlyph() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        fill="currentColor"
        d="M2 3h12v1.5H2V3zm2 3h8v1.5H4V6zm-2 3h12v1.5H2V9zm2 3h8v1.5H4V12z"
      />
    </svg>
  );
}
function AlignRightGlyph() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        fill="currentColor"
        d="M2 3h12v1.5H2V3zm4 3h8v1.5H6V6zm-4 3h12v1.5H2V9zm4 3h8v1.5H6V12z"
      />
    </svg>
  );
}

/* ─── 1. Basic ──────────────────────────────────────────────────────── */
export const Basic: Story = {
  name: "Basic button row",
  parameters: {
    docs: {
      description: {
        story:
          "Plain button row. The Toolbar renders `role=\"toolbar\"` + " +
          "`aria-orientation=\"horizontal\"`; arrow-left/right roves between " +
          "items via Base UI's roving tabindex.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic toolbar">
      <Toolbar data-testid="toolbar-basic">
        <Button variant="gray">Cut</Button>
        <Button variant="gray">Copy</Button>
        <Button variant="gray">Paste</Button>
      </Toolbar>
    </div>
  ),
};

/* ─── 2. WithSeparator ──────────────────────────────────────────────── */
export const WithSeparator: Story = {
  name: "With separator",
  parameters: {
    docs: {
      description: {
        story:
          "Cluster boundaries via `Toolbar.Separator`. The separator " +
          "carries `aria-orientation=\"vertical\"` automatically (the " +
          "perpendicular of the horizontal toolbar) so AT users hear " +
          "it as a cluster break, not as a structural divider.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With separator">
      <Toolbar data-testid="toolbar-with-separator">
        <Button variant="gray">Cut</Button>
        <Button variant="gray">Copy</Button>
        <Button variant="gray">Paste</Button>
        <Toolbar.Separator data-testid="toolbar-separator-1" />
        <Button variant="gray">Undo</Button>
        <Button variant="gray">Redo</Button>
      </Toolbar>
    </div>
  ),
};

/* ─── 3. WithToggleGroup ─────────────────────────────────────────────
 *
 * Toggle.Group composed inside a Toolbar. Toggle.Group also paints
 * `role="toolbar"` for the segmented-control semantics — when nested
 * inside this Toolbar the inner group inherits the parent's roving
 * tabindex and becomes part of the same Tab stop. */
export const WithToggleGroup: Story = {
  name: "With Toggle.Group (segmented control inline)",
  parameters: {
    docs: {
      description: {
        story:
          "A segmented Toggle.Group sits inline next to plain Buttons. " +
          "The Toggle.Group's three pills share one focus stop via Base " +
          "UI's roving tabindex; the outer Toolbar's roving picks up " +
          "where the group leaves off so the entire row feels like a " +
          "single keyboard cluster.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With Toggle.Group">
      <Toolbar data-testid="toolbar-with-togglegroup">
        <Button variant="gray">Cut</Button>
        <Button variant="gray">Copy</Button>
        <Toolbar.Separator />
        <Toggle.Group
          defaultValue="center"
          aria-label="Text alignment"
          data-testid="toolbar-togglegroup"
        >
          <Toggle value="left" aria-label="Align left">
            <AlignLeftGlyph />
          </Toggle>
          <Toggle value="center" aria-label="Align center">
            <AlignCenterGlyph />
          </Toggle>
          <Toggle value="right" aria-label="Align right">
            <AlignRightGlyph />
          </Toggle>
        </Toggle.Group>
      </Toolbar>
    </div>
  ),
};

/* ─── 4. WithIconButtons ────────────────────────────────────────────── */
export const WithIconButtons: Story = {
  name: "With icon-only buttons",
  parameters: {
    docs: {
      description: {
        story:
          "Icon-only Buttons inside a Toolbar. Each Button carries an " +
          "`aria-label` so the icon-only affordance reads the same to " +
          "AT users as it does visually.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With icon-only buttons">
      <Toolbar data-testid="toolbar-with-icon-buttons">
        <Button variant="gray" aria-label="Bold">
          <BoldGlyph />
        </Button>
        <Button variant="gray" aria-label="Italic">
          <ItalicGlyph />
        </Button>
        <Button variant="gray" aria-label="Underline">
          <UnderlineGlyph />
        </Button>
      </Toolbar>
    </div>
  ),
};

/* ─── 5. Vertical ───────────────────────────────────────────────────── */
export const Vertical: Story = {
  name: "Vertical orientation",
  parameters: {
    docs: {
      description: {
        story:
          "`orientation=\"vertical\"` stacks the items block-wise. The " +
          "Toolbar reports `aria-orientation=\"vertical\"`; arrow-up/down " +
          "roves between items (instead of arrow-left/right). Separators " +
          "flip to a horizontal hairline automatically.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Vertical toolbar">
      <Toolbar
        orientation="vertical"
        data-testid="toolbar-vertical"
      >
        <Button variant="gray" aria-label="Bold">
          <BoldGlyph />
        </Button>
        <Button variant="gray" aria-label="Italic">
          <ItalicGlyph />
        </Button>
        <Toolbar.Separator />
        <Button variant="gray" aria-label="Align left">
          <AlignLeftGlyph />
        </Button>
        <Button variant="gray" aria-label="Align center">
          <AlignCenterGlyph />
        </Button>
      </Toolbar>
    </div>
  ),
};

/* ─── 6. WithGroups ─────────────────────────────────────────────────
 *
 * Two logical clusters separated by Toolbar.Separator. This is the
 * canonical rich-text toolbar shape: format cluster on the left,
 * alignment cluster on the right. */
export const WithGroups: Story = {
  name: "With logical groups",
  parameters: {
    docs: {
      description: {
        story:
          "Two logical clusters — format vs alignment — separated by " +
          "`Toolbar.Separator`. The roving Tab stop still treats the " +
          "whole row as one cluster; separators are visual + a11y hints " +
          "only.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With groups">
      <Toolbar data-testid="toolbar-with-groups">
        <Toggle.Group
          multiple
          defaultValue={["bold"]}
          aria-label="Text formatting"
          equalWidth={false}
        >
          <Toggle value="bold" aria-label="Bold">
            <BoldGlyph />
          </Toggle>
          <Toggle value="italic" aria-label="Italic">
            <ItalicGlyph />
          </Toggle>
          <Toggle value="underline" aria-label="Underline">
            <UnderlineGlyph />
          </Toggle>
        </Toggle.Group>
        <Toolbar.Separator />
        <Toggle.Group
          defaultValue="left"
          aria-label="Text alignment"
          equalWidth={false}
        >
          <Toggle value="left" aria-label="Align left">
            <AlignLeftGlyph />
          </Toggle>
          <Toggle value="center" aria-label="Align center">
            <AlignCenterGlyph />
          </Toggle>
          <Toggle value="right" aria-label="Align right">
            <AlignRightGlyph />
          </Toggle>
        </Toggle.Group>
      </Toolbar>
    </div>
  ),
};

/* ─── 7. Disabled ───────────────────────────────────────────────────── */
export const Disabled: Story = {
  name: "Disabled toolbar",
  parameters: {
    docs: {
      description: {
        story:
          "`disabled` on the root disables every child Button / Toggle. " +
          "Base UI emits `aria-disabled` on the disabled descendants and " +
          "skips them in roving-tabindex navigation.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled toolbar">
      <Toolbar disabled data-testid="toolbar-disabled">
        <Button variant="gray" disabled>
          Cut
        </Button>
        <Button variant="gray" disabled>
          Copy
        </Button>
        <Toolbar.Separator />
        <Button variant="gray" disabled>
          Undo
        </Button>
      </Toolbar>
    </div>
  ),
};

/* ─── 8. Rtl ────────────────────────────────────────────────────────── */
export const Rtl: Story = {
  name: "RTL — right-to-left direction",
  parameters: {
    docs: {
      description: {
        story:
          "Wrap in `dir=\"rtl\"`. Flex flow mirrors and Base UI's roving " +
          "keymap flips arrow-left/right accordingly so the cluster reads " +
          "natively in RTL locales.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="RTL toolbar"
      dir="rtl"
    >
      <Toolbar data-testid="toolbar-rtl">
        <Button variant="gray">قص</Button>
        <Button variant="gray">نسخ</Button>
        <Button variant="gray">لصق</Button>
        <Toolbar.Separator />
        <Button variant="gray">تراجع</Button>
        <Button variant="gray">إعادة</Button>
      </Toolbar>
    </div>
  ),
};
