import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { useState } from "react";
import { Field, Slider } from "../components";

const meta: Meta<typeof Slider> = {
  title: "Components/Slider",
  component: Slider,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Slider>;

/* ─── 1. Basic — single thumb ──────────────────────────────────────── */
export const Basic: Story = {
  name: "Basic (single thumb)",
  parameters: {
    docs: {
      description: {
        story:
          "Stock single-thumb slider. Click the track to seek; drag the " +
          "thumb to fine-tune; ArrowLeft / ArrowRight on the thumb's nested " +
          "<input type=range> step by `step` (defaults to 1). The aria-" +
          "wiring suite asserts that ArrowRight×5 advances the value by " +
          "exactly 5 × step.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic slider">
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <Slider
          defaultValue={50}
          min={0}
          max={100}
          step={1}
          aria-label="Volume"
          data-testid="slider-basic"
        />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const slider = canvas.getByRole("slider", { name: /volume/i });

    await expect(slider).toHaveAttribute("aria-valuenow", "50");
    await userEvent.tab();
    await expect(slider).toHaveFocus();
    await userEvent.keyboard("{ArrowRight}{ArrowRight}{ArrowRight}{ArrowRight}{ArrowRight}");
    await waitFor(() => expect(slider).toHaveAttribute("aria-valuenow", "55"));
  },
};

/* ─── 2. Range — two thumbs ────────────────────────────────────────── */
export const Range: Story = {
  name: "Range (two thumbs)",
  parameters: {
    docs: {
      description: {
        story:
          "Pass an array to `value` / `defaultValue` and Base UI auto-" +
          "detects range mode: two thumbs, an indicator that spans the " +
          "selected segment, independent keyboard control per thumb. The " +
          "aria-wiring suite asserts dragging thumb-1 right increases " +
          "value[0] without touching value[1].",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Range slider">
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <Slider
          defaultValue={[20, 60]}
          min={0}
          max={100}
          step={1}
          data-testid="slider-range"
          aria-label="Price range"
        />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const minThumb = canvas.getByRole("slider", {
      name: /price range \(1 of 2\)/i,
    });
    const maxThumb = canvas.getByRole("slider", {
      name: /price range \(2 of 2\)/i,
    });

    await expect(minThumb).toHaveAttribute("aria-valuenow", "20");
    await expect(maxThumb).toHaveAttribute("aria-valuenow", "60");
    await userEvent.tab();
    await expect(minThumb).toHaveFocus();
    await userEvent.keyboard("{ArrowRight}");
    await waitFor(() =>
      expect(minThumb).toHaveAttribute("aria-valuenow", "21"),
    );
    await expect(maxThumb).toHaveAttribute("aria-valuenow", "60");
  },
};

/* ─── 3. All sizes ─────────────────────────────────────────────────── */
export const AllSizes: Story = {
  name: "All sizes",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="All slider sizes"
      style={{ flexDirection: "column", alignItems: "stretch", gap: "1.5rem" }}
    >
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <span className="zs-story-label">Small (track 0.25rem)</span>
        <Slider
          size="sm"
          defaultValue={30}
          aria-label="Small slider"
        />
      </div>
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <span className="zs-story-label">Medium (track 0.375rem)</span>
        <Slider
          size="md"
          defaultValue={50}
          aria-label="Medium slider"
        />
      </div>
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <span className="zs-story-label">Large (track 0.5rem)</span>
        <Slider
          size="lg"
          defaultValue={75}
          aria-label="Large slider"
        />
      </div>
    </div>
  ),
};

/* ─── 4. All variants ──────────────────────────────────────────────── */
export const AllVariants: Story = {
  name: "All variants",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="All slider variants"
      style={{ flexDirection: "column", alignItems: "stretch", gap: "1.5rem" }}
    >
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <span className="zs-story-label">Default (solid accent)</span>
        <Slider
          variant="default"
          defaultValue={50}
          aria-label="Default variant"
        />
      </div>
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <span className="zs-story-label">Outline (accent rim)</span>
        <Slider
          variant="outline"
          defaultValue={50}
          aria-label="Outline variant"
        />
      </div>
    </div>
  ),
};

/* ─── 5. Steps (discrete) ──────────────────────────────────────────── */
export const Steps: Story = {
  name: "Steps (discrete with snap)",
  parameters: {
    docs: {
      description: {
        story:
          "step=25 — the value snaps to 0 / 25 / 50 / 75 / 100. " +
          "Discrete stops let the user pick a coarse setting (e.g. quality " +
          "level / poll grade) without needing the precision of a numeric " +
          "input. Note: this slice ships value-snapping only; visual tick " +
          "marks at each step are deferred to a future Slider.Indicator " +
          "story (Base UI has no native tick part — they're rendered ad-" +
          "hoc against the track).",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Stepped slider">
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <Slider
          defaultValue={50}
          min={0}
          max={100}
          step={25}
          aria-label="Quality"
        />
      </div>
    </div>
  ),
};

/* ─── 6. With value badge ──────────────────────────────────────────── */
export const WithValue: Story = {
  name: "With numeric value badge",
  parameters: {
    docs: {
      description: {
        story:
          "`showValue` renders a Base UI `<output>` next to the track that " +
          "auto-updates as the value changes. Use `format` to spell " +
          "out percent / currency / units.",
      },
    },
  },
  render: function WithValueRender() {
    const [value, setValue] = useState<number>(60);
    return (
      <div className="zs-story-row" role="group" aria-label="Slider with value">
        <div className="zs-story-cell" style={{ inlineSize: "22rem" }}>
          <Slider
            showValue
            value={value}
            onValueChange={(next) => setValue(next)}
            min={0}
            max={100}
            step={1}
            // Format as a unit (%) so the value reads as the integer the
            // user expects. Intl's `style: "percent"` would interpret the
            // raw number as a fraction (60 → "6,000%") — surprising for a
            // 0–100 slider.
            format={{ style: "unit", unit: "percent", maximumFractionDigits: 0 }}
            aria-label="Brightness"
            data-testid="slider-withvalue"
          />
        </div>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const slider = canvas.getByRole("slider", { name: /brightness/i });

    await expect(slider).toHaveAttribute("aria-valuenow", "60");
    await userEvent.tab();
    await expect(slider).toHaveFocus();
    await userEvent.keyboard("{ArrowRight}");
    await waitFor(() => expect(slider).toHaveAttribute("aria-valuenow", "61"));
    await expect(canvas.getByText(/61%/i)).toBeVisible();
  },
};

/* ─── 7. Disabled ──────────────────────────────────────────────────── */
export const Disabled: Story = {
  name: "Disabled",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled slider">
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <span className="zs-story-label">disabled prop</span>
        <Slider disabled defaultValue={40} aria-label="Locked slider" />
      </div>
      <div
        className="zs-story-cell"
        style={{ inlineSize: "20rem", marginInlineStart: "2rem" }}
      >
        <span className="zs-story-label">Field disabled — inherits</span>
        <Field disabled>
          <Field.Label>Speed</Field.Label>
          <Slider defaultValue={60} aria-label="Speed" />
        </Field>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const locked = canvas.getByRole("slider", { name: /locked slider/i });
    const inherited = canvas.getByRole("slider", { name: /speed/i });

    await expect(locked).toBeDisabled();
    await expect(inherited).toBeDisabled();
  },
};

/* ─── 8. With label (Field cascade) ────────────────────────────────── */
export const WithLabel: Story = {
  name: "With label + Field cascade",
  parameters: {
    docs: {
      description: {
        story:
          "Composed inside a Field — label sits above, description below. " +
          "Field's `size` cascades to Slider (size='sm' shown). The label " +
          "auto-associates with the thumb input via aria-labelledby.",
      },
    },
  },
  render: function WithLabelRender() {
    const [value, setValue] = useState<number>(70);
    return (
      <div className="zs-story-row" role="group" aria-label="Slider with label">
        <div className="zs-story-cell" style={{ inlineSize: "24rem" }}>
          <Field size="sm">
            <Field.Label>Volume — {value}%</Field.Label>
            <Slider
              value={value}
              onValueChange={(next) => setValue(next)}
              min={0}
              max={100}
              step={1}
            />
            <Field.Description>
              Drag the handle or use arrow keys.
            </Field.Description>
          </Field>
        </div>
      </div>
    );
  },
};

/* ─── 9. Vertical orientation ──────────────────────────────────────── */
export const Vertical: Story = {
  name: "Vertical orientation",
  parameters: {
    docs: {
      description: {
        story:
          "`orientation=\"vertical\"` flips the track to the block-axis. " +
          "Up is more (the indicator fills from the bottom), down is less. " +
          "ArrowUp / ArrowDown still drive the value; PageUp / PageDown " +
          "step by `largeStep`.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Vertical slider"
      style={{ alignItems: "center", gap: "2rem" }}
    >
      <div
        className="zs-story-cell"
        style={{ blockSize: "10rem", inlineSize: "2.5rem" }}
      >
        <Slider
          orientation="vertical"
          defaultValue={60}
          min={0}
          max={100}
          step={1}
          aria-label="Vertical volume"
          data-testid="slider-vertical"
        />
      </div>
      <div
        className="zs-story-cell"
        style={{ blockSize: "10rem", inlineSize: "2.5rem" }}
      >
        <Slider
          orientation="vertical"
          defaultValue={[20, 80]}
          min={0}
          max={100}
          step={1}
          aria-label="Vertical range"
        />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const slider = canvas.getByRole("slider", { name: /vertical volume/i });

    await expect(slider).toHaveAttribute("aria-valuenow", "60");
    await userEvent.tab();
    await expect(slider).toHaveFocus();
    await userEvent.keyboard("{ArrowUp}");
    await waitFor(() => expect(slider).toHaveAttribute("aria-valuenow", "61"));
  },
};

/* ─── 10. Aria propagation regression (Slice-7 review item 2) ──────── */
export const AriaPropagation: Story = {
  name: "Aria propagation (aria-describedby)",
  parameters: {
    docs: {
      description: {
        story:
          "Regression for the Slice-7 review: `aria-describedby` must " +
          "land on each Thumb (the AT-focusable element via its nested " +
          "<input type=range>), not the Root. Mirrors `aria-label` and " +
          "`aria-labelledby` forwarding. The aria-wiring suite asserts " +
          "the thumb (and BOTH thumbs in range mode) carries the " +
          "caller's id.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Aria propagation"
      style={{ flexDirection: "column", alignItems: "stretch", gap: "1.5rem" }}
    >
      <div className="zs-story-cell" style={{ inlineSize: "22rem" }}>
        <span id="slider-aria-help-single" className="zs-story-label">
          0–100, drag or arrow.
        </span>
        <Slider
          defaultValue={50}
          min={0}
          max={100}
          step={1}
          aria-label="Volume"
          aria-describedby="slider-aria-help-single"
          data-testid="slider-aria-prop-single"
        />
      </div>
      <div className="zs-story-cell" style={{ inlineSize: "22rem" }}>
        <span id="slider-aria-help-range" className="zs-story-label">
          0–100, two thumbs.
        </span>
        <Slider
          defaultValue={[20, 60]}
          min={0}
          max={100}
          step={1}
          aria-label="Price range"
          aria-describedby="slider-aria-help-range"
          data-testid="slider-aria-prop-range"
        />
      </div>
    </div>
  ),
};

/* ─── 11. Forced-colors outline (Slice-7 review item 3) ────────────── */
export const ForcedColorsOutline: Story = {
  name: "Forced-colors outline variant",
  parameters: {
    docs: {
      description: {
        story:
          "Outline-variant assertion under `prefers-forced-colors: " +
          "active`. The aria-wiring suite checks the outline thumb's " +
          "computed background is Highlight (a system color), not the " +
          "accent-derived oklch the rest paint uses. Catches Slice 5/6 " +
          "specificity regressions on variant rules.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Forced-colors outline"
    >
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <Slider
          variant="outline"
          defaultValue={50}
          min={0}
          max={100}
          step={1}
          aria-label="Outline forced-colors"
          data-testid="slider-forced-outline"
        />
      </div>
    </div>
  ),
};

/* ─── 12. Coarse pointer hit-target (Slice-7 review item 4) ────────── */
export const CoarsePointer: Story = {
  name: "Coarse pointer hit-target",
  parameters: {
    docs: {
      description: {
        story:
          "Under `pointer: coarse` the thumb's transparent ::after halo " +
          "expands to the Apple HIG floor (`--zs-hit-min` 44 device-" +
          "units) on BOTH axes so a finger lands. The aria-wiring suite " +
          "emulates a coarse pointer and asserts the thumb's bounding " +
          "rect ≥ 2.75rem in inline and block.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Coarse pointer">
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <Slider
          defaultValue={50}
          min={0}
          max={100}
          step={1}
          aria-label="Coarse volume"
          data-testid="slider-coarse"
        />
      </div>
    </div>
  ),
};

/* ─── 13. RTL ──────────────────────────────────────────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew label. Under `dir=\"rtl\"`, ArrowRight DECREASES the " +
          "value and ArrowLeft INCREASES it (the inline-axis flips with " +
          "writing direction) — every translate / position above is a " +
          "logical property so the indicator fills from the inline-start, " +
          "no JS branch.",
      },
    },
  },
  render: () => (
    <div
      dir="rtl"
      className="zs-story-row"
      role="group"
      aria-label="RTL slider"
    >
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <Field>
          <Field.Label>עוצמת קול</Field.Label>
          <Slider
            defaultValue={60}
            min={0}
            max={100}
            step={1}
            aria-label="עוצמת קול"
            data-testid="slider-rtl"
          />
          <Field.Description>גררו או השתמשו בחיצים</Field.Description>
        </Field>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const slider = canvas.getByRole("slider", { name: /עוצמת קול/i });

    await expect(slider).toHaveAttribute("aria-valuenow", "60");
    await userEvent.tab();
    await expect(slider).toHaveFocus();
    await userEvent.keyboard("{ArrowRight}");
    await waitFor(() => expect(slider).toHaveAttribute("aria-valuenow", "59"));
  },
};

/* ─── 13. ConsumerStylePreserved — Round 5 regression ───────────────── *
 *
 * Regression for Round 5 fix #5. Pre-fix, the Slider wrapper spread
 * `{...rest}` BEFORE its own `style={valuePositionStyle}`, so a
 * caller-passed `style={{ background: "red" }}` was wiped — even when
 * `valuePositionStyle` was undefined (because `style` itself was
 * blanked by the explicit prop). The aria-wiring runner asserts that
 * the consumer-set `background-color` survives onto the slider root. */
export const ConsumerStylePreserved: Story = {
  name: "Consumer style preserved (Round 5 regression)",
  parameters: {
    docs: {
      description: {
        story:
          "Caller-passed `style` must reach the slider root, alongside " +
          "the wrapper's `--zs-slider-value-position` custom property.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Slider consumer style"
    >
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <Slider
          defaultValue={40}
          min={0}
          max={100}
          step={1}
          showValue
          aria-label="Styled slider"
          data-testid="slider-consumer-style"
          style={{
            backgroundColor: "rgb(255, 0, 0)",
            paddingInline: "0.5rem",
          }}
        />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("slider-consumer-style");
    // Walk to the BaseSlider.Root element — Base UI renders it as the
    // element with data-testid (no extra wrapper).
    await expect(root).toHaveStyle({
      backgroundColor: "rgb(255, 0, 0)",
    });
  },
};

/* ─── 14. RequiredCascade — Field required → thumb input aria-required ─ *
 *
 * Wave 5 🔴 regression. Pre-fix, `Slider` read Field context for `size`
 * and `disabled` but not `required`, so a `<Field required><Slider/></
 * Field>` thumb input carried no `aria-required` and AT users could not
 * tell the slider was required. Post-fix: `requiredProp ?? fieldCtx
 * ?.required ?? false` cascades, and the resolved boolean is attached
 * to each Thumb's nested `<input type="range">` via the `inputRef`
 * callback (Base UI's Slider.Root has no `required` prop). The
 * aria-wiring runner asserts the input carries `aria-required="true"`. */
export const RequiredCascade: Story = {
  name: "Required cascade (Wave 5 regression)",
  parameters: {
    docs: {
      description: {
        story:
          "`<Field required>` cascades into Slider so the thumb's nested " +
          "<input type=range> carries `aria-required=\"true\"`. Mirrors " +
          "Input.tsx's required cascade — explicit prop wins, then " +
          "Field context, then false.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Slider required cascade"
      style={{ flexDirection: "column", alignItems: "stretch", gap: "1.5rem" }}
    >
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <span className="zs-story-label">Field required — cascades</span>
        <Field required>
          <Field.Label>Volume</Field.Label>
          <Slider
            defaultValue={50}
            min={0}
            max={100}
            step={1}
            data-testid="slider-required-field"
          />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <span className="zs-story-label">Explicit required prop</span>
        <Slider
          required
          defaultValue={50}
          min={0}
          max={100}
          step={1}
          aria-label="Mandatory level"
          data-testid="slider-required-explicit"
        />
      </div>
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <span className="zs-story-label">No required — control</span>
        <Slider
          defaultValue={50}
          min={0}
          max={100}
          step={1}
          aria-label="Optional level"
          data-testid="slider-required-none"
        />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Field branch — the Field.Label gives the input its accessible
    // name; the slider role is the input itself.
    const fieldThumb = canvas.getByRole("slider", { name: /volume/i });
    await expect(fieldThumb).toHaveAttribute("aria-required", "true");

    const explicitThumb = canvas.getByRole("slider", {
      name: /mandatory level/i,
    });
    await expect(explicitThumb).toHaveAttribute("aria-required", "true");

    const optionalThumb = canvas.getByRole("slider", {
      name: /optional level/i,
    });
    await expect(optionalThumb).not.toHaveAttribute("aria-required");
  },
};

/* ─── 15. RangeLabelledBy — distinct names with aria-labelledby ──────── *
 *
 * Wave 5 🔴 regression. Pre-fix, range thumbs forwarded the SAME
 * `aria-labelledby` on both Thumb wrappers; Base UI puts
 * `aria-labelledby` ahead of `aria-label` on the input, so both thumbs
 * ended up with the same accessible name. Post-fix: we render a
 * visually-hidden per-thumb suffix span (" (1 of 2)" / " (2 of 2)") and
 * append its id to each thumb's `aria-labelledby` chain, so each thumb
 * announces "<external label> (n of N)" distinctly. The aria-wiring
 * runner asserts the two thumb inputs have distinct accessible names. */
export const RangeLabelledBy: Story = {
  name: "Range with aria-labelledby (Wave 5 regression)",
  parameters: {
    docs: {
      description: {
        story:
          "Range mode with an external label wired through " +
          "`aria-labelledby`. Each thumb gets a visually-hidden suffix " +
          "id appended so both thumbs receive distinct accessible " +
          "names, not the same one.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Range slider labelledby"
    >
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <span id="zs-slider-range-label" className="zs-story-label">
          Price range
        </span>
        <Slider
          defaultValue={[20, 60]}
          min={0}
          max={100}
          step={1}
          aria-labelledby="zs-slider-range-label"
          data-testid="slider-range-labelledby"
        />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const minThumb = canvas.getByRole("slider", {
      name: /price range \(1 of 2\)/i,
    });
    const maxThumb = canvas.getByRole("slider", {
      name: /price range \(2 of 2\)/i,
    });

    await expect(minThumb).toHaveAttribute("aria-valuenow", "20");
    await expect(maxThumb).toHaveAttribute("aria-valuenow", "60");
    // Names must be distinct — pre-fix they were identical.
    const nameA = minThumb.getAttribute("aria-labelledby");
    const nameB = maxThumb.getAttribute("aria-labelledby");
    await expect(nameA).not.toEqual(nameB);
  },
};
