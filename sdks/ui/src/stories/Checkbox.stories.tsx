import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { useEffect, useMemo, useRef, useState } from "react";
import { Form } from "@base-ui/react/form";
import { CheckboxGroup } from "@base-ui/react/checkbox-group";
import { Button, Checkbox, Field } from "../components";

const meta: Meta<typeof Checkbox> = {
  title: "Components/Checkbox",
  component: Checkbox,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Checkbox>;

/* ─── 1. All states ────────────────────────────────────────────────── */
export const AllStates: Story = {
  name: "All states",
  parameters: {
    docs: {
      description: {
        story:
          "Five visual states per chip — unchecked / checked / " +
          "indeterminate / disabled / readonly. Two glyph shapes " +
          "(checkmark vs minus bar) keep the on-states distinguishable " +
          "without color alone.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All checkbox states">
      <div className="zs-story-cell">
        <span className="zs-story-label">Unchecked</span>
        <Checkbox label="Unchecked" />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Checked</span>
        <Checkbox label="Checked" defaultChecked />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Indeterminate</span>
        <Checkbox label="Indeterminate" indeterminate />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Disabled — unchecked</span>
        <Checkbox label="Disabled" disabled />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Disabled — checked</span>
        <Checkbox label="Disabled, checked" disabled defaultChecked />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Disabled — indeterminate</span>
        <Checkbox label="Disabled, indeterminate" disabled indeterminate />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Read only</span>
        <Checkbox label="Read only" readOnly defaultChecked />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const unchecked = canvas.getByRole("checkbox", { name: /^unchecked$/i });
    const checked = canvas.getByRole("checkbox", { name: /^checked$/i });
    const disabled = canvas.getByRole("checkbox", { name: /^disabled$/i });
    const readOnly = canvas.getByRole("checkbox", { name: /^read only$/i });

    await expect(unchecked).toHaveAttribute("aria-checked", "false");
    await userEvent.click(unchecked);
    await waitFor(() =>
      expect(unchecked).toHaveAttribute("aria-checked", "true"),
    );

    await expect(checked).toHaveAttribute("aria-checked", "true");
    await userEvent.click(checked);
    await waitFor(() =>
      expect(checked).toHaveAttribute("aria-checked", "false"),
    );

    await expect(disabled).toHaveAttribute("data-disabled");
    await userEvent.click(disabled);
    await expect(disabled).toHaveAttribute("aria-checked", "false");

    await expect(readOnly).toHaveAttribute("aria-checked", "true");
    await userEvent.click(readOnly);
    await expect(readOnly).toHaveAttribute("aria-checked", "true");
  },
};

/* ─── 2. All sizes ─────────────────────────────────────────────────── */
export const AllSizes: Story = {
  name: "All sizes",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All checkbox sizes">
      <div className="zs-story-cell">
        <span className="zs-story-label">Small</span>
        <Checkbox size="sm" label="Small" defaultChecked />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Medium</span>
        <Checkbox size="md" label="Medium" defaultChecked />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Large</span>
        <Checkbox size="lg" label="Large" defaultChecked />
      </div>
    </div>
  ),
};

/* ─── 3. All variants ──────────────────────────────────────────────── */
export const AllVariants: Story = {
  name: "All variants",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All checkbox variants">
      <div className="zs-story-cell">
        <span className="zs-story-label">Default — unchecked</span>
        <Checkbox label="Default" variant="default" />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Default — checked</span>
        <Checkbox label="Default" variant="default" defaultChecked />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Tinted — unchecked</span>
        <Checkbox label="Tinted" variant="tinted" />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Tinted — checked</span>
        <Checkbox label="Tinted" variant="tinted" defaultChecked />
      </div>
    </div>
  ),
};

/* ─── 4. With external label (Field integration) ─────────────────────
 *
 * Renamed from `WithLabel` (slice-4 visual-polish item 6) — the
 * original name read like a single canonical pattern, but composing
 * Field.Label ABOVE a chip is one of two valid layouts. For a single
 * boolean, the inline `label` prop story (`Inline`, below) is the
 * visually-recommended default; this story documents the external-
 * label pattern that the Required + RequiredInvalid stories build on
 * (they need the Field.Label for the `<Field.Required />` indicator).
 * Renaming makes both stories self-describe their intent.
 *
 * The aria-wiring assertion now references
 * `components-checkbox--with-external-label`. */
export const WithExternalLabel: Story = {
  name: "With external label",
  parameters: {
    docs: {
      description: {
        story:
          "A bare Checkbox inside a Field with the Field.Label rendered " +
          "ABOVE the chip. Base UI wires `htmlFor` to the hidden input the " +
          "CheckboxRoot emits, so clicking the label text toggles the chip. " +
          "Use this pattern when the boolean needs a `<Field.Required />` " +
          "indicator or a Field.Description block — both compose cleanly " +
          "above the chip. For a single boolean with no required indicator, " +
          "prefer the inline `label` prop pattern (see the `Inline` story).",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Checkbox with external label">
      <div className="zs-story-cell" style={{ maxWidth: "20rem" }}>
        <Field>
          <Field.Label>Subscribe to product emails</Field.Label>
          <Checkbox data-testid="checkbox-with-label" name="subscribe" />
        </Field>
      </div>
    </div>
  ),
};

/* ─── 4b. Inline label (recommended default for single booleans) ─────
 *
 * Slice-4 visual-polish item 6. Codex flagged the `WithLabel` story's
 * external-label pattern as composing a bold Field.Label above a lone
 * chip — visually it read like a section title with an orphaned
 * control underneath. The Checkbox component already exposes an
 * inline `label` prop that wraps chip + text into one click surface
 * via the SelectionRow helper — that's the right visual default for a
 * single boolean. This story makes the pattern discoverable. */
export const Inline: Story = {
  name: "Inline label (recommended default)",
  parameters: {
    docs: {
      description: {
        story:
          "Single-boolean default: the inline `label` prop wraps chip + " +
          "text into one `<label>` so the whole row is a click surface, " +
          "and the label sits at the chip's inline-end at the same type " +
          "size as the surrounding body. Use this for terms-acceptance, " +
          "settings toggles, anything where the boolean and its meaning " +
          "live as a single row. Reach for `WithExternalLabel` only when " +
          "you need `<Field.Required />`, a Field.Description, or another " +
          "block-level element above the chip.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Checkbox inline label">
      <div className="zs-story-cell">
        <Checkbox label="Subscribe to product emails" name="subscribe-inline" />
      </div>
      <div className="zs-story-cell">
        <Checkbox label="Make profile public" name="public-inline" defaultChecked />
      </div>
    </div>
  ),
};

/* ─── 5. With description ──────────────────────────────────────────── */
export const WithDescription: Story = {
  name: "With description",
  parameters: {
    docs: {
      description: {
        story:
          "Field.Description below the chip; Base UI extends the hidden " +
          "input's `aria-describedby` to reference the description id. " +
          "Verified by the aria-wiring assertion.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Checkbox with description">
      <div className="zs-story-cell" style={{ maxWidth: "22rem" }}>
        <Field>
          <Field.Label>Marketing emails</Field.Label>
          <Checkbox data-testid="checkbox-with-description" name="marketing" />
          <Field.Description>
            We send at most one update per week and never share your address.
          </Field.Description>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 6. Required + Field.Error ─────────────────────────────────────── */
export const Required: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Required Checkbox inside a form. Submitting without checking " +
          "triggers Field's `valueMissing` and the chip's hidden input " +
          "carries `aria-invalid=\"true\"`. The aria-wiring assertion " +
          "submits the form and verifies both invalid and the Field.Error " +
          "text become visible.",
      },
    },
  },
  render: function RequiredRender() {
    return (
      <div className="zs-story-row" role="group" aria-label="Required checkbox">
        {/* Base UI's <Form> coordinates submit-time validation across
            its child <Field>s. Without it, the native form's `invalid`
            event fires on the hidden input but Field's aria-invalid
            state machine doesn't observe the submission. Form is the
            piece that wires onSubmit → Field.validate → aria-invalid. */}
        <Form
          className="zs-story-cell"
          style={{ maxWidth: "22rem", display: "flex", flexDirection: "column", gap: "0.75rem" }}
          onSubmit={(e) => {
            // The native onSubmit still fires if validation passes;
            // we preventDefault so the story doesn't navigate.
            e.preventDefault();
          }}
          data-testid="checkbox-required-form"
        >
          <Field required>
            <Field.Label>
              I agree to the terms <Field.Required />
            </Field.Label>
            {/* `required` intentionally NOT set on the Checkbox — it
                must cascade from the Field via context. The aria-wiring
                assertion submits the form and verifies the hidden
                input's aria-invalid flips, which only happens if the
                cascade actually delivered the requiredness. Slice-4
                review fix item 7. */}
            <Checkbox
              data-testid="checkbox-required"
              name="agree"
            />
            <Field.Error match="valueMissing">
              You must agree before continuing.
            </Field.Error>
          </Field>
          <Button
            type="submit"
            variant="filled"
            data-testid="checkbox-required-submit"
          >
            Continue
          </Button>
        </Form>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const terms = canvas.getByRole("checkbox", { name: /i agree/i });
    const submit = canvas.getByRole("button", { name: /continue/i });

    await userEvent.click(submit);
    await waitFor(() =>
      expect(canvas.getByText(/you must agree before continuing/i)).toBeVisible(),
    );

    await userEvent.click(terms);
    await expect(terms).toHaveAttribute("aria-checked", "true");
  },
};

/* ─── 6b. Required + Field.Error — POST-SUBMIT VISUAL EVIDENCE ────────
 *
 * Slice-4 visual-polish item 1. The Required story above lands in the
 * pre-submit state — the chip looks innocent, no red error text below,
 * so the screenshot capture doesn't actually evidence the validation
 * UI a real user would see. The aria-wiring assertion verifies the
 * aria-invalid + Field.Error wiring already, but a screenshot reviewer
 * has no visual confirmation that the error state is built.
 *
 * This companion story renders the same form but auto-fires submit on
 * mount via a one-shot useEffect → ref.current.click(). The capture
 * script photographs the post-submit DOM: aria-invalid is on the
 * hidden input, Field.Error has rendered the "You must agree" copy.
 *
 * We use the useEffect approach rather than Storybook's `play` because
 * `play` runs after the iframe is interactable but isn't guaranteed to
 * fire during a `build-storybook` static capture across every
 * Storybook setup; an effect-driven submit is observable from the
 * very first render after mount, which is what the capture script
 * sees. The Form's onSubmit still preventDefault's so nothing
 * navigates. */
export const RequiredInvalid: Story = {
  name: "Required — post-submit (invalid)",
  parameters: {
    docs: {
      description: {
        story:
          "Companion to `Required` that auto-submits on mount so the " +
          "capture lands in the validation-failed state. Red error text " +
          "appears below the chip; the hidden input carries `aria-" +
          "invalid=\"true\"`. The auto-submit fires once via useEffect " +
          "and the Form's onSubmit preventDefault's, so the page never " +
          "navigates. Visual-polish item 1.",
      },
    },
  },
  render: function RequiredInvalidRender() {
    // Button forwards an `HTMLElement` ref (so it stays compatible
    // with the `asChild` slot route which can render an <a> etc.). The
    // story default renders a <button>, but typing the ref as
    // HTMLElement matches the component's forwardRef signature.
    const submitRef = useRef<HTMLElement>(null);
    useEffect(() => {
      // Two RAFs so Base UI's Field state machine sees the React
      // mount commit before the click — one frame for layout, one
      // for the validation callbacks to subscribe. This was robust
      // enough across the Storybook 8 capture targets we tested.
      const id = requestAnimationFrame(() => {
        requestAnimationFrame(() => {
          submitRef.current?.click();
        });
      });
      return () => cancelAnimationFrame(id);
    }, []);
    return (
      <div className="zs-story-row" role="group" aria-label="Required checkbox (invalid)">
        <Form
          className="zs-story-cell"
          style={{ maxWidth: "22rem", display: "flex", flexDirection: "column", gap: "0.75rem" }}
          onSubmit={(e) => {
            e.preventDefault();
          }}
          data-testid="checkbox-required-invalid-form"
        >
          <Field required>
            <Field.Label>
              I agree to the terms <Field.Required />
            </Field.Label>
            <Checkbox
              data-testid="checkbox-required-invalid"
              name="agree"
            />
            <Field.Error match="valueMissing">
              You must agree before continuing.
            </Field.Error>
          </Field>
          <Button
            ref={submitRef}
            type="submit"
            variant="filled"
            data-testid="checkbox-required-invalid-submit"
          >
            Continue
          </Button>
        </Form>
      </div>
    );
  },
};

/* ─── 7. Indeterminate parent ──────────────────────────────────────── */
export const IndeterminateParent: Story = {
  name: "Indeterminate parent",
  parameters: {
    docs: {
      description: {
        story:
          "Parent checkbox represents the aggregate state of three " +
          "children. When some-but-not-all are checked, the parent reads " +
          "indeterminate. Clicking the parent toggles every child to its " +
          "new shared state.",
      },
    },
  },
  render: function IndeterminateParentRender() {
    const [items, setItems] = useState({
      red: true,
      green: false,
      blue: false,
    });
    const checkedCount = useMemo(
      () => Object.values(items).filter(Boolean).length,
      [items],
    );
    const allChecked = checkedCount === 3;
    const indeterminate = checkedCount > 0 && checkedCount < 3;

    return (
      <div className="zs-story-row" role="group" aria-label="Indeterminate parent">
        <div
          className="zs-story-cell"
          style={{ maxWidth: "22rem", display: "flex", flexDirection: "column", gap: "0.5rem" }}
        >
          <Checkbox
            label="All colors"
            checked={allChecked}
            indeterminate={indeterminate}
            onCheckedChange={(next) =>
              setItems({ red: next, green: next, blue: next })
            }
          />
          <div style={{ display: "flex", flexDirection: "column", gap: "0.25rem", paddingInlineStart: "1.5rem" }}>
            <Checkbox
              label="Red"
              checked={items.red}
              onCheckedChange={(next) => setItems((s) => ({ ...s, red: next }))}
            />
            <Checkbox
              label="Green"
              checked={items.green}
              onCheckedChange={(next) => setItems((s) => ({ ...s, green: next }))}
            />
            <Checkbox
              label="Blue"
              checked={items.blue}
              onCheckedChange={(next) => setItems((s) => ({ ...s, blue: next }))}
            />
          </div>
        </div>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const allColors = canvas.getByRole("checkbox", { name: /all colors/i });
    const green = canvas.getByRole("checkbox", { name: /green/i });
    const blue = canvas.getByRole("checkbox", { name: /blue/i });

    await expect(allColors).toHaveAttribute("aria-checked", "mixed");
    await userEvent.click(allColors);
    await waitFor(() =>
      expect(allColors).toHaveAttribute("aria-checked", "true"),
    );
    await expect(green).toHaveAttribute("aria-checked", "true");
    await expect(blue).toHaveAttribute("aria-checked", "true");

    await userEvent.click(green);
    await waitFor(() =>
      expect(allColors).toHaveAttribute("aria-checked", "mixed"),
    );
  },
};

/* ─── 8. Inside a form ─────────────────────────────────────────────── */
export const InsideForm: Story = {
  name: "Inside a form",
  parameters: {
    docs: {
      description: {
        story:
          "Native form submission: an unchecked checkbox submits NO value " +
          "for its name (matching the native HTML checkbox contract); a " +
          "checked one submits `on`. The submit handler reports what the " +
          "FormData saw.",
      },
    },
  },
  render: function InsideFormRender() {
    const [submitted, setSubmitted] = useState<string>("(not submitted)");
    return (
      <div className="zs-story-row" role="group" aria-label="Inside a form">
        <form
          className="zs-story-cell"
          style={{ maxWidth: "24rem", display: "flex", flexDirection: "column", gap: "0.75rem" }}
          onSubmit={(e) => {
            e.preventDefault();
            const data = new FormData(e.currentTarget);
            const entries = Array.from(data.entries())
              .map(([k, v]) => `${k}=${String(v)}`)
              .join(", ");
            setSubmitted(entries.length === 0 ? "(empty)" : entries);
          }}
        >
          <Checkbox label="Make profile public" name="public" defaultChecked />
          <Checkbox label="Enable beta features" name="beta" />
          <Button type="submit" variant="filled">
            Save
          </Button>
          <span style={{ fontSize: "0.8125rem" }} data-testid="checkbox-form-result">
            submitted: {submitted}
          </span>
        </form>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const publicProfile = canvas.getByRole("checkbox", {
      name: /make profile public/i,
    });
    const beta = canvas.getByRole("checkbox", { name: /enable beta features/i });
    const save = canvas.getByRole("button", { name: /save/i });
    const result = canvas.getByText(/^submitted:/i);

    await expect(publicProfile).toHaveAttribute("aria-checked", "true");
    await userEvent.click(save);
    await waitFor(() => expect(result).toHaveTextContent("public=on"));

    await userEvent.click(beta);
    await userEvent.click(save);
    await waitFor(() => expect(result).toHaveTextContent("beta=on"));
  },
};

/* ─── 9. Disabled (Field cascade) ──────────────────────────────────── */
export const Disabled: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Two patterns: an explicit `disabled` prop wins, AND a " +
          "`<Field disabled>` cascades to a contained Checkbox that " +
          "doesn't set its own disabled. Inside a disabled Field the " +
          "whole row reads grayed-out together.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled cascades">
      <div className="zs-story-cell">
        <span className="zs-story-label">disabled prop</span>
        <Checkbox label="Hard-disabled" disabled />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Field disabled — chip inherits</span>
        <Field disabled>
          <Field.Label>Pending invitation</Field.Label>
          <Checkbox name="invite" />
          <Field.Description>You can't change this until your invite is accepted.</Field.Description>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 10. RTL ──────────────────────────────────────────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew label flows right-to-left; the chip stays on the inline-" +
          "start side of the row (visually right under `dir=\"rtl\"`). " +
          "Logical properties carry the flip without per-direction CSS.",
      },
    },
  },
  render: () => (
    <div dir="rtl" className="zs-story-row" role="group" aria-label="RTL checkbox row">
      <div className="zs-story-cell" style={{ maxWidth: "22rem" }}>
        <Field>
          <Field.Label>אני מסכים לתנאי השימוש</Field.Label>
          <Checkbox name="agree-rtl" />
        </Field>
      </div>
    </div>
  ),
};

/* ─── 11. Indeterminate from group (slice-4 review fix item 6) ───────
 *
 * Base UI's `CheckboxGroup` parent-of-children pattern: name the
 * parent Checkbox with `allValues=[...]` so it represents the
 * aggregate of the children. When some-but-not-all children are
 * ticked, Base UI computes `data-indeterminate` on the parent
 * WITHOUT the wrapper having to set the `indeterminate` prop. The
 * minus glyph must show — earlier the wrapper only rendered the
 * checkmark unless the explicit prop was set, so the group-computed
 * indeterminate state painted the wrong glyph. */
export const IndeterminateFromGroup: Story = {
  name: "Indeterminate from group",
  parameters: {
    docs: {
      description: {
        story:
          "CheckboxGroup with `allValues=[red,green,blue]` and only " +
          "`red` selected. The parent Checkbox uses `name=\"parent\"` " +
          "so Base UI sees it as the aggregate row; with one of three " +
          "children checked, the parent's data-indeterminate flips on " +
          "and the minus glyph paints — entirely from Base UI's " +
          "computed state, with no explicit `indeterminate` prop on " +
          "the wrapper.",
      },
    },
  },
  render: function IndeterminateFromGroupRender() {
    // Uncontrolled CheckboxGroup so Base UI owns the value array
    // entirely — that's the cleanest exercise of "parent computes
    // its indeterminate state from group context".
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Indeterminate from group"
        data-testid="indeterminate-from-group-row"
      >
        <div
          className="zs-story-cell"
          style={{ maxWidth: "22rem", display: "flex", flexDirection: "column", gap: "0.5rem" }}
        >
          <CheckboxGroup
            defaultValue={["red"]}
            allValues={["red", "green", "blue"]}
          >
            {/* The `parent` prop wires this Checkbox to Base UI's
                useCheckboxGroupParent — its checked/indeterminate
                state derives from the group's value array vs
                allValues, with no explicit `indeterminate` prop. */}
            <Checkbox
              label="All colors"
              parent
              data-testid="indeterminate-from-group-parent"
            />
            <div style={{ display: "flex", flexDirection: "column", gap: "0.25rem", paddingInlineStart: "1.5rem" }}>
              <Checkbox label="Red" name="red" />
              <Checkbox label="Green" name="green" />
              <Checkbox label="Blue" name="blue" />
            </div>
          </CheckboxGroup>
        </div>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const allColors = canvas.getByRole("checkbox", { name: /all colors/i });
    const green = canvas.getByRole("checkbox", { name: /green/i });
    const blue = canvas.getByRole("checkbox", { name: /blue/i });

    await expect(allColors).toHaveAttribute("aria-checked", "mixed");
    await userEvent.click(green);
    await waitFor(() =>
      expect(allColors).toHaveAttribute("aria-checked", "mixed"),
    );
    await userEvent.click(blue);
    await waitFor(() =>
      expect(allColors).toHaveAttribute("aria-checked", "true"),
    );
  },
};
