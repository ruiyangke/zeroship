import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { useEffect, useRef, useState } from "react";
import { DirectionProvider } from "@base-ui/react/direction-provider";
import { Form } from "@base-ui/react/form";
import { Button, Field, Select } from "../components";

const meta: Meta<typeof Select> = {
  title: "Components/Select",
  component: Select,
  parameters: {
    layout: "fullscreen",
  },
};
export default meta;

type Story = StoryObj<typeof Select>;

const FRUITS = ["apple", "orange", "banana", "lemon"] as const;

/* ─── 1. Basic ──────────────────────────────────────────────────────── */
export const Basic: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Single-select with four options. The trigger reads like an " +
          "Input; the popup reads like a Dialog (same shadow, same " +
          "opaque base). Clicking the trigger opens the popup; arrow " +
          "keys navigate; Enter commits.",
      },
    },
  },
  render: function BasicRender() {
    const [value, setValue] = useState<string | null>(null);
    return (
      <div className="zs-story-row" role="group" aria-label="Basic">
        <div className="zs-story-cell" style={{ minWidth: "12rem" }}>
          <Select
            value={value}
            onValueChange={(v) => setValue(v)}
            placeholder="Pick a fruit"
            className="zs-select-basic"
            data-testid="select-basic"
          >
            {FRUITS.map((f) => (
              <Select.Item key={f} value={f} data-testid={`select-basic-item-${f}`}>
                {f.charAt(0).toUpperCase() + f.slice(1)}
              </Select.Item>
            ))}
          </Select>
        </div>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("combobox", { name: /pick a fruit/i });

    await userEvent.click(trigger);
    await waitFor(() =>
      expect(body.getByRole("option", { name: /orange/i })).toBeVisible(),
    );
    await userEvent.click(body.getByRole("option", { name: /orange/i }));
    await waitFor(() => expect(trigger).toHaveTextContent(/orange/i));
  },
};

/* ─── 2. WithGroups ─────────────────────────────────────────────────── */
export const WithGroups: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Items grouped by category. Group labels are aria-bound to " +
          "their group; the popup announces \"Citrus, list, 2 items\" " +
          "rather than a flat list of 5.",
      },
    },
  },
  render: function WithGroupsRender() {
    const [value, setValue] = useState<string | null>(null);
    return (
      <div className="zs-story-row" role="group" aria-label="With groups">
        <div className="zs-story-cell" style={{ minWidth: "14rem" }}>
          <Select
            value={value}
            onValueChange={(v) => setValue(v)}
            placeholder="Pick a fruit"
            data-testid="select-groups"
          >
            <Select.Group label="Citrus">
              <Select.Item value="orange">Orange</Select.Item>
              <Select.Item value="lemon">Lemon</Select.Item>
            </Select.Group>
            <Select.Separator />
            <Select.Group label="Berries">
              <Select.Item value="strawberry">Strawberry</Select.Item>
              <Select.Item value="blueberry">Blueberry</Select.Item>
              <Select.Item value="raspberry">Raspberry</Select.Item>
            </Select.Group>
          </Select>
        </div>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("combobox", { name: /pick a fruit/i });

    await userEvent.click(trigger);
    await waitFor(() => expect(body.getByText(/citrus/i)).toBeVisible());
    await expect(body.getByText(/berries/i)).toBeVisible();
    await userEvent.click(body.getByRole("option", { name: /lemon/i }));
    await waitFor(() => expect(trigger).toHaveTextContent(/lemon/i));
  },
};

/* ─── 3. AllSizes ───────────────────────────────────────────────────── */
export const AllSizes: Story = {
  name: "All sizes",
  parameters: {
    docs: {
      description: {
        story:
          "Size cascades the trigger AND the popup item rhythm. `sm` " +
          "2rem trigger / 1.75rem items, `md` 2.5rem / 1.75rem, `lg` " +
          "3rem trigger / 2.25rem items. Matches Input + Button.",
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
        <div className="zs-story-cell" key={size} style={{ minWidth: "12rem" }}>
          <span className="zs-story-label">{size.toUpperCase()}</span>
          <Select
            size={size}
            placeholder="Pick"
            data-testid={`select-size-${size}`}
          >
            {FRUITS.map((f) => (
              <Select.Item key={f} value={f}>
                {f.charAt(0).toUpperCase() + f.slice(1)}
              </Select.Item>
            ))}
          </Select>
        </div>
      ))}
    </div>
  ),
};

/* ─── 4. AllVariants ────────────────────────────────────────────────── */
export const AllVariants: Story = {
  name: "All variants",
  parameters: {
    docs: {
      description: {
        story:
          "`default` paints the trigger like a filled Input; `outline` " +
          "paints a transparent fill with a hairline border — mirrors " +
          "Input's variants.",
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
      <div className="zs-story-cell" style={{ minWidth: "12rem" }}>
        <span className="zs-story-label">Default</span>
        <Select
          variant="default"
          placeholder="Default"
          data-testid="select-variant-default"
        >
          {FRUITS.map((f) => (
            <Select.Item key={f} value={f}>
              {f.charAt(0).toUpperCase() + f.slice(1)}
            </Select.Item>
          ))}
        </Select>
      </div>
      <div className="zs-story-cell" style={{ minWidth: "12rem" }}>
        <span className="zs-story-label">Outline</span>
        <Select
          variant="outline"
          placeholder="Outline"
          data-testid="select-variant-outline"
        >
          {FRUITS.map((f) => (
            <Select.Item key={f} value={f}>
              {f.charAt(0).toUpperCase() + f.slice(1)}
            </Select.Item>
          ))}
        </Select>
      </div>
    </div>
  ),
};

/* ─── 5. Multiple ───────────────────────────────────────────────────── */
export const Multiple: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Multi-select. Checkmarks appear next to each selected item. " +
          "The trigger reads each selected value joined by commas (Base " +
          "UI default). For chip-based multi-mode UIs, prefer Combobox.",
      },
    },
  },
  render: function MultipleRender() {
    const [value, setValue] = useState<string[]>([]);
    return (
      <div className="zs-story-row" role="group" aria-label="Multiple">
        <div className="zs-story-cell" style={{ minWidth: "16rem" }}>
          <Select
            multiple
            value={value}
            onValueChange={(v) => setValue(v)}
            placeholder="Pick fruits"
            data-testid="select-multiple"
          >
            {FRUITS.map((f) => (
              <Select.Item
                key={f}
                value={f}
                data-testid={`select-multi-item-${f}`}
              >
                {f.charAt(0).toUpperCase() + f.slice(1)}
              </Select.Item>
            ))}
          </Select>
        </div>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("combobox", { name: /pick fruits/i });

    await userEvent.click(trigger);
    await waitFor(() =>
      expect(body.getByRole("option", { name: /apple/i })).toBeVisible(),
    );
    await userEvent.click(body.getByRole("option", { name: /apple/i }));
    await userEvent.click(body.getByRole("option", { name: /orange/i }));
    await waitFor(() => expect(trigger).toHaveTextContent(/apple/i));
    await expect(trigger).toHaveTextContent(/orange/i);
    // Wave-8 a11y hygiene: dismiss the popup before postVisit so axe
    // doesn't trip on Base UI's `data-base-ui-focus-guard` spans (which
    // are `tabindex=0 aria-hidden=true` by design — that's
    // floating-ui's focus trap). Closing the popup removes the guards
    // from the DOM so the Multiple story is axe-clean.
    await userEvent.keyboard("{Escape}");
    await waitFor(() =>
      expect(
        body.queryByRole("option", { name: /apple/i }),
      ).not.toBeInTheDocument(),
    );
  },
};

/* ─── 6. Disabled ───────────────────────────────────────────────────── */
export const Disabled: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "`disabled` cascades to the trigger AND prevents the popup " +
          "from opening. Cursor flips to `not-allowed`; the trigger " +
          "background drops to `--zs-input-bg-disabled`.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled">
      <div className="zs-story-cell" style={{ minWidth: "12rem" }}>
        <Select disabled placeholder="Disabled" data-testid="select-disabled">
          {FRUITS.map((f) => (
            <Select.Item key={f} value={f}>
              {f.charAt(0).toUpperCase() + f.slice(1)}
            </Select.Item>
          ))}
        </Select>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("combobox", { name: /disabled/i });

    await expect(trigger).toHaveAttribute("data-disabled");
    await userEvent.click(trigger);
    await expect(body.queryByRole("option", { name: /apple/i })).not.toBeInTheDocument();
  },
};

/* ─── 7. WithLabel ──────────────────────────────────────────────────── */
export const WithLabel: Story = {
  name: "With label (Field)",
  parameters: {
    docs: {
      description: {
        story:
          "Wrapped in a Field — label, description, and required " +
          "indicator are owned by Field. The Select cascades `size`, " +
          "`disabled`, and `required` from the enclosing Field.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With label">
      <div className="zs-story-cell" style={{ minWidth: "16rem" }}>
        <Field>
          <Field.Label>Favorite fruit</Field.Label>
          <Select placeholder="Pick one" data-testid="select-withlabel">
            {FRUITS.map((f) => (
              <Select.Item key={f} value={f}>
                {f.charAt(0).toUpperCase() + f.slice(1)}
              </Select.Item>
            ))}
          </Select>
          <Field.Description>We'll only ask once.</Field.Description>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 8. Required ───────────────────────────────────────────────────── */
export const Required: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "`<Field required>` cascades to the Select's hidden " +
          "submission input. Submitting empty fires `valueMissing`; " +
          "Base UI flips aria-invalid on the trigger and renders the " +
          "Field.Error.",
      },
    },
  },
  render: function RequiredRender() {
    return (
      <div className="zs-story-row" role="group" aria-label="Required">
        <div className="zs-story-cell" style={{ minWidth: "16rem" }}>
          <Form
            onSubmit={(e) => {
              e.preventDefault();
            }}
          >
            <Field required>
              <Field.Label>
                Favorite fruit <Field.Required />
              </Field.Label>
              <Select
                name="fruit"
                placeholder="Pick one"
                data-testid="select-required"
              >
                {FRUITS.map((f) => (
                  <Select.Item key={f} value={f}>
                    {f.charAt(0).toUpperCase() + f.slice(1)}
                  </Select.Item>
                ))}
              </Select>
              <Field.Error match="valueMissing">Pick a fruit.</Field.Error>
            </Field>
            <div style={{ marginTop: "0.75rem" }}>
              <Button type="submit" data-testid="select-required-submit">
                Submit
              </Button>
            </div>
          </Form>
        </div>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const submit = canvas.getByRole("button", { name: /submit/i });

    await userEvent.click(submit);
    await waitFor(() => expect(canvas.getByText("Pick a fruit.")).toBeVisible());
  },
};

/* ─── 8b. Required + Field.Error — POST-SUBMIT VISUAL EVIDENCE ─────── */
export const RequiredInvalid: Story = {
  name: "Required — post-submit (invalid)",
  parameters: {
    docs: {
      description: {
        story:
          "Companion to `Required` that auto-submits on mount so the " +
          "capture lands in the validation-failed state. Red error text " +
          "appears below the select trigger. The Form's onSubmit " +
          "preventDefault's so nothing navigates.",
      },
    },
  },
  render: function RequiredInvalidRender() {
    const submitRef = useRef<HTMLElement>(null);
    useEffect(() => {
      const id = requestAnimationFrame(() => {
        requestAnimationFrame(() => {
          submitRef.current?.click();
        });
      });
      return () => cancelAnimationFrame(id);
    }, []);
    return (
      <div className="zs-story-row" role="group" aria-label="Required select (invalid)">
        <div className="zs-story-cell" style={{ minWidth: "16rem" }}>
          <Form
            onSubmit={(e) => {
              e.preventDefault();
            }}
            data-testid="select-required-invalid-form"
          >
            <Field required>
              <Field.Label>
                Favorite fruit <Field.Required />
              </Field.Label>
              <Select
                name="fruit"
                placeholder="Pick one"
                data-testid="select-required-invalid"
              >
                {FRUITS.map((f) => (
                  <Select.Item key={f} value={f}>
                    {f.charAt(0).toUpperCase() + f.slice(1)}
                  </Select.Item>
                ))}
              </Select>
              <Field.Error match="valueMissing">Pick a fruit.</Field.Error>
            </Field>
            <div style={{ marginTop: "0.75rem" }}>
              <Button
                ref={submitRef}
                type="submit"
                data-testid="select-required-invalid-submit"
              >
                Submit
              </Button>
            </div>
          </Form>
        </div>
      </div>
    );
  },
};

/* ─── 9. LongList ───────────────────────────────────────────────────── */
export const LongList: Story = {
  name: "Long list",
  parameters: {
    docs: {
      description: {
        story:
          "50 options exercise the popup's scroll-area and keyboard " +
          "roving. The popup caps at `min(50dvb, 24rem)` and overflows " +
          "vertically; arrow keys scroll into view.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Long list">
      <div className="zs-story-cell" style={{ minWidth: "14rem" }}>
        <Select placeholder="Pick a number" data-testid="select-long-list">
          {Array.from({ length: 50 }, (_, i) => (
            <Select.Item key={i} value={`opt-${i + 1}`}>
              Option {i + 1}
            </Select.Item>
          ))}
        </Select>
      </div>
    </div>
  ),
};

/* ─── 10. Align ─────────────────────────────────────────────────────── */
export const AlignStartCenterEnd: Story = {
  name: "Align — start / center / end",
  parameters: {
    docs: {
      description: {
        story:
          "Popup alignment relative to the trigger's start, center, or " +
          "end edge. The default is `start`. `end` is useful for right- " +
          "aligned form controls; `center` for stand-alone selectors in " +
          "a centered column.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Align"
      style={{ flexDirection: "column", alignItems: "stretch" }}
    >
      {(["start", "center", "end"] as const).map((align) => (
        <div className="zs-story-cell" key={align} style={{ minWidth: "12rem" }}>
          <span className="zs-story-label">{align}</span>
          <Select
            align={align}
            placeholder={align}
            data-testid={`select-align-${align}`}
          >
            {FRUITS.map((f) => (
              <Select.Item key={f} value={f}>
                {f.charAt(0).toUpperCase() + f.slice(1)}
              </Select.Item>
            ))}
          </Select>
        </div>
      ))}
    </div>
  ),
};

/* ─── 11. Placement ─────────────────────────────────────────────────── */
export const Placement: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "`placement` anchors the popup above (`top`) or below " +
          "(`bottom`, default) the trigger. Base UI auto-flips when " +
          "there isn't room — the prop is the PREFERRED side.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Placement"
      style={{ alignItems: "flex-end", minHeight: "20rem" }}
    >
      <div className="zs-story-cell" style={{ minWidth: "12rem" }}>
        <span className="zs-story-label">Top</span>
        <Select
          placement="top"
          placeholder="Top"
          data-testid="select-placement-top"
        >
          {FRUITS.map((f) => (
            <Select.Item key={f} value={f}>
              {f.charAt(0).toUpperCase() + f.slice(1)}
            </Select.Item>
          ))}
        </Select>
      </div>
      <div className="zs-story-cell" style={{ minWidth: "12rem" }}>
        <span className="zs-story-label">Bottom</span>
        <Select
          placement="bottom"
          placeholder="Bottom"
          data-testid="select-placement-bottom"
        >
          {FRUITS.map((f) => (
            <Select.Item key={f} value={f}>
              {f.charAt(0).toUpperCase() + f.slice(1)}
            </Select.Item>
          ))}
        </Select>
      </div>
    </div>
  ),
};

/* ─── 12. RTL ───────────────────────────────────────────────────────── *
 *
 * Wave-8 review-fix 🟡: pre-fix the story only set `dir="rtl"` on a
 * wrapper `<div>`. Base UI's Floating UI positioner reads its direction
 * from `DirectionContext` (seeded by `DirectionProvider`), NOT from
 * the inherited DOM `dir` attribute — the popup portals to
 * `document.body` and the `dir` never propagates across that boundary.
 * Slice 11 documented this exact failure mode in `Menu.stories.tsx`.
 * Without the provider, this story did NOT prove popup alignment
 * flipped to the visual right under RTL — `start` resolved against
 * the LTR axis and the popup anchored to the visual left. We now wrap
 * the row in `<DirectionProvider direction="rtl">` AND open the popup
 * in a play() assertion that proves the trigger renders the chevron
 * on the inline-end (visual left) under RTL. */
export const RTL: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Shows this component behavior with realistic content and keeps the edge case easy to inspect.",
      },
    },
  },
  render: () => (
    <DirectionProvider direction="rtl">
      <div
        dir="rtl"
        lang="he"
        className="zs-story-row"
        role="group"
        aria-label="RTL"
      >
        <div className="zs-story-cell" style={{ minWidth: "16rem" }}>
          <Select placeholder="בחר פרי" data-testid="select-rtl">
            <Select.Item value="mango" data-testid="select-rtl-item-mango">
              מנגו
            </Select.Item>
            <Select.Item value="orange" data-testid="select-rtl-item-orange">
              תפוז
            </Select.Item>
            <Select.Item value="banana" data-testid="select-rtl-item-banana">
              בננה
            </Select.Item>
            <Select.Item value="lemon" data-testid="select-rtl-item-lemon">
              לימון
            </Select.Item>
          </Select>
        </div>
      </div>
    </DirectionProvider>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("combobox", { name: /בחר פרי/ });

    await userEvent.click(trigger);
    await waitFor(() =>
      expect(body.getByTestId("select-rtl-item-mango")).toBeVisible(),
    );
    // Wave-8 a11y hygiene: close the popup before postVisit so axe
    // doesn't trip on Base UI's `data-base-ui-focus-guard` spans.
    await userEvent.keyboard("{Escape}");
    // On close, Base UI's Select KEEPS the popup mounted but moves it
    // into a `hidden` subtree (the option's ancestor `<div hidden>`),
    // which drops it from the accessibility tree without removing the
    // DOM node. So `queryByTestId(...).not.toBeInTheDocument()` is the
    // wrong contract — the testid node persists. Assert the dismissal
    // the way the a11y tree sees it (the option is no longer an
    // exposed, visible `option`) — the same shape the Multiple story
    // uses with `queryByRole(...).not.toBeInTheDocument()`. This proves
    // the popup closed and the focus-guard spans are gone for axe.
    await waitFor(() =>
      expect(
        body.queryByRole("option", { name: /מנגו/ }),
      ).not.toBeInTheDocument(),
    );
  },
};

/* ─── 13. FieldAriaAutowiring — wave-8 🔴 #1 regression ──────────────── *
 *
 * Inside a `<Field>` with `<Field.Label>` and `<Field.Description>`,
 * the consumer passes NO `aria-label` / `aria-labelledby` /
 * `aria-describedby` on `<Select>`. Pre-fix the wrapper stamped
 * `aria-labelledby={undefined}` and `aria-describedby={undefined}` on
 * `<BaseSelect.Trigger>`. Base UI's `mergeProps` treated the explicit
 * `undefined`s as overrides and clobbered the ids the Field bridge had
 * auto-wired through `resolveAriaLabelledBy` (label) and
 * `validation.getValidationProps` (description). The focused trigger
 * then had no accessible name. Post-fix the wrapper only spreads
 * aria-* keys when they are actually defined; this story is the
 * positive regression hook (`aria-labelledby` and `aria-describedby`
 * must BOTH be non-empty on the trigger). */
export const FieldAriaAutowiring: Story = {
  name: "Field auto-wires labelledby + describedby (no consumer aria-*)",
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
      aria-label="Field aria autowiring"
    >
      <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
        <Field>
          <Field.Label>Favorite fruit (field-wired)</Field.Label>
          <Select
            placeholder="Pick one"
            data-testid="select-field-aria-autowiring"
          >
            {FRUITS.map((f) => (
              <Select.Item key={f} value={f}>
                {f.charAt(0).toUpperCase() + f.slice(1)}
              </Select.Item>
            ))}
          </Select>
          <Field.Description data-testid="select-field-aria-autowiring-desc">
            Field-wired description.
          </Field.Description>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 14. AriaDescribedByMerge — caller id UNIONS with Field ids ──── *
 *
 * Wave-8 review-fix 🔴 #1 second leg: when the consumer DOES pass an
 * explicit `aria-describedby` AND the Select sits inside a `<Field>`
 * with `<Field.Description>`, both ids must end up on the trigger so
 * external help-text doesn't replace Field's auto-wired description.
 * Pre-fix the wrapper passed the caller id straight through
 * `elementProps`; `mergeProps` clobbered Base UI's
 * `validation.getValidationProps` result with it. Post-fix the render
 * callback reads the auto-wired value AFTER the validation merge and
 * unions it with the caller's id(s). */
export const AriaDescribedByMerge: Story = {
  name: "aria-describedby unions caller id with Field-wired ids",
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
      aria-label="aria-describedby merge"
    >
      <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
        <span
          id="select-external-help"
          data-testid="select-described-by-merge-external"
          style={{ display: "block", marginBlockEnd: "0.5rem" }}
        >
          External help text.
        </span>
        <Field>
          <Field.Label>Favorite fruit (described-by merge)</Field.Label>
          <Select
            placeholder="Pick one"
            aria-describedby="select-external-help"
            data-testid="select-described-by-merge"
          >
            {FRUITS.map((f) => (
              <Select.Item key={f} value={f}>
                {f.charAt(0).toUpperCase() + f.slice(1)}
              </Select.Item>
            ))}
          </Select>
          <Field.Description data-testid="select-described-by-merge-desc">
            Field-wired description.
          </Field.Description>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 15. InvalidFocusRing — wave-8 🔴 #2 regression ─────────────── *
 *
 * Pre-fix the base `[data-invalid]` selector in `Select.css` was
 * source-ordered AFTER the focus / open rules and reset the second
 * box-shadow slot to `0 0 0 0 transparent`, dropping the focus ring.
 * Post-fix `[data-invalid]:focus-visible` and
 * `[data-invalid][data-popup-open]` mirror the focus / open rules with
 * the invalid border colour, so the ring stays visible.
 *
 * The story auto-submits the enclosing `<Form>` after mount, which
 * fires the `valueMissing` validation path and stamps `data-invalid`
 * on the trigger via Base UI's Field bridge. A second rAF clicks the
 * trigger to open the popup so the trigger also carries
 * `data-popup-open`. Together those two attributes drive the
 * `[data-invalid][data-popup-open]` cascade — pre-fix that cascade
 * zeroed the ring; post-fix it preserves the focus-ring slot. The
 * aria-wiring regression asserts the computed `box-shadow` still
 * contains the ring colour. */
export const InvalidFocusRing: Story = {
  name: "Invalid + focused — focus ring stays visible",
  parameters: {
    docs: {
      description: {
        story:
          "Shows this component behavior with realistic content and keeps the edge case easy to inspect.",
      },
    },
    /*
     * The popup MUST remain open for the aria-wiring regression to read
     * the trigger's computed box-shadow under both `[data-invalid]` and
     * `[data-popup-open]`. While the popup is open, Base UI mounts its
     * `data-base-ui-focus-guard` spans (`tabindex=0 aria-hidden=true`)
     * which axe flags via `aria-hidden-focus`. The guards are part of
     * Base UI's floating-ui focus trap and are not in the design
     * system's surface — we disable just that rule here. Other Select
     * stories (Basic, Multiple, RTL, etc.) close the popup in their
     * play() so they remain axe-clean against the same rule. */
    a11y: {
      config: {
        rules: [{ id: "aria-hidden-focus", enabled: false }],
      },
    },
  },
  render: function InvalidFocusRingRender() {
    const submitRef = useRef<HTMLElement>(null);
    useEffect(() => {
      // Two-step rAF: first submit to trigger `valueMissing` (which
      // stamps `data-invalid` on the trigger), then click the trigger
      // to open the popup (which stamps `data-popup-open`). A small
      // setTimeout between steps lets Base UI's validity commit flush
      // before we open.
      const submitId = requestAnimationFrame(() => {
        requestAnimationFrame(() => {
          submitRef.current?.click();
          setTimeout(() => {
            const trigger = document.querySelector(
              '[data-testid="select-invalid-focus-ring"]',
            );
            if (trigger instanceof HTMLElement) trigger.click();
          }, 120);
        });
      });
      return () => cancelAnimationFrame(submitId);
    }, []);
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Invalid focus ring"
      >
        <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
          <Form
            onSubmit={(e) => {
              e.preventDefault();
            }}
            data-testid="select-invalid-focus-ring-form"
          >
            <Field required>
              <Field.Label>
                Favorite fruit (invalid) <Field.Required />
              </Field.Label>
              <Select
                name="fruit"
                placeholder="Pick one"
                data-testid="select-invalid-focus-ring"
              >
                {FRUITS.map((f) => (
                  <Select.Item key={f} value={f}>
                    {f.charAt(0).toUpperCase() + f.slice(1)}
                  </Select.Item>
                ))}
              </Select>
              <Field.Error match="valueMissing">Pick a fruit.</Field.Error>
            </Field>
            <div style={{ marginTop: "0.75rem" }}>
              <Button
                ref={submitRef}
                type="submit"
                data-testid="select-invalid-focus-ring-submit"
              >
                Submit
              </Button>
            </div>
          </Form>
        </div>
      </div>
    );
  },
};
