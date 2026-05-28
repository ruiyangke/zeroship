import type { Meta, StoryObj } from "@storybook/react";
import { useState } from "react";
import { Menu } from "@base-ui/react/menu";
import { Menubar } from "../components";

/* ─── Menu wiring note ──────────────────────────────────────────────
 *
 * Menubar wraps `Menu.Root` siblings. Slice 11 ships our wrapped
 * `@zeroship/ui` Menu component; until then these stories compose Base
 * UI's `Menu.*` namespace directly with the `.zs-menubar-menu*` classes
 * declared in Menubar.css. The composition pattern is identical either
 * way — Menubar only enforces structure at the root level.
 *
 * When Slice 11 lands, the imports above flip to
 * `import { Menu } from "../components";` and the className props on
 * Menu.Trigger/Popup/Item disappear (Menu.* will apply the styling
 * automatically). The Menubar wrapper itself does not change. */

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
function ChevronRightGlyph() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        d="M5.5 3l5 5-5 5"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/* ─── helpers for the repeated Menu structure ──────────────────────── */
function MenuTrigger({
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
      className="zs-menubar-trigger"
      data-testid={testId}
      disabled={disabled}
    >
      {label}
    </Menu.Trigger>
  );
}

function MenuPopupShell({
  testId,
  children,
}: {
  testId?: string;
  children: React.ReactNode;
}) {
  return (
    <Menu.Portal>
      <Menu.Positioner className="zs-menubar-menu-positioner" sideOffset={6}>
        <Menu.Popup
          className="zs-menubar-menu"
          data-testid={testId}
        >
          {children}
        </Menu.Popup>
      </Menu.Positioner>
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
        <Menu.Root>
          <MenuTrigger label="File" testId="menubar-basic-file" />
          <MenuPopupShell testId="menubar-basic-file-popup">
            <Menu.Item className="zs-menubar-menu-item">New</Menu.Item>
            <Menu.Item className="zs-menubar-menu-item">Open…</Menu.Item>
            <Menu.Item className="zs-menubar-menu-item">Save</Menu.Item>
            <Menu.Separator className="zs-menubar-menu-separator" />
            <Menu.Item className="zs-menubar-menu-item">Quit</Menu.Item>
          </MenuPopupShell>
        </Menu.Root>

        <Menu.Root>
          <MenuTrigger label="Edit" testId="menubar-basic-edit" />
          <MenuPopupShell testId="menubar-basic-edit-popup">
            <Menu.Item className="zs-menubar-menu-item">Undo</Menu.Item>
            <Menu.Item className="zs-menubar-menu-item">Redo</Menu.Item>
            <Menu.Separator className="zs-menubar-menu-separator" />
            <Menu.Item className="zs-menubar-menu-item">Cut</Menu.Item>
            <Menu.Item className="zs-menubar-menu-item">Copy</Menu.Item>
            <Menu.Item className="zs-menubar-menu-item">Paste</Menu.Item>
          </MenuPopupShell>
        </Menu.Root>

        <Menu.Root>
          <MenuTrigger label="View" testId="menubar-basic-view" />
          <MenuPopupShell testId="menubar-basic-view-popup">
            <Menu.Item className="zs-menubar-menu-item">Zoom in</Menu.Item>
            <Menu.Item className="zs-menubar-menu-item">Zoom out</Menu.Item>
            <Menu.Item className="zs-menubar-menu-item">
              Reset zoom
            </Menu.Item>
          </MenuPopupShell>
        </Menu.Root>
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
          "Menu items can nest submenus via `Menu.SubmenuRoot` + " +
          "`Menu.SubmenuTrigger`. Hovering the parent item opens the " +
          "submenu after a small delay; arrow-right opens it via the " +
          "keyboard, arrow-left closes it.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With submenus">
      <Menubar data-testid="menubar-submenus">
        <Menu.Root>
          <MenuTrigger label="File" testId="menubar-submenus-file" />
          <MenuPopupShell testId="menubar-submenus-file-popup">
            <Menu.Item className="zs-menubar-menu-item">New</Menu.Item>
            <Menu.SubmenuRoot>
              <Menu.SubmenuTrigger
                className="zs-menubar-menu-item"
                data-testid="menubar-submenus-recent"
              >
                Open recent
                <span style={{ marginInlineStart: "auto" }}>
                  <ChevronRightGlyph />
                </span>
              </Menu.SubmenuTrigger>
              <Menu.Portal>
                <Menu.Positioner
                  className="zs-menubar-menu-positioner"
                  sideOffset={6}
                >
                  <Menu.Popup
                    className="zs-menubar-menu"
                    data-testid="menubar-submenus-recent-popup"
                  >
                    <Menu.Item className="zs-menubar-menu-item">
                      project-alpha
                    </Menu.Item>
                    <Menu.Item className="zs-menubar-menu-item">
                      builder.zship
                    </Menu.Item>
                    <Menu.Item className="zs-menubar-menu-item">
                      sandbox.zship
                    </Menu.Item>
                  </Menu.Popup>
                </Menu.Positioner>
              </Menu.Portal>
            </Menu.SubmenuRoot>
            <Menu.Separator className="zs-menubar-menu-separator" />
            <Menu.Item className="zs-menubar-menu-item">Quit</Menu.Item>
          </MenuPopupShell>
        </Menu.Root>
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
          <Menu.Root>
            <MenuTrigger label="View" testId="menubar-checkbox-view" />
            <MenuPopupShell testId="menubar-checkbox-view-popup">
              <Menu.CheckboxItem
                className="zs-menubar-menu-item"
                checked={showRuler}
                onCheckedChange={setShowRuler}
                data-testid="menubar-checkbox-ruler"
              >
                <Menu.CheckboxItemIndicator className="zs-menubar-menu-indicator">
                  <CheckGlyph />
                </Menu.CheckboxItemIndicator>
                Show ruler
              </Menu.CheckboxItem>
              <Menu.CheckboxItem
                className="zs-menubar-menu-item"
                checked={showGrid}
                onCheckedChange={setShowGrid}
                data-testid="menubar-checkbox-grid"
              >
                <Menu.CheckboxItemIndicator className="zs-menubar-menu-indicator">
                  <CheckGlyph />
                </Menu.CheckboxItemIndicator>
                Show grid
              </Menu.CheckboxItem>
            </MenuPopupShell>
          </Menu.Root>
        </Menubar>
      </div>
    );
  },
};

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
          <Menu.Root>
            <MenuTrigger label="Theme" testId="menubar-radio-theme" />
            <MenuPopupShell testId="menubar-radio-theme-popup">
              <Menu.RadioGroup value={theme} onValueChange={setTheme}>
                <Menu.RadioItem
                  value="light"
                  className="zs-menubar-menu-item"
                  data-testid="menubar-radio-light"
                >
                  <Menu.RadioItemIndicator className="zs-menubar-menu-indicator">
                    <DotGlyph />
                  </Menu.RadioItemIndicator>
                  Light
                </Menu.RadioItem>
                <Menu.RadioItem
                  value="dark"
                  className="zs-menubar-menu-item"
                  data-testid="menubar-radio-dark"
                >
                  <Menu.RadioItemIndicator className="zs-menubar-menu-indicator">
                    <DotGlyph />
                  </Menu.RadioItemIndicator>
                  Dark
                </Menu.RadioItem>
                <Menu.RadioItem
                  value="system"
                  className="zs-menubar-menu-item"
                  data-testid="menubar-radio-system"
                >
                  <Menu.RadioItemIndicator className="zs-menubar-menu-indicator">
                    <DotGlyph />
                  </Menu.RadioItemIndicator>
                  System
                </Menu.RadioItem>
              </Menu.RadioGroup>
            </MenuPopupShell>
          </Menu.Root>
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
        <Menu.Root>
          <MenuTrigger label="File" testId="menubar-keyboard-file" />
          <MenuPopupShell testId="menubar-keyboard-file-popup">
            <Menu.Item className="zs-menubar-menu-item">New</Menu.Item>
            <Menu.Item className="zs-menubar-menu-item">Open</Menu.Item>
          </MenuPopupShell>
        </Menu.Root>
        <Menu.Root>
          <MenuTrigger label="Edit" testId="menubar-keyboard-edit" />
          <MenuPopupShell testId="menubar-keyboard-edit-popup">
            <Menu.Item className="zs-menubar-menu-item">Undo</Menu.Item>
            <Menu.Item className="zs-menubar-menu-item">Redo</Menu.Item>
          </MenuPopupShell>
        </Menu.Root>
        <Menu.Root>
          <MenuTrigger label="Help" testId="menubar-keyboard-help" />
          <MenuPopupShell testId="menubar-keyboard-help-popup">
            <Menu.Item className="zs-menubar-menu-item">Docs</Menu.Item>
            <Menu.Item className="zs-menubar-menu-item">About</Menu.Item>
          </MenuPopupShell>
        </Menu.Root>
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
        <Menu.Root>
          <MenuTrigger label="File" testId="menubar-disabled-file" />
          <MenuPopupShell testId="menubar-disabled-file-popup">
            <Menu.Item className="zs-menubar-menu-item">New</Menu.Item>
            <Menu.Item className="zs-menubar-menu-item">Open</Menu.Item>
          </MenuPopupShell>
        </Menu.Root>
        <Menu.Root>
          <MenuTrigger
            label="Edit"
            testId="menubar-disabled-edit"
            disabled
          />
          <MenuPopupShell>
            <Menu.Item className="zs-menubar-menu-item">Undo</Menu.Item>
          </MenuPopupShell>
        </Menu.Root>
        <Menu.Root>
          <MenuTrigger label="Help" testId="menubar-disabled-help" />
          <MenuPopupShell testId="menubar-disabled-help-popup">
            <Menu.Item className="zs-menubar-menu-item">Docs</Menu.Item>
          </MenuPopupShell>
        </Menu.Root>
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
          ".zs-menubar-menu-item so spacing stays consistent.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With icons">
      <Menubar data-testid="menubar-icons">
        <Menu.Root>
          <MenuTrigger label="File" testId="menubar-icons-file" />
          <MenuPopupShell testId="menubar-icons-file-popup">
            <Menu.Item className="zs-menubar-menu-item">
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
            <Menu.Item className="zs-menubar-menu-item">
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
          </MenuPopupShell>
        </Menu.Root>
      </Menubar>
    </div>
  ),
};

/* ─── 8. Rtl ───────────────────────────────────────────────────────── */
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
        <Menu.Root>
          <MenuTrigger label="ملف" testId="menubar-rtl-file" />
          <MenuPopupShell testId="menubar-rtl-file-popup">
            <Menu.Item className="zs-menubar-menu-item">جديد</Menu.Item>
            <Menu.Item className="zs-menubar-menu-item">فتح…</Menu.Item>
            <Menu.Item className="zs-menubar-menu-item">حفظ</Menu.Item>
          </MenuPopupShell>
        </Menu.Root>
        <Menu.Root>
          <MenuTrigger label="تحرير" testId="menubar-rtl-edit" />
          <MenuPopupShell testId="menubar-rtl-edit-popup">
            <Menu.Item className="zs-menubar-menu-item">تراجع</Menu.Item>
            <Menu.Item className="zs-menubar-menu-item">إعادة</Menu.Item>
          </MenuPopupShell>
        </Menu.Root>
      </Menubar>
    </div>
  ),
};
