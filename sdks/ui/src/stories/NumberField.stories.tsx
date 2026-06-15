import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { Form } from "@base-ui/react/form";
import { Field, NumberField } from "../components";

const meta: Meta<typeof NumberField> = {
  title: "Components/NumberField",
  component: NumberField,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof NumberField>;

/* ─── 1. Basic ─────────────────────────────────────────────────────── */
export const Basic: Story = {
  name: "Basic",
  parameters: {
    docs: {
      description: {
        story:
          "Stock NumberField — text input with `-` / `+` stepper buttons. " +
          "Stepper clicks increment by `step` (defaults to 1); ArrowUp / " +
          "ArrowDown on the input do the same.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic number field">
      <div className="zs-story-cell" style={{ maxWidth: "16rem" }}>
        <NumberField
          defaultValue={42}
          data-testid="numberfield-basic"
          aria-label="Quantity"
        />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Base UI 1.5's NumberField.Input is a text input
    // (`type="text" inputmode="numeric" aria-roledescription="Number field"`),
    // NOT `role="spinbutton"`. Its accessible role is `textbox` and the
    // current value lives in the input's `value` (there is no
    // `aria-valuenow`). Query + assert the real contract.
    const input = canvas.getByRole("textbox", { name: /quantity/i });
    const increment = canvas.getByRole("button", { name: /increment/i });
    const decrement = canvas.getByRole("button", { name: /decrement/i });

    await expect(input).toHaveValue("42");
    await userEvent.click(increment);
    await waitFor(() => expect(input).toHaveValue("43"));
    await userEvent.click(input);
    await userEvent.keyboard("{ArrowDown}");
    await waitFor(() => expect(input).toHaveValue("42"));
    await userEvent.click(decrement);
    await waitFor(() => expect(input).toHaveValue("41"));
  },
};

/* ─── 2. AllSizes ──────────────────────────────────────────────────── */
export const AllSizes: Story = {
  name: "All sizes",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="All number field sizes"
    >
      <div className="zs-story-cell" style={{ maxWidth: "12rem" }}>
        <span className="zs-story-label">Small</span>
        <NumberField
          size="sm"
          defaultValue={5}
          aria-label="Small quantity"
          data-testid="numberfield-size-sm"
        />
      </div>
      <div className="zs-story-cell" style={{ maxWidth: "12rem" }}>
        <span className="zs-story-label">Medium</span>
        <NumberField
          size="md"
          defaultValue={10}
          aria-label="Medium quantity"
          data-testid="numberfield-size-md"
        />
      </div>
      <div className="zs-story-cell" style={{ maxWidth: "12rem" }}>
        <span className="zs-story-label">Large</span>
        <NumberField
          size="lg"
          defaultValue={20}
          aria-label="Large quantity"
          data-testid="numberfield-size-lg"
        />
      </div>
    </div>
  ),
};

/* ─── 3. AllVariants ───────────────────────────────────────────────── */
export const AllVariants: Story = {
  name: "All variants",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="All number field variants"
    >
      <div className="zs-story-cell" style={{ maxWidth: "12rem" }}>
        <span className="zs-story-label">Default</span>
        <NumberField
          variant="default"
          defaultValue={42}
          aria-label="Default variant"
        />
      </div>
      <div className="zs-story-cell" style={{ maxWidth: "12rem" }}>
        <span className="zs-story-label">Outline</span>
        <NumberField
          variant="outline"
          defaultValue={42}
          aria-label="Outline variant"
        />
      </div>
    </div>
  ),
};

/* ─── 4. MinMaxStep ────────────────────────────────────────────────── */
export const MinMaxStep: Story = {
  name: "Min / max / step (1-100 step 5)",
  parameters: {
    docs: {
      description: {
        story:
          "Range bounded 1–100 with step 5. Stepper buttons disable at " +
          "the bounds (Base UI auto-toggles `disabled` on Decrement at " +
          "min and Increment at max). The aria-wiring suite asserts that " +
          "clicking `+` three times advances the value by exactly 3 × step.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Min max step number field"
    >
      <div className="zs-story-cell" style={{ maxWidth: "16rem" }}>
        <NumberField
          defaultValue={50}
          min={1}
          max={100}
          step={5}
          aria-label="Bounded quantity"
          data-testid="numberfield-minmaxstep"
        />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Base UI 1.5 NumberField.Input is a `textbox`, not a `spinbutton`;
    // the value lives in the input's `value`, not `aria-valuenow`.
    const input = canvas.getByRole("textbox", { name: /bounded quantity/i });
    const increment = canvas.getByRole("button", { name: /increment/i });
    const decrement = canvas.getByRole("button", { name: /decrement/i });

    await userEvent.click(increment);
    await userEvent.click(increment);
    await userEvent.click(increment);
    await waitFor(() => expect(input).toHaveValue("65"));

    await userEvent.click(decrement);
    await waitFor(() => expect(input).toHaveValue("60"));
  },
};

/* ─── 5. Currency (snapOnStep + Intl format) ───────────────────────── */
export const Currency: Story = {
  name: "Currency (snapOnStep + Intl format)",
  parameters: {
    docs: {
      description: {
        story:
          "Currency formatting via `format={{ style: 'currency', currency: " +
          "'USD' }}`. `snapOnStep` rounds to multiples of `step` so 0.50 " +
          "increments don't drift into 0.4999999 from floating-point " +
          "arithmetic. The locale prop pins the rendering (en-US shown).",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Currency number field"
    >
      <div className="zs-story-cell" style={{ maxWidth: "16rem" }}>
        <NumberField
          defaultValue={9.99}
          min={0}
          max={9999.99}
          step={0.5}
          snapOnStep
          format={{ style: "currency", currency: "USD" }}
          locale="en-US"
          aria-label="Price"
          data-testid="numberfield-currency"
        />
      </div>
    </div>
  ),
};

/* ─── 6. ScrubArea ─────────────────────────────────────────────────── */
export const ScrubArea: Story = {
  name: "With scrub area (drag to change)",
  parameters: {
    docs: {
      description: {
        story:
          "`showScrub` mounts a small drag-to-scrub area at the leading " +
          "edge of the input. Drag horizontally; the value changes by " +
          "`step` per `pixelSensitivity` pixels. Hover shows the resize-EW " +
          "cursor; while dragging the browser pointer-locks and a custom " +
          "cursor pops in over the page.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Scrub area">
      <div
        className="zs-story-cell"
        style={{ maxWidth: "16rem", paddingInlineStart: "3rem" }}
      >
        <NumberField
          showScrub
          defaultValue={50}
          min={0}
          max={100}
          step={1}
          aria-label="Scrubbable quantity"
          data-testid="numberfield-scrub"
        />
      </div>
    </div>
  ),
};

/* ─── 7. Disabled ──────────────────────────────────────────────────── */
export const Disabled: Story = {
  name: "Disabled",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled cascade">
      <div className="zs-story-cell" style={{ maxWidth: "16rem" }}>
        <span className="zs-story-label">disabled prop</span>
        <NumberField
          disabled
          defaultValue={42}
          aria-label="Locked quantity"
        />
      </div>
      <div className="zs-story-cell" style={{ maxWidth: "16rem" }}>
        <span className="zs-story-label">Field disabled — inherits</span>
        <Field disabled>
          <Field.Label>Servings</Field.Label>
          <NumberField defaultValue={4} aria-label="Servings" />
          <Field.Description>Adjust at checkout instead.</Field.Description>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 8. WithLabel + Field cascade ─────────────────────────────────── */
export const WithLabel: Story = {
  name: "With label + Field cascade",
  parameters: {
    docs: {
      description: {
        story:
          "Composed inside a Field — label sits above, description below. " +
          "Field's `size` cascades to NumberField (size='sm' shown). The " +
          "label's `htmlFor` auto-binds to the inner input id.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Field cascade">
      <div className="zs-story-cell" style={{ maxWidth: "20rem" }}>
        <Field size="sm">
          <Field.Label>Servings</Field.Label>
          <NumberField defaultValue={2} min={1} max={20} step={1} />
          <Field.Description>
            How many people are you cooking for?
          </Field.Description>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 9. Required + invalid (combined per brief matrix item 9) ─────── *
 *
 * The brief lists "Required + RequiredInvalid" as a single matrix item.
 * We mount both a normal-required NumberField and an auto-submitted
 * empty one in the same story so the screenshot captures both the rest
 * indicator AND the post-submit invalid styling — slice-4 RequiredInvalid
 * pattern. */
export const Required: Story = {
  name: "Required + invalid (auto-submitted)",
  parameters: {
    docs: {
      description: {
        story:
          "Shows this component behavior with realistic content and keeps the edge case easy to inspect.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Required + invalid number field"
    >
      <div className="zs-story-cell" style={{ maxWidth: "20rem" }}>
        <span className="zs-story-label">Required — rest</span>
        <Field required>
          <Field.Label>
            Quantity <Field.Required />
          </Field.Label>
          <NumberField
            defaultValue={1}
            min={1}
            aria-label="Required quantity"
          />
          <Field.Description>Required — must be ≥ 1.</Field.Description>
        </Field>
      </div>
      <div className="zs-story-cell" style={{ maxWidth: "20rem" }}>
        <span className="zs-story-label">Required — invalid (auto-submitted)</span>
        <RequiredInvalidInline />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Base UI 1.5 NumberField.Input is a `textbox`, not a `spinbutton`.
    //
    // Both side-by-side fields are wrapped in a `<Field><Field.Label>
    // Quantity …</Field.Label>`. Per the ARIA accessible-name algorithm,
    // a Field.Label (wired as `aria-labelledby`) WINS over a caller's
    // `aria-label`, so the inner input's real accessible name is
    // "Quantity" for BOTH — the `aria-label` does not surface. We
    // therefore query the two `Quantity` textboxes and pick the one that
    // becomes `aria-invalid` after the empty form auto-submits (the
    // post-submit invalid field). `aria-invalid` is still wired by Base
    // UI on validation failure.
    await waitFor(() =>
      expect(canvas.getByText(/quantity is required/i)).toBeVisible(),
    );
    const quantityFields = canvas.getAllByRole("textbox", {
      name: /quantity/i,
    });
    const invalid = quantityFields.find(
      (el) => el.getAttribute("aria-invalid") === "true",
    );
    await expect(invalid).toBeDefined();
    await expect(invalid).toHaveAttribute("aria-invalid", "true");
  },
};

function RequiredInvalidInline() {
  // Use Base UI's `<Form>` (not a plain `<form>`). `Field.Error` is wired
  // into Base UI's Form validation pipeline — a Field-level `valueMissing`
  // error only surfaces when the surrounding form is the Base UI `<Form>`
  // that orchestrates the field validity state. A plain `<form>` submit
  // (even via `requestSubmit()`) leaves Base UI's Field unaware of the
  // native validity failure, so `Field.Error match="valueMissing"` never
  // renders. The Radio Required story uses the same `<Form>` for the same
  // reason.
  return (
    <Form
      style={{ display: "flex", flexDirection: "column", gap: "0.5rem" }}
      ref={(form) => {
        if (form) {
          // Defer one tick so React paints once before the synthetic
          // submit runs; otherwise touched/dirty don't tick over.
          queueMicrotask(() => {
            form.requestSubmit();
          });
        }
      }}
      onSubmit={(e) => e.preventDefault()}
    >
      <Field required>
        <Field.Label>
          Quantity <Field.Required />
        </Field.Label>
        <NumberField
          name="quantity"
          min={1}
          aria-label="Required quantity (empty on submit)"
          data-testid="numberfield-required-invalid"
        />
        <Field.Error match="valueMissing">
          Quantity is required.
        </Field.Error>
      </Field>
      <button type="submit" hidden>
        submit
      </button>
    </Form>
  );
}

/* ─── 10. Aria propagation regression (Slice-7 review item 2) ──────── */
export const AriaPropagation: Story = {
  name: "Aria propagation (aria-describedby)",
  parameters: {
    docs: {
      description: {
        story:
          "Shows this component behavior with realistic content and keeps the edge case easy to inspect.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Aria propagation"
    >
      <div className="zs-story-cell" style={{ maxWidth: "20rem" }}>
        <span id="numberfield-aria-help" className="zs-story-label">
          Min 0, max 100, step 1.
        </span>
        <NumberField
          defaultValue={50}
          aria-label="Quantity"
          aria-describedby="numberfield-aria-help"
          data-testid="numberfield-aria-prop"
        />
      </div>
    </div>
  ),
};

/* ─── 11. Bare focus ring (Slice-7 review item 1) ──────────────────── */
export const BareFocus: Story = {
  name: "Bare focus ring",
  parameters: {
    docs: {
      description: {
        story:
          "Shows this component behavior with realistic content and keeps the edge case easy to inspect.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Bare focus">
      <div className="zs-story-cell" style={{ maxWidth: "16rem" }}>
        <NumberField
          defaultValue={42}
          aria-label="Quantity"
          data-testid="numberfield-bare-focus"
        />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Base UI 1.5 NumberField.Input is a `textbox`, not a `spinbutton`.
    const input = canvas.getByRole("textbox", { name: /quantity/i });
    const root = input.closest(".zs-number-field");

    await userEvent.click(input);
    await waitFor(() => expect(root).toHaveAttribute("data-focused"));
    await userEvent.tab();
    await waitFor(() => expect(root).not.toHaveAttribute("data-focused"));
  },
};

/* ─── 12. Forced-colors hover (Slice-7 review item 3) ──────────────── */
export const ForcedColorsHover: Story = {
  name: "Forced-colors hover",
  parameters: {
    docs: {
      description: {
        story:
          "Shows this component behavior with realistic content and keeps the edge case easy to inspect.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Forced-colors hover"
      style={{ flexDirection: "column", alignItems: "stretch", gap: "1.5rem" }}
    >
      <div className="zs-story-cell" style={{ maxWidth: "16rem" }}>
        <span className="zs-story-label">Default variant</span>
        <NumberField
          variant="default"
          defaultValue={50}
          aria-label="Default forced-colors"
          data-testid="numberfield-forced-default"
        />
      </div>
      <div className="zs-story-cell" style={{ maxWidth: "16rem" }}>
        <span className="zs-story-label">Outline variant</span>
        <NumberField
          variant="outline"
          defaultValue={50}
          aria-label="Outline forced-colors"
          data-testid="numberfield-forced-outline"
        />
      </div>
    </div>
  ),
};

/* ─── 13. Coarse pointer hit-target (Slice-7 review item 4) ────────── */
export const CoarsePointer: Story = {
  name: "Coarse pointer hit-target",
  parameters: {
    docs: {
      description: {
        story:
          "Under `pointer: coarse` the stepper buttons grow to the " +
          "coarse-pointer minimum target (44 device-units ≈ " +
          "`--zs-hit-min`) on BOTH axes so a finger lands. The " +
          "aria-wiring suite emulates a coarse pointer and asserts " +
          "the button's bounding rect ≥ 2.75rem in inline and block.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Coarse pointer">
      <div className="zs-story-cell" style={{ maxWidth: "16rem" }}>
        <NumberField
          defaultValue={42}
          aria-label="Coarse quantity"
          data-testid="numberfield-coarse"
        />
      </div>
    </div>
  ),
};

/* ─── 14. RTL ──────────────────────────────────────────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew label. `dir=\"rtl\"` flips the stepper button order " +
          "(decrement on the right, increment on the left, mirroring the " +
          "reading direction) and the scrub-area anchor (leading edge " +
          "becomes the right side of the shell) — all via logical " +
          "properties, no JS branch.",
      },
    },
  },
  render: () => (
    <div
      dir="rtl"
      className="zs-story-row"
      role="group"
      aria-label="RTL number field"
    >
      <div className="zs-story-cell" style={{ maxWidth: "20rem" }}>
        <Field>
          <Field.Label>כמות</Field.Label>
          <NumberField
            defaultValue={5}
            min={1}
            max={20}
            step={1}
            aria-label="כמות"
            data-testid="numberfield-rtl"
          />
          <Field.Description>הקלידו או החליקו</Field.Description>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 15. Field-auto aria preservation (Wave-9 review item 1) ──────── *
 *
 * REGRESSION for the aria-merge fix. Mounts NumberField inside a Field
 * with a Label AND a Description, but passes NO `aria-label` /
 * `aria-labelledby` / `aria-describedby` props from the caller. The
 * inner <input> must STILL carry Base UI's auto-wired
 * `aria-labelledby` (from Field.Label) and `aria-describedby` (from
 * Field.Description) — pre-fix, our wrapper passed every aria-*
 * prop directly to BaseNumberField.Input, and Base UI's mergeProps
 * writes `undefined` over its own auto-wired ids
 * (`mergedProps[propName] = externalPropValue` even when undefined,
 * `@base-ui/merge-props/mergeProps.js:153`). The aria-wiring suite
 * asserts BOTH ids are present.
 *
 * Post-fix the inner input keeps Base UI's `aria-labelledby` (the
 * Label's id) and `aria-describedby` (the Description's id). */
export const FieldAutoAria: Story = {
  name: "Field-auto aria preservation (no caller props)",
  parameters: {
    docs: {
      description: {
        story:
          "Shows this component behavior with realistic content and keeps the edge case easy to inspect.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Field aria preservation"
    >
      <div className="zs-story-cell" style={{ maxWidth: "20rem" }}>
        <Field>
          <Field.Label>Quantity</Field.Label>
          <NumberField
            defaultValue={3}
            min={0}
            max={10}
            step={1}
            data-testid="numberfield-field-auto-aria"
          />
          <Field.Description>
            How many do you need? Field auto-wires this id to the input.
          </Field.Description>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 16. Consumer focus handlers preserved (Wave-9 review item 2) ─── *
 *
 * REGRESSION for the focus-handler compose fix. Pre-fix the Root
 * render callback REPLACED `rootProps.onFocus` / `rootProps.onBlur`
 * with our local bare-focus trackers, dropping any handler a parent
 * forwarded down via inherited root props. The story attaches an
 * `onFocus` / `onBlur` to the NumberField; the play() focuses the
 * input and asserts the counter ticks. Pre-fix the counter stays
 * at zero. */
export const ConsumerFocusHandlers: Story = {
  name: "Consumer onFocus / onBlur preserved",
  parameters: {
    docs: {
      description: {
        story:
          "Shows this component behavior with realistic content and keeps the edge case easy to inspect.",
      },
    },
  },
  render: () => <ConsumerFocusInline />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const input = canvas.getByTestId("numberfield-focus-handlers");
    const counter = canvas.getByTestId("numberfield-focus-counter");
    await expect(counter).toHaveTextContent("focus=0 blur=0");
    await userEvent.click(input);
    await waitFor(() =>
      expect(counter).toHaveTextContent("focus=1 blur=0"),
    );
    // Blur by tabbing away.
    await userEvent.tab();
    await waitFor(() =>
      expect(counter).toHaveTextContent("focus=1 blur=1"),
    );
  },
};

function ConsumerFocusInline() {
  const [focusCount, setFocusCount] = useState(0);
  const [blurCount, setBlurCount] = useState(0);
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Consumer focus handlers"
    >
      <div className="zs-story-cell" style={{ maxWidth: "20rem" }}>
        <NumberField
          defaultValue={1}
          aria-label="Counted focus"
          data-testid="numberfield-focus-handlers"
          onFocus={() => setFocusCount((n) => n + 1)}
          onBlur={() => setBlurCount((n) => n + 1)}
        />
        <span
          className="zs-story-label"
          data-testid="numberfield-focus-counter"
        >
          {`focus=${focusCount} blur=${blurCount}`}
        </span>
      </div>
    </div>
  );
}
