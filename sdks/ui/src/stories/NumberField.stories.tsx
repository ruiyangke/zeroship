import type { Meta, StoryObj } from "@storybook/react";
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
          "Two NumberFields side by side: a normal required-marker field " +
          "(rest state, asterisk on the label) and an empty one inside a " +
          "form that auto-submits on first paint so the post-submit invalid " +
          "styling lands in the screenshot. Mirrors the Slice 4 Required-" +
          "invalid pattern.",
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
};

function RequiredInvalidInline() {
  return (
    <form
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
    </form>
  );
}

/* ─── 10. RTL ──────────────────────────────────────────────────────── */
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

