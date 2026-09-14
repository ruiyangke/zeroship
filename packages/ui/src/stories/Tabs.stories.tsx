import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { useState } from "react";
import { Tabs } from "../components";

const meta: Meta<typeof Tabs> = {
  title: "Components/Tabs",
  component: Tabs,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Tabs>;

/* ─── Inline glyphs — kept tiny so the icon stories don't pull a
 * dependency. Pixel-pushed Bold/Italic/Underline glyphs match the
 * Toggle stories' rich-text-editor visual vocabulary. */
function HomeGlyph() {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        fill="currentColor"
        d="M8 1.5l6.5 5.25V14h-4.25v-4.25h-4.5V14H1.5V6.75L8 1.5z"
      />
    </svg>
  );
}
function SettingsGlyph() {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        fill="currentColor"
        d="M8 2a6 6 0 1 0 0 12A6 6 0 0 0 8 2zm0 3.5a2.5 2.5 0 1 1 0 5 2.5 2.5 0 0 1 0-5z"
      />
    </svg>
  );
}
function InboxGlyph() {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        fill="currentColor"
        d="M2.5 2.5h11l.5 6h-3.5l-1 1.5h-3l-1-1.5H2L2.5 2.5zm-.5 7h3.4l1 1.5h3.2l1-1.5H14v4H2v-4z"
      />
    </svg>
  );
}

/* ─── 1. Basic 3-tab — minimal default variant ─────────────────────── */
export const Basic: Story = {
  name: "Basic 3-tab",
  parameters: {
    docs: {
      description: {
        story:
          "Minimal Tabs row in the `default` variant. Three sibling " +
          "panels swap based on the active tab; the underline " +
          "indicator slides between active tabs.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic">
      <Tabs defaultValue="overview" data-testid="tabs-basic">
        <Tabs.List>
          <Tabs.Tab value="overview" data-testid="tabs-basic-tab-overview">
            Overview
          </Tabs.Tab>
          <Tabs.Tab value="usage" data-testid="tabs-basic-tab-usage">
            Usage
          </Tabs.Tab>
          <Tabs.Tab value="alerts" data-testid="tabs-basic-tab-alerts">
            Alerts
          </Tabs.Tab>
          <Tabs.Indicator data-testid="tabs-basic-indicator" />
        </Tabs.List>
        <Tabs.Panel value="overview" data-testid="tabs-basic-panel-overview">
          The overview panel summarises the account at a glance.
        </Tabs.Panel>
        <Tabs.Panel value="usage" data-testid="tabs-basic-panel-usage">
          The usage panel reports request volume and metering counters.
        </Tabs.Panel>
        <Tabs.Panel value="alerts" data-testid="tabs-basic-panel-alerts">
          The alerts panel lists outstanding warnings.
        </Tabs.Panel>
      </Tabs>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const overview = canvas.getByRole("tab", { name: /overview/i });
    const usage = canvas.getByRole("tab", { name: /usage/i });

    await expect(overview).toHaveAttribute("aria-selected", "true");
    await userEvent.click(usage);
    await waitFor(() =>
      expect(usage).toHaveAttribute("aria-selected", "true"),
    );
    await expect(
      canvas.getByRole("tabpanel", { name: /usage/i }),
    ).toHaveTextContent(/request volume/i);
  },
};

/* ─── 2. AllVariants — default / pill / card side by side ──────────── */
export const AllVariants: Story = {
  name: "All variants",
  parameters: {
    docs: {
      description: {
        story:
          "Three variant ladders. `default` paints an underline " +
          "indicator under the active tab; `pill` paints an accent " +
          "pill behind it (Toggle.Group's pressed pattern); `card` " +
          "raises each tab as a folder-style surface.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="All variants"
      style={{ flexDirection: "column", alignItems: "stretch" }}
    >
      <div className="zs-story-cell">
        <span className="zs-story-label">Default — underline</span>
        <Tabs defaultValue="one" variant="default" data-testid="tabs-variant-default">
          <Tabs.List>
            <Tabs.Tab value="one">Inbox</Tabs.Tab>
            <Tabs.Tab value="two">Archive</Tabs.Tab>
            <Tabs.Tab value="three">Drafts</Tabs.Tab>
            <Tabs.Indicator />
          </Tabs.List>
          <Tabs.Panel value="one">Inbox content.</Tabs.Panel>
          <Tabs.Panel value="two">Archive content.</Tabs.Panel>
          <Tabs.Panel value="three">Drafts content.</Tabs.Panel>
        </Tabs>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Pill — segmented</span>
        <Tabs defaultValue="one" variant="pill" data-testid="tabs-variant-pill">
          <Tabs.List>
            <Tabs.Tab value="one">Day</Tabs.Tab>
            <Tabs.Tab value="two">Week</Tabs.Tab>
            <Tabs.Tab value="three">Month</Tabs.Tab>
            <Tabs.Indicator />
          </Tabs.List>
          <Tabs.Panel value="one">Today.</Tabs.Panel>
          <Tabs.Panel value="two">This week.</Tabs.Panel>
          <Tabs.Panel value="three">This month.</Tabs.Panel>
        </Tabs>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Card — folder tabs</span>
        <Tabs defaultValue="one" variant="card" data-testid="tabs-variant-card">
          <Tabs.List>
            <Tabs.Tab value="one">Details</Tabs.Tab>
            <Tabs.Tab value="two">Permissions</Tabs.Tab>
            <Tabs.Tab value="three">History</Tabs.Tab>
          </Tabs.List>
          <Tabs.Panel value="one">Details panel.</Tabs.Panel>
          <Tabs.Panel value="two">Permissions panel.</Tabs.Panel>
          <Tabs.Panel value="three">History panel.</Tabs.Panel>
        </Tabs>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    await userEvent.click(canvas.getByRole("tab", { name: /archive/i }));
    await expect(canvas.getByText(/archive content/i)).toBeVisible();
    await userEvent.click(canvas.getByRole("tab", { name: /week/i }));
    await expect(canvas.getByText(/this week/i)).toBeVisible();
    await userEvent.click(canvas.getByRole("tab", { name: /history/i }));
    await expect(canvas.getByText(/history panel/i)).toBeVisible();
  },
};

/* ─── 3. AllSizes — sm / md / lg ───────────────────────────────────── */
export const AllSizes: Story = {
  name: "All sizes",
  parameters: {
    docs: {
      description: {
        story:
          "Tabs at the three sizes (`sm`, `md` default, `lg`). The size " +
          "cascade is identical to Toggle / Field: explicit prop on " +
          "Tab wins over root context.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="All sizes"
      style={{ flexDirection: "column", alignItems: "stretch" }}
    >
      {(["sm", "md", "lg"] as const).map((size) => (
        <div className="zs-story-cell" key={size}>
          <span className="zs-story-label">Size: {size}</span>
          <Tabs
            defaultValue="overview"
            size={size}
            data-testid={`tabs-sizes-${size}`}
          >
            <Tabs.List>
              <Tabs.Tab value="overview">Overview</Tabs.Tab>
              <Tabs.Tab value="usage">Usage</Tabs.Tab>
              <Tabs.Tab value="alerts">Alerts</Tabs.Tab>
              <Tabs.Indicator />
            </Tabs.List>
            <Tabs.Panel value="overview">Overview panel.</Tabs.Panel>
            <Tabs.Panel value="usage">Usage panel.</Tabs.Panel>
            <Tabs.Panel value="alerts">Alerts panel.</Tabs.Panel>
          </Tabs>
        </div>
      ))}
    </div>
  ),
};

/* ─── 4. Vertical orientation ──────────────────────────────────────── */
export const Vertical: Story = {
  name: "Vertical orientation",
  parameters: {
    docs: {
      description: {
        story:
          "Vertical Tabs row — the list stacks along the inline axis " +
          "and the indicator translates vertically. Logical properties " +
          "keep RTL automatic.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Vertical"
      style={{ minBlockSize: "12rem" }}
    >
      <Tabs
        defaultValue="profile"
        orientation="vertical"
        data-testid="tabs-vertical"
      >
        <Tabs.List>
          <Tabs.Tab value="profile">Profile</Tabs.Tab>
          <Tabs.Tab value="account">Account</Tabs.Tab>
          <Tabs.Tab value="security">Security</Tabs.Tab>
          <Tabs.Indicator />
        </Tabs.List>
        <Tabs.Panel value="profile">Profile preferences.</Tabs.Panel>
        <Tabs.Panel value="account">Account billing.</Tabs.Panel>
        <Tabs.Panel value="security">Security audit log.</Tabs.Panel>
      </Tabs>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const profile = canvas.getByRole("tab", { name: /profile/i });
    const account = canvas.getByRole("tab", { name: /account/i });

    await expect(canvas.getByRole("tablist")).toHaveAttribute(
      "aria-orientation",
      "vertical",
    );
    await userEvent.tab();
    await expect(profile).toHaveFocus();
    await userEvent.keyboard("{ArrowDown}{Enter}");
    await waitFor(() =>
      expect(account).toHaveAttribute("aria-selected", "true"),
    );
  },
};

/* ─── 5. WithIcons — leading glyph ─────────────────────────────────── */
export const WithIcons: Story = {
  name: "With icons",
  parameters: {
    docs: {
      description: {
        story:
          "Each tab carries a leading glyph; the icon sizes to 1em " +
          "(currentColor) so it tracks the tab's font-size.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With icons">
      <Tabs defaultValue="home" data-testid="tabs-with-icons">
        <Tabs.List>
          <Tabs.Tab value="home">
            <HomeGlyph />
            Home
          </Tabs.Tab>
          <Tabs.Tab value="inbox">
            <InboxGlyph />
            Inbox
          </Tabs.Tab>
          <Tabs.Tab value="settings">
            <SettingsGlyph />
            Settings
          </Tabs.Tab>
          <Tabs.Indicator />
        </Tabs.List>
        <Tabs.Panel value="home">Home content.</Tabs.Panel>
        <Tabs.Panel value="inbox">Inbox content.</Tabs.Panel>
        <Tabs.Panel value="settings">Settings content.</Tabs.Panel>
      </Tabs>
    </div>
  ),
};

/* ─── 6. WithBadges — trailing notification count ──────────────────── */
export const WithBadges: Story = {
  name: "With badges",
  parameters: {
    docs: {
      description: {
        story:
          "Tabs with trailing counts read like an inbox row. The badge " +
          "sits inside the tab content as a sibling span so it picks " +
          "up the active-state color along with the label.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With badges">
      <Tabs defaultValue="all" data-testid="tabs-with-badges">
        <Tabs.List>
          <Tabs.Tab value="all">
            All
            <span
              style={{
                display: "inline-flex",
                alignItems: "center",
                marginInlineStart: "0.5rem",
                paddingInline: "0.5rem",
                blockSize: "1.25rem",
                borderRadius: "0.625rem",
                backgroundColor: "var(--zs-fill-secondary)",
                color: "var(--zs-label-secondary)",
                fontSize: "var(--zs-text-caption-1-size)",
                fontWeight: 600,
              }}
            >
              128
            </span>
          </Tabs.Tab>
          <Tabs.Tab value="unread">
            Unread
            <span
              style={{
                display: "inline-flex",
                alignItems: "center",
                marginInlineStart: "0.5rem",
                paddingInline: "0.5rem",
                blockSize: "1.25rem",
                borderRadius: "0.625rem",
                backgroundColor: "var(--zs-fill-secondary)",
                color: "var(--zs-label-secondary)",
                fontSize: "var(--zs-text-caption-1-size)",
                fontWeight: 600,
              }}
            >
              7
            </span>
          </Tabs.Tab>
          <Tabs.Tab value="alerts">
            Alerts
            <span
              style={{
                display: "inline-flex",
                alignItems: "center",
                marginInlineStart: "0.5rem",
                paddingInline: "0.5rem",
                blockSize: "1.25rem",
                borderRadius: "0.625rem",
                backgroundColor: "color-mix(in oklch, var(--zs-system-red) 18%, transparent)",
                color: "var(--zs-system-red)",
                fontSize: "var(--zs-text-caption-1-size)",
                fontWeight: 600,
              }}
            >
              2
            </span>
          </Tabs.Tab>
          <Tabs.Indicator />
        </Tabs.List>
        <Tabs.Panel value="all">All messages.</Tabs.Panel>
        <Tabs.Panel value="unread">Unread messages.</Tabs.Panel>
        <Tabs.Panel value="alerts">Alerts.</Tabs.Panel>
      </Tabs>
    </div>
  ),
};

/* ─── 7. ManyTabs — scrolls horizontally ───────────────────────────── */
export const ManyTabs: Story = {
  name: "Many tabs (scroll)",
  parameters: {
    docs: {
      description: {
        story:
          "10 tabs in a constrained container — the list scrolls " +
          "horizontally (scrollbar hidden) so the row stays usable " +
          "without wrapping.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Many tabs"
      style={{ inlineSize: "32rem" }}
    >
      <Tabs defaultValue="t-1" data-testid="tabs-many">
        <Tabs.List>
          {Array.from({ length: 10 }, (_, idx) => `t-${idx + 1}`).map((value) => (
            <Tabs.Tab key={value} value={value}>
              Section {value.replace("t-", "")}
            </Tabs.Tab>
          ))}
          <Tabs.Indicator />
        </Tabs.List>
        {Array.from({ length: 10 }, (_, idx) => `t-${idx + 1}`).map((value) => (
          <Tabs.Panel key={value} value={value}>
            Panel for {value}.
          </Tabs.Panel>
        ))}
      </Tabs>
    </div>
  ),
};

/* ─── 8. DisabledTab — one tab disabled inside the row ─────────────── */
export const DisabledTab: Story = {
  name: "Disabled tab",
  parameters: {
    docs: {
      description: {
        story:
          "A single Tab disabled mid-row. The visual treatment reads " +
          "inactive and the slot still occupies its layout space. The " +
          "tab is NOT activatable (click / Enter / Space are no-ops; " +
          "`aria-selected` does not flip), but it IS still a focus " +
          "stop in the roving order — Base UI's composite controller " +
          "hardcodes `disabledIndices: []`, so ArrowRight CAN park " +
          "focus on it. See Tabs Guarantee 10 for the contract.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled tab">
      <Tabs defaultValue="enabled-1" data-testid="tabs-disabled">
        <Tabs.List>
          <Tabs.Tab value="enabled-1">Active</Tabs.Tab>
          <Tabs.Tab value="disabled" disabled data-testid="tabs-disabled-tab">
            Disabled
          </Tabs.Tab>
          <Tabs.Tab value="enabled-2">Other</Tabs.Tab>
          <Tabs.Indicator />
        </Tabs.List>
        <Tabs.Panel value="enabled-1">Active content.</Tabs.Panel>
        <Tabs.Panel value="disabled">Disabled content.</Tabs.Panel>
        <Tabs.Panel value="enabled-2">Other content.</Tabs.Panel>
      </Tabs>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const active = canvas.getByRole("tab", { name: /active/i });
    const disabled = canvas.getByRole("tab", { name: /disabled/i });

    await expect(active).toHaveAttribute("aria-selected", "true");
    // Base UI's Tabs.Tab is a composite `role="tab"` button, not a
    // native form control. Per the WAI-ARIA Tabs pattern a disabled
    // tab stays a focus stop in the roving order (see this story's
    // doc note), so Base UI marks it `aria-disabled="true"` +
    // `data-disabled` rather than the native `disabled` attribute that
    // would yank it from the tab sequence. jest-dom's `toBeDisabled()`
    // only sees native `disabled`; the faithful assertion is the
    // aria/data contract plus the behavioral guarantee (clicking does
    // not flip selection).
    await expect(disabled).toHaveAttribute("aria-disabled", "true");
    await expect(disabled).toHaveAttribute("data-disabled");
    await userEvent.click(disabled);
    await expect(active).toHaveAttribute("aria-selected", "true");
    await expect(disabled).toHaveAttribute("aria-selected", "false");
  },
};

/* ─── 9. ControlledValue — controlled with external state ──────────── */
export const ControlledValue: Story = {
  name: "Controlled value",
  parameters: {
    docs: {
      description: {
        story:
          "Controlled Tabs — external state drives `value` and " +
          "`onValueChange`. The selected value reads out in the " +
          "live readout below so the contract is observable.",
      },
    },
  },
  render: function Render() {
    const [value, setValue] = useState<string>("two");
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Controlled value"
        style={{ flexDirection: "column", alignItems: "stretch" }}
      >
        <Tabs
          value={value}
          // Base UI types `TabsTab.Value` as `any | null`, so we coerce
          // through `String(...)` to the consumer's chosen string-key
          // domain (Guarantee 9 in Tabs.tsx).
          onValueChange={(next) => setValue(String(next))}
          data-testid="tabs-controlled"
        >
          <Tabs.List>
            <Tabs.Tab value="one">One</Tabs.Tab>
            <Tabs.Tab value="two">Two</Tabs.Tab>
            <Tabs.Tab value="three">Three</Tabs.Tab>
            <Tabs.Indicator />
          </Tabs.List>
          <Tabs.Panel value="one">First panel.</Tabs.Panel>
          <Tabs.Panel value="two">Second panel.</Tabs.Panel>
          <Tabs.Panel value="three">Third panel.</Tabs.Panel>
        </Tabs>
        <output
          aria-live="polite"
          data-testid="tabs-controlled-readout"
          style={{
            marginBlockStart: "var(--zs-space-3)",
            fontSize: "var(--zs-text-caption-1-size)",
            color: "var(--zs-label-secondary)",
          }}
        >
          Selected: {value}
        </output>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    await expect(canvas.getByText(/selected: two/i)).toBeVisible();
    await userEvent.click(canvas.getByRole("tab", { name: /three/i }));
    await waitFor(() =>
      expect(canvas.getByText(/selected: three/i)).toBeVisible(),
    );
  },
};

/* ─── 10. WithAnimatedIndicator — sandboxed default variant ────────── */
export const WithAnimatedIndicator: Story = {
  name: "With animated indicator",
  parameters: {
    docs: {
      description: {
        story:
          "Default variant in isolation so the underline indicator " +
          "transition between active tabs is easy to see. Reduced-" +
          "motion users get an instant snap (no transition).",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Animated indicator"
    >
      <Tabs defaultValue="alpha" data-testid="tabs-animated">
        <Tabs.List>
          <Tabs.Tab value="alpha">Alpha</Tabs.Tab>
          <Tabs.Tab value="bravo">Bravo</Tabs.Tab>
          <Tabs.Tab value="charlie">Charlie</Tabs.Tab>
          <Tabs.Tab value="delta">Delta</Tabs.Tab>
          <Tabs.Indicator data-testid="tabs-animated-indicator" />
        </Tabs.List>
        <Tabs.Panel value="alpha">A.</Tabs.Panel>
        <Tabs.Panel value="bravo">B.</Tabs.Panel>
        <Tabs.Panel value="charlie">C.</Tabs.Panel>
        <Tabs.Panel value="delta">D.</Tabs.Panel>
      </Tabs>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const alpha = canvas.getByRole("tab", { name: /alpha/i });
    const bravo = canvas.getByRole("tab", { name: /bravo/i });

    /* Animated-indicator story renders Alpha/Bravo/Charlie/Delta. With
     * keepMounted=true (the default — `lazyMount` is off here), all
     * four panels are mounted simultaneously, so `getByRole("tabpanel")`
     * would match four nodes. Verify the story's actual contract: the
     * indicator's `--active-tab-left` CSS custom property tracks the
     * active tab. The initial Alpha tab sits at the start of the row;
     * clicking Bravo translates the indicator and the custom property
     * updates to Bravo's pixel position. */
    await expect(alpha).toHaveAttribute("aria-selected", "true");

    const indicator = canvasElement.querySelector(
      '[data-testid="tabs-animated-indicator"]',
    ) as HTMLElement | null;
    if (!indicator) throw new Error("animated indicator not found");
    // Wait for Base UI to write the initial CSS vars (raf-driven).
    await waitFor(() => {
      const left = indicator.style.getPropertyValue("--active-tab-left");
      expect(left).not.toBe("");
    });
    const initialLeft = indicator.style.getPropertyValue("--active-tab-left");

    await userEvent.click(bravo);
    await waitFor(() =>
      expect(bravo).toHaveAttribute("aria-selected", "true"),
    );
    await waitFor(() => {
      const nextLeft = indicator.style.getPropertyValue("--active-tab-left");
      expect(nextLeft).not.toBe("");
      expect(nextLeft).not.toBe(initialLeft);
    });
  },
};

/* ─── 11. LazyMountPanel — only the active panel is in the DOM ─────── */
export const LazyMountPanel: Story = {
  name: "Lazy-mount panel",
  parameters: {
    docs: {
      description: {
        story:
          "`lazyMount={true}` flips Base UI's `keepMounted` default to " +
          "`false`. Only the active panel renders to the DOM; switching " +
          "unmounts the previous panel and mounts the next. Useful for " +
          "expensive panels (heavy charts, large tables) where keeping " +
          "every panel hydrated costs more than the switch.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Lazy-mount panel"
    >
      <Tabs
        defaultValue="active"
        lazyMount
        data-testid="tabs-lazy-mount"
      >
        <Tabs.List>
          <Tabs.Tab value="active">Active</Tabs.Tab>
          <Tabs.Tab value="dormant">Dormant</Tabs.Tab>
          <Tabs.Tab value="hidden">Hidden</Tabs.Tab>
          <Tabs.Indicator />
        </Tabs.List>
        <Tabs.Panel value="active" data-testid="tabs-lazy-panel-active">
          Active content — visible at first paint.
        </Tabs.Panel>
        <Tabs.Panel value="dormant" data-testid="tabs-lazy-panel-dormant">
          Dormant content — NOT in the DOM until selected.
        </Tabs.Panel>
        <Tabs.Panel value="hidden" data-testid="tabs-lazy-panel-hidden">
          Hidden content — NOT in the DOM until selected.
        </Tabs.Panel>
      </Tabs>
    </div>
  ),
};

/* ─── 13. ActivationAndOverrides ───────────────────────────────────── */
export const ActivationAndOverrides: Story = {
  name: "Activate on focus and overrides",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Activation overrides"
    >
      <Tabs defaultValue="alpha" size="sm" data-testid="tabs-overrides">
        <Tabs.List activateOnFocus loopFocus={false}>
          <Tabs.Tab value="alpha" size="lg">
            Alpha
          </Tabs.Tab>
          <Tabs.Tab value="beta">Beta</Tabs.Tab>
          <Tabs.Indicator />
        </Tabs.List>
        <Tabs.Panel value="alpha" keepMounted={false}>
          Alpha panel.
        </Tabs.Panel>
        <Tabs.Panel value="beta" keepMounted>
          Beta panel.
        </Tabs.Panel>
      </Tabs>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const alpha = canvas.getByRole("tab", { name: /alpha/i });
    const beta = canvas.getByRole("tab", { name: /beta/i });

    await expect(alpha).toHaveAttribute("data-size", "lg");
    await userEvent.tab();
    await expect(alpha).toHaveFocus();
    await userEvent.keyboard("{ArrowRight}");
    await waitFor(() =>
      expect(beta).toHaveAttribute("aria-selected", "true"),
    );
    await userEvent.keyboard("{ArrowRight}");
    await expect(beta).toHaveFocus();
    await expect(canvas.getByText(/beta panel/i)).toBeVisible();
  },
};

/* ─── 12. RTL — mirrored layout ────────────────────────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Tabs in RTL — the list flips automatically because every " +
          "edge / inset / margin on the rail uses logical properties. " +
          "The horizontal indicator anchors via physical `left:` " +
          "(direction-agnostic by construction) so it tracks the active " +
          "tab in both LTR and RTL — Base UI's `--active-tab-left` is a " +
          "physical pixel value, and feeding it into a logical " +
          "`inset-inline-start` would mirror it away from the tab. " +
          "Vertical orientation mirrors symmetrically.",
      },
    },
  },
  render: () => (
    <div
      dir="rtl"
      className="zs-story-row"
      role="group"
      aria-label="RTL"
    >
      <Tabs defaultValue="overview" data-testid="tabs-rtl">
        <Tabs.List>
          <Tabs.Tab value="overview">סקירה</Tabs.Tab>
          <Tabs.Tab value="usage">שימוש</Tabs.Tab>
          <Tabs.Tab value="alerts">התראות</Tabs.Tab>
          <Tabs.Indicator />
        </Tabs.List>
        <Tabs.Panel value="overview">סקירה כללית.</Tabs.Panel>
        <Tabs.Panel value="usage">דוחות שימוש.</Tabs.Panel>
        <Tabs.Panel value="alerts">התראות פעילות.</Tabs.Panel>
      </Tabs>
    </div>
  ),
};
