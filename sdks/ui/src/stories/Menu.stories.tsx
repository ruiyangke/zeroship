import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react";
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
          "kbd hint at the trailing edge of each row. The hint is " +
          "presentational — Base UI's text-navigation matches the row's " +
          "label, not its shortcut text.",
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
            <Menu.Item>
              <span
                style={{
                  display: "flex",
                  justifyContent: "space-between",
                  inlineSize: "100%",
                  gap: "1rem",
                }}
              >
                <span>Cut</span>
                <kbd
                  style={{
                    fontFamily: "var(--zs-font-mono)",
                    color: "var(--zs-label-secondary)",
                    fontSize: "0.8125rem",
                  }}
                >
                  ⌘X
                </kbd>
              </span>
            </Menu.Item>
            <Menu.Item>
              <span
                style={{
                  display: "flex",
                  justifyContent: "space-between",
                  inlineSize: "100%",
                  gap: "1rem",
                }}
              >
                <span>Copy</span>
                <kbd
                  style={{
                    fontFamily: "var(--zs-font-mono)",
                    color: "var(--zs-label-secondary)",
                    fontSize: "0.8125rem",
                  }}
                >
                  ⌘C
                </kbd>
              </span>
            </Menu.Item>
            <Menu.Item>
              <span
                style={{
                  display: "flex",
                  justifyContent: "space-between",
                  inlineSize: "100%",
                  gap: "1rem",
                }}
              >
                <span>Paste</span>
                <kbd
                  style={{
                    fontFamily: "var(--zs-font-mono)",
                    color: "var(--zs-label-secondary)",
                    fontSize: "0.8125rem",
                  }}
                >
                  ⌘V
                </kbd>
              </span>
            </Menu.Item>
          </Menu.Popup>
        </Menu.Portal>
      </Menu>
    </div>
  ),
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
            <Menu.Submenu trigger="مشاركة مع…">
              <Menu.Item>البريد الإلكتروني</Menu.Item>
              <Menu.Item>نسخ الرابط</Menu.Item>
            </Menu.Submenu>
            <Menu.Separator />
            <Menu.Item>حذف</Menu.Item>
          </Menu.Popup>
        </Menu.Portal>
      </Menu>
    </div>
  ),
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
