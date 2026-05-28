import type { Meta, StoryObj } from "@storybook/react";
import { useState } from "react";
import { Field, Toggle } from "../components";

const meta: Meta<typeof Toggle> = {
  title: "Components/Toggle",
  component: Toggle,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Toggle>;

/* Small inline icon — used by icon-only and icon+text stories. The
 * "B" glyph maps to a Bold-style toggle, the canonical icon Toggle
 * demo (RichTextEditor-style toolbar). Color rides currentColor so
 * pressed / disabled state flows through. */
function BoldGlyph() {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        fill="currentColor"
        d="M5 3h4.2c2.1 0 3.5 1.05 3.5 2.85 0 1.05-.45 1.95-1.2 2.4 1.05.45 1.65 1.45 1.65 2.7C13.15 12.95 11.7 14 9.4 14H5V3zm2 4.45h2c.95 0 1.5-.45 1.5-1.2 0-.8-.55-1.2-1.5-1.2H7v2.4zm0 4.55h2.3c.95 0 1.5-.45 1.5-1.25 0-.85-.55-1.3-1.5-1.3H7V12z"
      />
    </svg>
  );
}
function ItalicGlyph() {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        fill="currentColor"
        d="M6 3h6v1.5h-2L8 11.5h2V13H4v-1.5h2L8 4.5H6V3z"
      />
    </svg>
  );
}
function UnderlineGlyph() {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        fill="currentColor"
        d="M4 2h1.5v6c0 1.7 1 2.5 2.5 2.5S10.5 9.7 10.5 8V2H12v6.1c0 2.4-1.6 3.9-4 3.9S4 10.5 4 8.1V2zm-.5 11.5h9V15h-9v-1.5z"
      />
    </svg>
  );
}

/* ─── 1. AllStates — standalone Toggle ─────────────────────────────── */
export const AllStates: Story = {
  name: "All states (standalone)",
  parameters: {
    docs: {
      description: {
        story:
          "Standalone Toggle in the four core states — unpressed, " +
          "pressed, disabled-unpressed, disabled-pressed. The pressed " +
          "state paints with the accent fill + ink combo (same as " +
          "Button's `filled`); disabled-pressed desaturates toward a " +
          "neutral so the pill reads inactive without losing the " +
          "'this WAS on' signal.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All states">
      <div className="zs-story-cell">
        <span className="zs-story-label">Unpressed</span>
        <Toggle data-testid="toggle-state-unpressed">Bold</Toggle>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Pressed</span>
        <Toggle defaultPressed data-testid="toggle-state-pressed">
          Bold
        </Toggle>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Disabled</span>
        <Toggle disabled data-testid="toggle-state-disabled">
          Bold
        </Toggle>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Disabled + pressed</span>
        <Toggle
          disabled
          defaultPressed
          data-testid="toggle-state-disabled-pressed"
        >
          Bold
        </Toggle>
      </div>
    </div>
  ),
};

/* ─── 2. AllSizes — pressed standalone in sm/md/lg ─────────────────── */
export const AllSizes: Story = {
  name: "All sizes",
  parameters: {
    docs: {
      description: {
        story:
          "Toggle sizes mirror Button + Input rhythm — `sm` 2rem, `md` " +
          "2.5rem (default), `lg` 3rem. Shown in the pressed state so the " +
          "accent fill is visible at every size.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All sizes">
      <div className="zs-story-cell">
        <span className="zs-story-label">Small</span>
        <Toggle size="sm" defaultPressed data-testid="toggle-size-sm">
          Bold
        </Toggle>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Medium</span>
        <Toggle size="md" defaultPressed data-testid="toggle-size-md">
          Bold
        </Toggle>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Large</span>
        <Toggle size="lg" defaultPressed data-testid="toggle-size-lg">
          Bold
        </Toggle>
      </div>
    </div>
  ),
};

/* ─── 3. AllVariants — default / plain / tinted × press states ─────── */
export const AllVariants: Story = {
  name: "All variants",
  parameters: {
    docs: {
      description: {
        story:
          "Each variant shown unpressed AND pressed. `default` gray " +
          "rest, accent press. `plain` transparent rest, accent press. " +
          "`tinted` accent-tinted rest, accent fill press — saturation " +
          "step from soft to solid.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All variants">
      <div className="zs-story-cell">
        <span className="zs-story-label">Default</span>
        <Toggle variant="default" data-testid="toggle-variant-default-off">
          Bold
        </Toggle>
        <Toggle
          variant="default"
          defaultPressed
          data-testid="toggle-variant-default-on"
        >
          Bold
        </Toggle>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Plain</span>
        <Toggle variant="plain" data-testid="toggle-variant-plain-off">
          Bold
        </Toggle>
        <Toggle
          variant="plain"
          defaultPressed
          data-testid="toggle-variant-plain-on"
        >
          Bold
        </Toggle>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Tinted</span>
        <Toggle variant="tinted" data-testid="toggle-variant-tinted-off">
          Bold
        </Toggle>
        <Toggle
          variant="tinted"
          defaultPressed
          data-testid="toggle-variant-tinted-on"
        >
          Bold
        </Toggle>
      </div>
    </div>
  ),
};

/* ─── 4. WithIconOnly — pressable button with a single icon ────────── */
export const WithIconOnly: Story = {
  name: "Icon only",
  parameters: {
    docs: {
      description: {
        story:
          "Icon-only Toggle requires an explicit `aria-label` so screen " +
          "readers announce the action. The glyph sizes to 1em against " +
          "the Toggle's font-size; sm/md/lg sizes scale the icon " +
          "automatically.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Icon only">
      <div className="zs-story-cell">
        <Toggle aria-label="Bold" data-testid="toggle-icon-only">
          <BoldGlyph />
        </Toggle>
      </div>
    </div>
  ),
};

/* ─── 5. WithIconAndText — combined icon + label ───────────────────── */
export const WithIconAndText: Story = {
  name: "Icon + text",
  parameters: {
    docs: {
      description: {
        story:
          "Icon followed by inline text. The icon inherits currentColor " +
          "so pressed state flows through to both the glyph and the " +
          "label without separate rules.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Icon and text">
      <div className="zs-story-cell">
        <Toggle defaultPressed data-testid="toggle-icon-text">
          <BoldGlyph />
          Bold
        </Toggle>
      </div>
    </div>
  ),
};

/* ─── 6. Disabled — both states side by side ───────────────────────── */
export const Disabled: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Disabled in both unpressed and pressed states. The pressed " +
          "disabled paints a desaturated accent so the 'was on' " +
          "history is still readable; both share the disabled cursor " +
          "and label hierarchy.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled">
      <div className="zs-story-cell">
        <span className="zs-story-label">Disabled — unpressed</span>
        <Toggle disabled data-testid="toggle-disabled-unpressed">
          Bold
        </Toggle>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Disabled — pressed</span>
        <Toggle
          disabled
          defaultPressed
          data-testid="toggle-disabled-pressed"
        >
          Bold
        </Toggle>
      </div>
    </div>
  ),
};

/* ─── 7. TwoSegmentsSingle — segmented control, 2 segments ─────────── *
 *
 * Single-selection (the default — `multiple={false}` is implicit).
 * Controlled so the aria-wiring assertion has a predictable starting
 * point: `day` is pressed initially; the test clicks the 2nd segment
 * and verifies its `aria-pressed` flips while the 1st's drops to
 * `false`.
 */
export const TwoSegmentsSingle: Story = {
  name: "Two segments (single)",
  parameters: {
    docs: {
      description: {
        story:
          "Single-selection segmented control with two segments. " +
          "Clicking one flips the other off — mutually exclusive. " +
          "Default `multiple={false}`.",
      },
    },
  },
  render: function TwoSegmentsSingleRender() {
    const [value, setValue] = useState<string[]>(["day"]);
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Two segments single"
      >
        <div className="zs-story-cell">
          <Toggle.Group
            value={value}
            onValueChange={(v: string[]) => setValue(v)}
            aria-label="Time range"
            data-testid="toggle-group-single"
          >
            <Toggle value="day" data-testid="toggle-single-day">
              Day
            </Toggle>
            <Toggle value="week" data-testid="toggle-single-week">
              Week
            </Toggle>
          </Toggle.Group>
        </div>
      </div>
    );
  },
};

/* ─── 8. FiveSegmentsSingle — max comfortable single-selection ─────── */
export const FiveSegmentsSingle: Story = {
  name: "Five segments (single, max)",
  parameters: {
    docs: {
      description: {
        story:
          "Five segments is the comfortable upper bound for a segmented " +
          "control. Beyond five, a Select reads more comfortably and " +
          "Toggle.Group emits a dev-warn (deduped per process).",
      },
    },
  },
  render: function FiveSegmentsRender() {
    const [value, setValue] = useState<string[]>(["1y"]);
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Five segments single"
      >
        <div className="zs-story-cell">
          <Toggle.Group
            value={value}
            onValueChange={(v: string[]) => setValue(v)}
            aria-label="Range"
            data-testid="toggle-group-five"
          >
            <Toggle value="1d">1D</Toggle>
            <Toggle value="1w">1W</Toggle>
            <Toggle value="1m">1M</Toggle>
            <Toggle value="3m">3M</Toggle>
            <Toggle value="1y">1Y</Toggle>
          </Toggle.Group>
        </div>
      </div>
    );
  },
};

/* ─── 9. MultipleMode — independent boolean filter pills ───────────── */
export const MultipleMode: Story = {
  name: "Multiple mode",
  parameters: {
    docs: {
      description: {
        story:
          "`multiple={true}` lets each segment carry an independent " +
          "boolean — filter pills, formatting toggles, anything where " +
          "0–N of N can be pressed at once. The aria-wiring assertion " +
          "presses three in sequence then un-presses the middle one to " +
          "verify independence.",
      },
    },
  },
  render: function MultipleModeRender() {
    const [value, setValue] = useState<string[]>([]);
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Multiple mode"
      >
        <div className="zs-story-cell">
          <Toggle.Group
            multiple
            value={value}
            onValueChange={(v: string[]) => setValue(v)}
            aria-label="Text formatting"
            data-testid="toggle-group-multiple"
          >
            <Toggle
              value="bold"
              aria-label="Bold"
              data-testid="toggle-multi-bold"
            >
              <BoldGlyph />
            </Toggle>
            <Toggle
              value="italic"
              aria-label="Italic"
              data-testid="toggle-multi-italic"
            >
              <ItalicGlyph />
            </Toggle>
            <Toggle
              value="underline"
              aria-label="Underline"
              data-testid="toggle-multi-underline"
            >
              <UnderlineGlyph />
            </Toggle>
          </Toggle.Group>
        </div>
      </div>
    );
  },
};

/* ─── 10. AllSizes (group) — sm / md / lg stacked groups ───────────── */
export const AllSizesGroup: Story = {
  name: "Group — all sizes",
  parameters: {
    docs: {
      description: {
        story:
          "The group's `size` cascades to every Toggle child via " +
          "context. A child explicit `size` prop overrides the cascade.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="All group sizes"
      style={{ flexDirection: "column", alignItems: "flex-start" }}
    >
      <div className="zs-story-cell">
        <span className="zs-story-label">Small</span>
        <Toggle.Group size="sm" defaultValue={["a"]} aria-label="Small group">
          <Toggle value="a">Alpha</Toggle>
          <Toggle value="b">Bravo</Toggle>
          <Toggle value="c">Charlie</Toggle>
        </Toggle.Group>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Medium</span>
        <Toggle.Group size="md" defaultValue={["b"]} aria-label="Medium group">
          <Toggle value="a">Alpha</Toggle>
          <Toggle value="b">Bravo</Toggle>
          <Toggle value="c">Charlie</Toggle>
        </Toggle.Group>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Large</span>
        <Toggle.Group size="lg" defaultValue={["c"]} aria-label="Large group">
          <Toggle value="a">Alpha</Toggle>
          <Toggle value="b">Bravo</Toggle>
          <Toggle value="c">Charlie</Toggle>
        </Toggle.Group>
      </div>
    </div>
  ),
};

/* ─── 11. Horizontal — default orientation made explicit ───────────── */
export const Horizontal: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Explicit `orientation=\"horizontal\"` (the default). Segments " +
          "lay out as columns in a single grid row. Arrow-left / " +
          "arrow-right move focus AND selection between siblings.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Horizontal">
      <div className="zs-story-cell">
        <Toggle.Group
          orientation="horizontal"
          defaultValue={["list"]}
          aria-label="View mode"
          data-testid="toggle-group-horizontal"
        >
          <Toggle value="list">List</Toggle>
          <Toggle value="grid">Grid</Toggle>
          <Toggle value="kanban">Kanban</Toggle>
        </Toggle.Group>
      </div>
    </div>
  ),
};

/* ─── 12. Vertical — stacked segments for settings panels ──────────── */
export const Vertical: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "`orientation=\"vertical\"` stacks segments in a column. Useful " +
          "in narrow settings panels where horizontal space is at a " +
          "premium. Segments size to `inline-size: 100%` so the rail " +
          "stays unified.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Vertical">
      <div className="zs-story-cell">
        <Toggle.Group
          orientation="vertical"
          defaultValue={["medium"]}
          aria-label="Density"
          data-testid="toggle-group-vertical"
        >
          <Toggle value="compact">Compact</Toggle>
          <Toggle value="medium">Medium</Toggle>
          <Toggle value="comfortable">Comfortable</Toggle>
        </Toggle.Group>
      </div>
    </div>
  ),
};

/* ─── 13. EqualWidthOff — intrinsic widths for icon+text mix ───────── */
export const EqualWidthOff: Story = {
  name: "Equal width off",
  parameters: {
    docs: {
      description: {
        story:
          "`equalWidth={false}` lets each Toggle size to its own " +
          "content. Useful in toolbars where icon-only segments mix " +
          "with text segments and forcing equal widths wastes space.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Equal width off"
    >
      <div className="zs-story-cell">
        <Toggle.Group
          equalWidth={false}
          defaultValue={["left"]}
          aria-label="Alignment"
          data-testid="toggle-group-equalwidth-off"
        >
          <Toggle value="left" aria-label="Align left">
            <BoldGlyph />
          </Toggle>
          <Toggle value="center">Center text</Toggle>
          <Toggle value="right" aria-label="Align right">
            <ItalicGlyph />
          </Toggle>
        </Toggle.Group>
      </div>
    </div>
  ),
};

/* ─── 14. WithLabel — Field-wrapped group ──────────────────────────── */
export const WithLabel: Story = {
  name: "With label (Field)",
  parameters: {
    docs: {
      description: {
        story:
          "Toggle.Group wrapped in a Field. Field.Label labels the " +
          "whole group; the group is what `aria-required` and " +
          "validation attach to. Mirrors the Radio + Field idiom from " +
          "Slice 4.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With label">
      <div className="zs-story-cell" style={{ maxWidth: "24rem" }}>
        <Field>
          <Field.Label>View</Field.Label>
          <Toggle.Group defaultValue={["card"]} aria-label="View">
            <Toggle value="list">List</Toggle>
            <Toggle value="card">Card</Toggle>
            <Toggle value="map">Map</Toggle>
          </Toggle.Group>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 15. Disabled — whole group inactive ──────────────────────────── */
export const DisabledGroup: Story = {
  name: "Disabled (group)",
  parameters: {
    docs: {
      description: {
        story:
          "`disabled` on the group cascades to every child. The rail " +
          "surface fades and the segments lose hover affordance; the " +
          "pressed segment still reads as 'was on' via the desaturated " +
          "fill.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled group">
      <div className="zs-story-cell">
        <Toggle.Group
          disabled
          defaultValue={["week"]}
          aria-label="Disabled range"
          data-testid="toggle-group-disabled"
        >
          <Toggle value="day">Day</Toggle>
          <Toggle value="week">Week</Toggle>
          <Toggle value="month">Month</Toggle>
        </Toggle.Group>
      </div>
    </div>
  ),
};

/* ─── 16. RTL — Hebrew labels, segments flow right-to-left ─────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew labels. Grid auto-columns + logical properties carry " +
          "the flip — the first segment ends up on the visual right in " +
          "RTL; the pressed pill, hover tints, and focus ring all " +
          "follow without manual flips.",
      },
    },
  },
  render: () => (
    <div
      dir="rtl"
      className="zs-story-row"
      role="group"
      aria-label="RTL toggle group"
    >
      <div className="zs-story-cell">
        <Toggle.Group
          defaultValue={["שבוע"]}
          aria-label="טווח זמן"
          data-testid="toggle-group-rtl"
        >
          <Toggle value="יום">יום</Toggle>
          <Toggle value="שבוע">שבוע</Toggle>
          <Toggle value="חודש">חודש</Toggle>
        </Toggle.Group>
      </div>
    </div>
  ),
};
