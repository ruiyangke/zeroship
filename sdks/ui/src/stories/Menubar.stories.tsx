import type { Meta, StoryObj } from "@storybook/react";
import { useState } from "react";
import { Menu, Menubar } from "../components";

/* ─── Menu wiring note ──────────────────────────────────────────────
 *
 * Menubar wraps `Menu` siblings from `@zeroship/ui`. Each `Menu.Trigger`
 * inside a Menubar carries `data-chrome="menubar"` so the menubar-flavored
 * trigger paint (defined in Menubar.css) kicks in without leaking into
 * stand-alone Menus elsewhere on the page. The popup chrome itself is
 * owned by Menu.css — Menubar instances pass `data-chrome="menubar"` on
 * their `Menu.Popup` so Menu.css can scope any menubar-specific deltas
 * off that attribute (rather than duplicating the popup CSS in
 * Menubar.css). */

const meta: Meta<typeof Menubar> = {
  title: "Components/Menubar",
  component: Menubar,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Menubar>;

/* ─── glyphs ────────────────────────────────────────────────────────── */
function CheckGlyph() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        fill="currentColor"
        d="M6.4 10.9L3.7 8.2l-1 1L6.4 13l8-8-1-1z"
      />
    </svg>
  );
}
function DotGlyph() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <circle cx="8" cy="8" r="3" fill="currentColor" />
    </svg>
  );
}
function FolderGlyph() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        fill="currentColor"
        d="M2 3.5A1.5 1.5 0 0 1 3.5 2H7l1.5 1.5h4A1.5 1.5 0 0 1 14 5v7.5A1.5 1.5 0 0 1 12.5 14h-9A1.5 1.5 0 0 1 2 12.5v-9z"
      />
    </svg>
  );
}

/* ─── helpers for the repeated Menu structure ──────────────────────── */
function MenubarMenuTrigger({
  label,
  testId,
  disabled,
}: {
  label: string;
  testId?: string;
  disabled?: boolean;
}) {
  return (
    <Menu.Trigger
      data-chrome="menubar"
      data-testid={testId}
      disabled={disabled}
    >
      {label}
    </Menu.Trigger>
  );
}

function MenubarMenuPopup({
  testId,
  children,
}: {
  testId?: string;
  children: React.ReactNode;
}) {
  return (
    <Menu.Portal>
      <Menu.Popup data-chrome="menubar" data-testid={testId} sideOffset={6}>
        {children}
      </Menu.Popup>
    </Menu.Portal>
  );
}

/* ─── 1. Basic File/Edit/View ──────────────────────────────────────── */
export const Basic: Story = {
  name: "Basic File / Edit / View",
  parameters: {
    docs: {
      description: {
        story:
          "Three Menu siblings as the canonical macOS-style strip. Click " +
          "any trigger to open its menu; once one is open, hovering an " +
          "adjacent trigger swaps the menu without an intermediate click " +
          "(Base UI's auto-open-on-hover-after-first-click).",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic menubar">
      <Menubar data-testid="menubar-basic">
        <Menu>
          <MenubarMenuTrigger label="File" testId="menubar-basic-file" />
          <MenubarMenuPopup testId="menubar-basic-file-popup">
            <Menu.Item>New</Menu.Item>
            <Menu.Item>Open…</Menu.Item>
            <Menu.Item>Save</Menu.Item>
            <Menu.Separator />
            <Menu.Item>Quit</Menu.Item>
          </MenubarMenuPopup>
        </Menu>

        <Menu>
          <MenubarMenuTrigger label="Edit" testId="menubar-basic-edit" />
          <MenubarMenuPopup testId="menubar-basic-edit-popup">
            <Menu.Item>Undo</Menu.Item>
            <Menu.Item>Redo</Menu.Item>
            <Menu.Separator />
            <Menu.Item>Cut</Menu.Item>
            <Menu.Item>Copy</Menu.Item>
            <Menu.Item>Paste</Menu.Item>
          </MenubarMenuPopup>
        </Menu>

        <Menu>
          <MenubarMenuTrigger label="View" testId="menubar-basic-view" />
          <MenubarMenuPopup testId="menubar-basic-view-popup">
            <Menu.Item>Zoom in</Menu.Item>
            <Menu.Item>Zoom out</Menu.Item>
            <Menu.Item>Reset zoom</Menu.Item>
          </MenubarMenuPopup>
        </Menu>
      </Menubar>
    </div>
  ),
};

/* ─── 2. WithSubmenus ──────────────────────────────────────────────── */
export const WithSubmenus: Story = {
  name: "With submenus",
  parameters: {
    docs: {
      description: {
        story:
          "Menu items can nest submenus via `Menu.Submenu`. Hovering the " +
          "parent item opens the submenu after a small delay; arrow-right " +
          "opens it via the keyboard, arrow-left closes it.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With submenus">
      <Menubar data-testid="menubar-submenus">
        <Menu>
          <MenubarMenuTrigger label="File" testId="menubar-submenus-file" />
          <MenubarMenuPopup testId="menubar-submenus-file-popup">
            <Menu.Item>New</Menu.Item>
            <Menu.Submenu
              trigger="Open recent"
              data-testid="menubar-submenus-recent"
            >
              <Menu.Item>project-alpha</Menu.Item>
              <Menu.Item>builder.zship</Menu.Item>
              <Menu.Item>sandbox.zship</Menu.Item>
            </Menu.Submenu>
            <Menu.Separator />
            <Menu.Item>Quit</Menu.Item>
          </MenubarMenuPopup>
        </Menu>
      </Menubar>
    </div>
  ),
};

/* ─── 3. WithCheckboxItem ──────────────────────────────────────────── */
export const WithCheckboxItem: Story = {
  name: "With checkbox items",
  parameters: {
    docs: {
      description: {
        story:
          "View / Settings menus often expose toggleable preferences as " +
          "`Menu.CheckboxItem`. The checkmark paints inside " +
          "`Menu.CheckboxItemIndicator` whenever the item is checked. " +
          "Base UI emits `role=\"menuitemcheckbox\"`.",
      },
    },
  },
  render: function CheckboxStory() {
    const [showRuler, setShowRuler] = useState(true);
    const [showGrid, setShowGrid] = useState(false);
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="With checkbox items"
      >
        <Menubar data-testid="menubar-checkbox">
          <Menu>
            <MenubarMenuTrigger label="View" testId="menubar-checkbox-view" />
            <MenubarMenuPopup testId="menubar-checkbox-view-popup">
              <Menu.CheckboxItem
                checked={showRuler}
                onCheckedChange={setShowRuler}
                data-testid="menubar-checkbox-ruler"
              >
                Show ruler
              </Menu.CheckboxItem>
              <Menu.CheckboxItem
                checked={showGrid}
                onCheckedChange={setShowGrid}
                data-testid="menubar-checkbox-grid"
              >
                Show grid
              </Menu.CheckboxItem>
            </MenubarMenuPopup>
          </Menu>
        </Menubar>
      </div>
    );
  },
};
void CheckGlyph;
void DotGlyph;

/* ─── 4. WithRadioGroup ────────────────────────────────────────────── */
export const WithRadioGroup: Story = {
  name: "With radio group",
  parameters: {
    docs: {
      description: {
        story:
          "Mutually exclusive selections via `Menu.RadioGroup` + " +
          "`Menu.RadioItem`. Base UI emits `role=\"menuitemradio\"` and " +
          "manages the single-selection state.",
      },
    },
  },
  render: function RadioStory() {
    const [theme, setTheme] = useState<string>("system");
    return (
      <div className="zs-story-row" role="group" aria-label="With radio group">
        <Menubar data-testid="menubar-radio">
          <Menu>
            <MenubarMenuTrigger label="Theme" testId="menubar-radio-theme" />
            <MenubarMenuPopup testId="menubar-radio-theme-popup">
              <Menu.RadioGroup value={theme} onValueChange={setTheme}>
                <Menu.RadioItem
                  value="light"
                  data-testid="menubar-radio-light"
                >
                  Light
                </Menu.RadioItem>
                <Menu.RadioItem
                  value="dark"
                  data-testid="menubar-radio-dark"
                >
                  Dark
                </Menu.RadioItem>
                <Menu.RadioItem
                  value="system"
                  data-testid="menubar-radio-system"
                >
                  System
                </Menu.RadioItem>
              </Menu.RadioGroup>
            </MenubarMenuPopup>
          </Menu>
        </Menubar>
      </div>
    );
  },
};

/* ─── 5. KeyboardNav ───────────────────────────────────────────────── */
export const KeyboardNav: Story = {
  name: "Keyboard navigation",
  parameters: {
    docs: {
      description: {
        story:
          "Tab focuses the menubar's first trigger. ArrowRight roves to " +
          "the next trigger (loops to the first at the end); ArrowDown " +
          "opens the focused menu and lands on its first item. ArrowUp " +
          "closes the open menu and returns focus to the trigger.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Keyboard navigation">
      <Menubar data-testid="menubar-keyboard">
        <Menu>
          <MenubarMenuTrigger label="File" testId="menubar-keyboard-file" />
          <MenubarMenuPopup testId="menubar-keyboard-file-popup">
            <Menu.Item>New</Menu.Item>
            <Menu.Item>Open</Menu.Item>
          </MenubarMenuPopup>
        </Menu>
        <Menu>
          <MenubarMenuTrigger label="Edit" testId="menubar-keyboard-edit" />
          <MenubarMenuPopup testId="menubar-keyboard-edit-popup">
            <Menu.Item>Undo</Menu.Item>
            <Menu.Item>Redo</Menu.Item>
          </MenubarMenuPopup>
        </Menu>
        <Menu>
          <MenubarMenuTrigger label="Help" testId="menubar-keyboard-help" />
          <MenubarMenuPopup testId="menubar-keyboard-help-popup">
            <Menu.Item>Docs</Menu.Item>
            <Menu.Item>About</Menu.Item>
          </MenubarMenuPopup>
        </Menu>
      </Menubar>
    </div>
  ),
};

/* ─── 6. Disabled ──────────────────────────────────────────────────── */
export const Disabled: Story = {
  name: "Disabled trigger",
  parameters: {
    docs: {
      description: {
        story:
          "`disabled` on a `Menu.Trigger` makes that menu unopenable. " +
          "Roving navigation skips it; AT users hear it as disabled.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled trigger">
      <Menubar data-testid="menubar-disabled">
        <Menu>
          <MenubarMenuTrigger label="File" testId="menubar-disabled-file" />
          <MenubarMenuPopup testId="menubar-disabled-file-popup">
            <Menu.Item>New</Menu.Item>
            <Menu.Item>Open</Menu.Item>
          </MenubarMenuPopup>
        </Menu>
        <Menu>
          <MenubarMenuTrigger
            label="Edit"
            testId="menubar-disabled-edit"
            disabled
          />
          <MenubarMenuPopup>
            <Menu.Item>Undo</Menu.Item>
          </MenubarMenuPopup>
        </Menu>
        <Menu>
          <MenubarMenuTrigger label="Help" testId="menubar-disabled-help" />
          <MenubarMenuPopup testId="menubar-disabled-help-popup">
            <Menu.Item>Docs</Menu.Item>
          </MenubarMenuPopup>
        </Menu>
      </Menubar>
    </div>
  ),
};

/* ─── 7. WithIcons ─────────────────────────────────────────────────── */
export const WithIcons: Story = {
  name: "With icons",
  parameters: {
    docs: {
      description: {
        story:
          "Menu items can carry a leading icon for a stronger affordance. " +
          "Icons sit inside the item as a child; the gap is owned by " +
          ".zs-menu-item so spacing stays consistent.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With icons">
      <Menubar data-testid="menubar-icons">
        <Menu>
          <MenubarMenuTrigger label="File" testId="menubar-icons-file" />
          <MenubarMenuPopup testId="menubar-icons-file-popup">
            <Menu.Item>
              <span
                aria-hidden="true"
                style={{
                  display: "inline-flex",
                  inlineSize: "1rem",
                  blockSize: "1rem",
                }}
              >
                <FolderGlyph />
              </span>
              Open project…
            </Menu.Item>
            <Menu.Item>
              <span
                aria-hidden="true"
                style={{
                  display: "inline-flex",
                  inlineSize: "1rem",
                  blockSize: "1rem",
                }}
              >
                <FolderGlyph />
              </span>
              Open recent…
            </Menu.Item>
          </MenubarMenuPopup>
        </Menu>
      </Menubar>
    </div>
  ),
};

/* ─── 8. RoleLockRegression — Slice 12 review fix #3 ───────────────── *
 *
 * Defensive coverage for the `role` lock. The public type
 * `Omit<…, "role">` already rejects a literal `<Menubar role="…">`, but
 * a caller could still bypass via a `{...untypedProps}` spread. The
 * Menubar wrapper strips `role` at runtime so the rendered element is
 * always `role="menubar"`. The aria-wiring runner checks the rendered
 * DOM stays `role="menubar"`. */
export const RoleLockRegression: Story = {
  name: "Role lock — caller-passed role is stripped",
  parameters: {
    docs: {
      description: {
        story:
          "Regression for Slice 12 review fix #3. The Menubar wrapper " +
          "strips a user-passed `role` at runtime so a `{...spread}` " +
          "injection cannot override `role=\"menubar\"`.",
      },
    },
  },
  render: () => {
    const bypass = { role: "presentation" } as Record<string, unknown>;
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Menubar role lock regression"
      >
        <Menubar data-testid="menubar-role-lock" {...bypass}>
          <Menu>
            <MenubarMenuTrigger label="File" />
            <MenubarMenuPopup>
              <Menu.Item>New</Menu.Item>
            </MenubarMenuPopup>
          </Menu>
        </Menubar>
      </div>
    );
  },
};

/* ─── 9. Rtl ───────────────────────────────────────────────────────── */
export const Rtl: Story = {
  name: "RTL — right-to-left direction",
  parameters: {
    docs: {
      description: {
        story:
          "Wrap in `dir=\"rtl\"`. The trigger row mirrors and Base UI " +
          "anchors the popups with the start/end flipped so the menu " +
          "reads natively in RTL locales.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="RTL menubar"
      dir="rtl"
    >
      <Menubar data-testid="menubar-rtl">
        <Menu>
          <MenubarMenuTrigger label="ملف" testId="menubar-rtl-file" />
          <MenubarMenuPopup testId="menubar-rtl-file-popup">
            <Menu.Item>جديد</Menu.Item>
            <Menu.Item>فتح…</Menu.Item>
            <Menu.Item>حفظ</Menu.Item>
          </MenubarMenuPopup>
        </Menu>
        <Menu>
          <MenubarMenuTrigger label="تحرير" testId="menubar-rtl-edit" />
          <MenubarMenuPopup testId="menubar-rtl-edit-popup">
            <Menu.Item>تراجع</Menu.Item>
            <Menu.Item>إعادة</Menu.Item>
          </MenubarMenuPopup>
        </Menu>
      </Menubar>
    </div>
  ),
};
