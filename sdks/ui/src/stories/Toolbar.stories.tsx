import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { Toggle, Toolbar } from "../components";

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
          "Plain `Toolbar.Button` row. The Toolbar renders " +
          "`role=\"toolbar\"` + `aria-orientation=\"horizontal\"`; " +
          "arrow-left/right roves between items via Base UI's roving " +
          "tabindex. Use `Toolbar.Button` (not the bare `Button` from " +
          "the package) so each item registers with Base UI's composite " +
          "context — that's what enables roving and disabled-skip.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic toolbar">
      <Toolbar data-testid="toolbar-basic">
        <Toolbar.Button data-testid="toolbar-basic-cut">Cut</Toolbar.Button>
        <Toolbar.Button data-testid="toolbar-basic-copy">Copy</Toolbar.Button>
        <Toolbar.Button data-testid="toolbar-basic-paste">Paste</Toolbar.Button>
      </Toolbar>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const cut = canvas.getByRole("button", { name: /cut/i });
    const copy = canvas.getByRole("button", { name: /copy/i });

    await expect(canvas.getByRole("toolbar")).toHaveAttribute(
      "aria-orientation",
      "horizontal",
    );
    await userEvent.tab();
    await expect(cut).toHaveFocus();
    await userEvent.keyboard("{ArrowRight}");
    await expect(copy).toHaveFocus();
  },
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
        <Toolbar.Button>Cut</Toolbar.Button>
        <Toolbar.Button>Copy</Toolbar.Button>
        <Toolbar.Button>Paste</Toolbar.Button>
        <Toolbar.Separator data-testid="toolbar-separator-1" />
        <Toolbar.Button>Undo</Toolbar.Button>
        <Toolbar.Button>Redo</Toolbar.Button>
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
        <Toolbar.Button>Cut</Toolbar.Button>
        <Toolbar.Button>Copy</Toolbar.Button>
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const left = canvas.getByRole("button", { name: /align left/i });
    const center = canvas.getByRole("button", { name: /align center/i });

    await expect(center).toHaveAttribute("aria-pressed", "true");
    await userEvent.click(left);
    await waitFor(() =>
      expect(left).toHaveAttribute("aria-pressed", "true"),
    );
  },
};

/* ─── 4. WithIconButtons ────────────────────────────────────────────── */
export const WithIconButtons: Story = {
  name: "With icon-only buttons",
  parameters: {
    docs: {
      description: {
        story:
          "Icon-only `Toolbar.Button` entries. Each item carries an " +
          "`aria-label` so the icon-only affordance reads the same to " +
          "AT users as it does visually.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With icon-only buttons">
      <Toolbar data-testid="toolbar-with-icon-buttons">
        <Toolbar.Button aria-label="Bold">
          <BoldGlyph />
        </Toolbar.Button>
        <Toolbar.Button aria-label="Italic">
          <ItalicGlyph />
        </Toolbar.Button>
        <Toolbar.Button aria-label="Underline">
          <UnderlineGlyph />
        </Toolbar.Button>
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
        <Toolbar.Button aria-label="Bold">
          <BoldGlyph />
        </Toolbar.Button>
        <Toolbar.Button aria-label="Italic">
          <ItalicGlyph />
        </Toolbar.Button>
        <Toolbar.Separator />
        <Toolbar.Button aria-label="Align left">
          <AlignLeftGlyph />
        </Toolbar.Button>
        <Toolbar.Button aria-label="Align center">
          <AlignCenterGlyph />
        </Toolbar.Button>
      </Toolbar>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const bold = canvas.getByRole("button", { name: /bold/i });
    const italic = canvas.getByRole("button", { name: /italic/i });

    await expect(canvas.getByRole("toolbar")).toHaveAttribute(
      "aria-orientation",
      "vertical",
    );
    await userEvent.tab();
    await expect(bold).toHaveFocus();
    await userEvent.keyboard("{ArrowDown}");
    await expect(italic).toHaveFocus();
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const bold = canvas.getByRole("button", { name: /bold/i });
    const italic = canvas.getByRole("button", { name: /italic/i });
    const center = canvas.getByRole("button", { name: /align center/i });

    await expect(bold).toHaveAttribute("aria-pressed", "true");
    await userEvent.click(italic);
    await waitFor(() =>
      expect(italic).toHaveAttribute("aria-pressed", "true"),
    );
    await userEvent.click(center);
    await waitFor(() =>
      expect(center).toHaveAttribute("aria-pressed", "true"),
    );
  },
};

/* ─── 7. Disabled ───────────────────────────────────────────────────── */
export const Disabled: Story = {
  name: "Disabled toolbar",
  parameters: {
    docs: {
      description: {
        story:
          "`disabled` on the root disables every child `Toolbar.Button` " +
          "/ `Toggle`. Base UI emits `aria-disabled` on the disabled " +
          "descendants and skips them in roving-tabindex navigation.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled toolbar">
      <Toolbar disabled data-testid="toolbar-disabled">
        <Toolbar.Button disabled>Cut</Toolbar.Button>
        <Toolbar.Button disabled>Copy</Toolbar.Button>
        <Toolbar.Separator />
        <Toolbar.Button disabled>Undo</Toolbar.Button>
      </Toolbar>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const cut = canvas.getByRole("button", { name: /cut/i });
    const copy = canvas.getByRole("button", { name: /copy/i });

    await expect(cut).toBeDisabled();
    await userEvent.click(cut);
    await expect(cut).not.toHaveFocus();
    await userEvent.tab();
    await expect(copy).not.toHaveFocus();
  },
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
        <Toolbar.Button>قص</Toolbar.Button>
        <Toolbar.Button>نسخ</Toolbar.Button>
        <Toolbar.Button>لصق</Toolbar.Button>
        <Toolbar.Separator />
        <Toolbar.Button>تراجع</Toolbar.Button>
        <Toolbar.Button>إعادة</Toolbar.Button>
      </Toolbar>
    </div>
  ),
};

/* ─── 9. RoleLockRegression — Slice 12 review fix #3 ────────────────── *
 *
 * Defensive coverage for the `role` lock. The component's public type
 * `Omit<…, "role">` already rejects a literal `<Toolbar role="…">`, but
 * a caller could still bypass via a `{...untypedProps}` spread. The
 * Toolbar wrapper strips `role` at runtime so the rendered element is
 * always `role="toolbar"`. This story passes a `role="navigation"`
 * via a spread so the aria-wiring runner can confirm the strip
 * survives the type bypass. */
export const RoleLockRegression: Story = {
  name: "Role lock — caller-passed role is stripped",
  parameters: {
    docs: {
      description: {
        story:
          "Regression for Slice 12 review fix #3. The wrapper strips a " +
          "user-passed `role` at runtime so a `{...spread}` injection " +
          "cannot override `role=\"toolbar\"`. The aria-wiring runner " +
          "checks the rendered DOM stays `role=\"toolbar\"`.",
      },
    },
  },
  render: () => {
    // Type-bypass via a spread. The `Omit<…, "role">` rejects a direct
    // `<Toolbar role="navigation">`, so we route through an untyped
    // bag. The wrapper's runtime strip MUST drop this before forward.
    const bypass = { role: "navigation" } as Record<string, unknown>;
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Role lock regression"
      >
        <Toolbar data-testid="toolbar-role-lock" {...bypass}>
          <Toolbar.Button>Cut</Toolbar.Button>
          <Toolbar.Button>Copy</Toolbar.Button>
        </Toolbar>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByRole("toolbar")).toHaveAttribute(
      "role",
      "toolbar",
    );
  },
};

/* ─── 10. Roving — regression for Slice 12 review fix #1 ────────────── *
 *
 * Story dedicated to the roving-tabindex contract. Tab enters at the
 * first focusable item; ArrowRight advances and SKIPS the disabled
 * item; Tab leaves the cluster. The aria-wiring runner drives this
 * story directly so the assertion fails pre-fix (when plain `Button`
 * children break the composite-item registration). */
export const Roving: Story = {
  name: "Roving tabindex + disabled skip",
  parameters: {
    docs: {
      description: {
        story:
          "Regression coverage for the roving-tabindex contract: Tab " +
          "lands on the first item, ArrowRight advances and skips the " +
          "disabled item, then Tab leaves the cluster. Built with " +
          "`Toolbar.Button` so each item registers as a composite item " +
          "with Base UI's roving context.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Roving toolbar">
      <button type="button" data-testid="toolbar-roving-before">
        Before
      </button>
      <Toolbar data-testid="toolbar-roving">
        <Toolbar.Button data-testid="toolbar-roving-cut">Cut</Toolbar.Button>
        <Toolbar.Button
          data-testid="toolbar-roving-copy"
          disabled
          focusableWhenDisabled={false}
        >
          Copy
        </Toolbar.Button>
        <Toolbar.Button data-testid="toolbar-roving-paste">Paste</Toolbar.Button>
      </Toolbar>
      <button type="button" data-testid="toolbar-roving-after">
        After
      </button>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const before = canvas.getByRole("button", { name: /before/i });
    const cut = canvas.getByRole("button", { name: /cut/i });
    const paste = canvas.getByRole("button", { name: /paste/i });
    const after = canvas.getByRole("button", { name: /after/i });

    before.focus();
    await userEvent.tab();
    await expect(cut).toHaveFocus();
    await userEvent.keyboard("{ArrowRight}");
    await expect(paste).toHaveFocus();
    await userEvent.tab();
    await expect(after).toHaveFocus();
  },
};

/* ─── 11. LinkAndInput ─────────────────────────────────────────────── */
export const LinkAndInput: Story = {
  name: "Link and input items",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Toolbar link and input"
    >
      <Toolbar loopFocus={false}>
        <Toolbar.Button>Refresh</Toolbar.Button>
        <Toolbar.Link href="https://example.com/docs">Docs</Toolbar.Link>
        <Toolbar.Input aria-label="Filter rows" placeholder="Filter" />
      </Toolbar>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const refresh = canvas.getByRole("button", { name: /refresh/i });
    const docs = canvas.getByRole("link", { name: /docs/i });
    const filter = canvas.getByRole("textbox", { name: /filter rows/i });

    await expect(docs).toHaveAttribute("href", "https://example.com/docs");
    await userEvent.tab();
    await expect(refresh).toHaveFocus();
    await userEvent.keyboard("{ArrowRight}{ArrowRight}");
    await expect(filter).toHaveFocus();
    await userEvent.type(filter, "deploys");
    await expect(filter).toHaveValue("deploys");
  },
};

/* ─── 13. AriaOrientationLockRegression — Round 5 fix #4 ─────────────── *
 *
 * Regression for Round 5 fix #4. The Toolbar root LOCKS
 * `aria-orientation` to the value Base UI computes from the `orientation`
 * prop. Pre-fix, a caller-passed `aria-orientation` could leak through
 * Base UI's `mergeProps` (rightmost-wins) and contradict the resolved
 * orientation. The wrapper now omits `aria-orientation` at the type
 * level AND strips it at runtime so a typed-bypass spread cannot ship
 * the inconsistency. */
export const AriaOrientationLockRegression: Story = {
  name: "aria-orientation lock — caller override is stripped",
  parameters: {
    docs: {
      description: {
        story:
          "Pass `aria-orientation=\"vertical\"` via untyped spread on a " +
          "horizontal toolbar. The wrapper's runtime strip MUST keep the " +
          "rendered `aria-orientation` equal to `\"horizontal\"`.",
      },
    },
  },
  render: () => {
    const bypass = { "aria-orientation": "vertical" } as Record<
      string,
      unknown
    >;
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="aria-orientation lock regression"
      >
        <Toolbar
          data-testid="toolbar-aria-orientation-lock"
          orientation="horizontal"
          {...bypass}
        >
          <Toolbar.Button>Cut</Toolbar.Button>
          <Toolbar.Button>Copy</Toolbar.Button>
        </Toolbar>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const toolbar = canvas.getByRole("toolbar");
    await expect(toolbar).toHaveAttribute("aria-orientation", "horizontal");
  },
};

/* ─── 14. SeparatorOrientationLockRegression — Wave 7 fix #3 ─────────── *
 *
 * Regression for Wave 7 fix #3. `Toolbar.Separator` LOCKS its
 * `orientation` / `role` / `aria-orientation` to the perpendicular of
 * the parent Toolbar axis. Pre-fix, `ToolbarSeparatorProps` was a bare
 * alias of Base UI's separator props and forwarded everything; a
 * caller could pass `orientation="horizontal"` (or `role="navigation"`
 * / `aria-orientation="horizontal"`) inside a horizontal toolbar and
 * Base UI's spread-after-defaults pattern would propagate the
 * override — the rendered DOM would carry the wrong axis. The wrapper
 * now omits all three at the type layer AND strips them at runtime,
 * so a typed-bypass spread cannot ship the inconsistency.
 *
 * Inside a horizontal toolbar, the correct separator carries
 * `aria-orientation="vertical"` (the perpendicular). We try to spread
 * `orientation="horizontal"`, `role="navigation"`, and
 * `aria-orientation="horizontal"` via an untyped bag. The runtime
 * strip MUST drop them so the rendered separator stays
 * `role="separator"` + `aria-orientation="vertical"`. */
export const SeparatorOrientationLockRegression: Story = {
  name: "Separator orientation lock — caller override is stripped",
  parameters: {
    docs: {
      description: {
        story:
          "Pass `orientation=\"horizontal\"`, `role=\"navigation\"`, and " +
          "`aria-orientation=\"horizontal\"` via untyped spread on a " +
          "`Toolbar.Separator` inside a horizontal toolbar. The wrapper's " +
          "runtime strip MUST keep the rendered separator at " +
          "`role=\"separator\"` + `aria-orientation=\"vertical\"` " +
          "(the perpendicular Base UI computes from the parent context).",
      },
    },
  },
  render: () => {
    // Type-bypass via a spread. The new `Omit<…, "render" | "orientation"
    // | "role" | "aria-orientation">` rejects direct overrides, so we
    // route through an untyped bag. The wrapper's runtime strip MUST
    // drop all three before forwarding into Base UI.
    const bypass = {
      orientation: "horizontal",
      role: "navigation",
      "aria-orientation": "horizontal",
    } as Record<string, unknown>;
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Separator orientation lock regression"
      >
        <Toolbar
          data-testid="toolbar-separator-lock-host"
          orientation="horizontal"
        >
          <Toolbar.Button>Cut</Toolbar.Button>
          <Toolbar.Separator
            data-testid="toolbar-separator-lock"
            {...bypass}
          />
          <Toolbar.Button>Copy</Toolbar.Button>
        </Toolbar>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const sep = canvasElement.querySelector(
      '[data-testid="toolbar-separator-lock"]',
    );
    await expect(sep).not.toBeNull();
    await expect(sep).toHaveAttribute("role", "separator");
    await expect(sep).toHaveAttribute("aria-orientation", "vertical");
  },
};
