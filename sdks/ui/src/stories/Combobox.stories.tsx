import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { useEffect, useRef, useState } from "react";
import { Form } from "@base-ui/react/form";
import { Button, Combobox, Field } from "../components";

const meta: Meta<typeof Combobox> = {
  title: "Components/Combobox",
  component: Combobox,
  parameters: {
    layout: "fullscreen",
  },
};
export default meta;

type Story = StoryObj<typeof Combobox>;

const FRUITS = [
  "apple",
  "orange",
  "banana",
  "lemon",
  "grape",
  "kiwi",
  "mango",
  "papaya",
  "peach",
  "pear",
] as const;

/* ─── 1. Basic ──────────────────────────────────────────────────────── */
export const Basic: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Single-select combobox. Type to filter, ↓/↑ to navigate, " +
          "Enter to commit, ESC to close.",
      },
    },
  },
  render: function BasicRender() {
    const [value, setValue] = useState<string | null>(null);
    return (
      <div className="zs-story-row" role="group" aria-label="Basic">
        <div className="zs-story-cell" style={{ minWidth: "16rem" }}>
          <Combobox
            value={value}
            onValueChange={(v) => setValue(v)}
            placeholder="Type a fruit"
            aria-label="Fruit search"
            items={FRUITS as unknown as string[]}
            data-testid="combobox-basic"
          >
            {(item: string) => (
              <Combobox.Item
                key={item}
                value={item}
                data-testid={`combobox-basic-item-${item}`}
              >
                {item.charAt(0).toUpperCase() + item.slice(1)}
              </Combobox.Item>
            )}
          </Combobox>
        </div>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const input = canvas.getByRole("combobox", { name: /fruit search/i });

    await userEvent.click(input);
    await userEvent.type(input, "man");
    await waitFor(() =>
      expect(body.getByRole("option", { name: /mango/i })).toBeVisible(),
    );
    // Commit via the keyboard (ArrowDown highlights the first match,
    // Enter selects it). Base UI's `<Combobox.Item>` calls
    // `event.preventDefault()` in `onPointerDownCapture`, which cancels
    // the compatibility click that testing-library's `userEvent.click`
    // relies on — so a synthetic option click never reaches the item's
    // `onClick` → `commitSelection` path and the input keeps the typed
    // query. Keyboard selection is the documented interaction and the
    // path AT users take; it commits reliably.
    await userEvent.keyboard("{ArrowDown}{Enter}");
    await waitFor(() => expect(input).toHaveValue("mango"));
  },
};

/* ─── 2. Multiple ───────────────────────────────────────────────────── */
export const Multiple: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Multi-mode renders selected items as chips inside the input " +
          "row. Click a chip's X to remove; Backspace from the empty " +
          "input removes the trailing chip (Base UI default).",
      },
    },
  },
  render: function MultipleRender() {
    const [value, setValue] = useState<string[]>(["apple", "orange"]);
    return (
      <div className="zs-story-row" role="group" aria-label="Multiple">
        <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
          <Combobox
            multiple
            value={value}
            onValueChange={(v) => setValue(v)}
            placeholder="Pick fruits"
            aria-label="Fruit multiselect"
            items={FRUITS as unknown as string[]}
            data-testid="combobox-multiple"
          >
            {(item: string) => (
              <Combobox.Item key={item} value={item}>
                {item.charAt(0).toUpperCase() + item.slice(1)}
              </Combobox.Item>
            )}
          </Combobox>
        </div>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const input = canvas.getByRole("combobox", { name: /fruit multiselect/i });
    const [removeApple] = canvas.getAllByRole("button", { name: /^remove$/i });

    await expect(canvas.getByText("apple")).toBeVisible();
    await userEvent.click(removeApple);
    await waitFor(() =>
      expect(canvas.queryByText("apple")).not.toBeInTheDocument(),
    );

    await userEvent.click(input);
    await userEvent.type(input, "pea");
    await waitFor(() =>
      expect(body.getByRole("option", { name: /peach/i })).toBeVisible(),
    );
    // Commit via keyboard (ArrowDown highlights "Peach", the first
    // match, then Enter selects it). Base UI's `<Combobox.Item>`
    // `preventDefault()`s in `onPointerDownCapture`, cancelling the
    // compatibility click `userEvent.click` depends on — a synthetic
    // option click never reaches `commitSelection`. Keyboard selection
    // is the AT path and commits the chip reliably.
    await userEvent.keyboard("{ArrowDown}{Enter}");
    await waitFor(() => expect(canvas.getByText("peach")).toBeVisible());
  },
};

/* ─── 3. AllSizes ───────────────────────────────────────────────────── */
export const AllSizes: Story = {
  name: "All sizes",
  parameters: {
    docs: {
      description: {
        story: "sm 32 / md 40 (default) / lg 48 — mirror Input + Select.",
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
        <div className="zs-story-cell" key={size} style={{ minWidth: "16rem" }}>
          <span className="zs-story-label">{size.toUpperCase()}</span>
          <Combobox
            size={size}
            placeholder="Type"
            items={FRUITS as unknown as string[]}
            data-testid={`combobox-size-${size}`}
          >
            {(item: string) => (
              <Combobox.Item key={item} value={item}>
                {item.charAt(0).toUpperCase() + item.slice(1)}
              </Combobox.Item>
            )}
          </Combobox>
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
          "`default` paints the input row like a filled Input; " +
          "`outline` paints a transparent fill with a hairline border.",
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
      <div className="zs-story-cell" style={{ minWidth: "16rem" }}>
        <span className="zs-story-label">Default</span>
        <Combobox
          variant="default"
          placeholder="Default"
          items={FRUITS as unknown as string[]}
          data-testid="combobox-variant-default"
        >
          {(item: string) => (
            <Combobox.Item key={item} value={item}>
              {item.charAt(0).toUpperCase() + item.slice(1)}
            </Combobox.Item>
          )}
        </Combobox>
      </div>
      <div className="zs-story-cell" style={{ minWidth: "16rem" }}>
        <span className="zs-story-label">Outline</span>
        <Combobox
          variant="outline"
          placeholder="Outline"
          items={FRUITS as unknown as string[]}
          data-testid="combobox-variant-outline"
        >
          {(item: string) => (
            <Combobox.Item key={item} value={item}>
              {item.charAt(0).toUpperCase() + item.slice(1)}
            </Combobox.Item>
          )}
        </Combobox>
      </div>
    </div>
  ),
};

/* ─── 5. Empty ──────────────────────────────────────────────────────── */
export const Empty: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "`Combobox.Empty` renders a custom 'no results' message when " +
          "the filter excludes every item. Base UI mounts this " +
          "component persistently and toggles aria-live; its root must " +
          "stay in the DOM so the SR announces it.",
      },
    },
  },
  render: function EmptyRender() {
    const [value, setValue] = useState<string | null>(null);
    return (
      <div className="zs-story-row" role="group" aria-label="Empty">
        <div className="zs-story-cell" style={{ minWidth: "16rem" }}>
          <Combobox
            value={value}
            onValueChange={(v) => setValue(v)}
            placeholder="Type 'xyz'"
            aria-label="Empty fruit search"
            items={FRUITS as unknown as string[]}
            data-testid="combobox-empty"
            empty={
              <Combobox.Empty data-testid="combobox-empty-sentinel">
                No fruits match.
              </Combobox.Empty>
            }
          >
            {(item: string) => (
              <Combobox.Item key={item} value={item}>
                {item.charAt(0).toUpperCase() + item.slice(1)}
              </Combobox.Item>
            )}
          </Combobox>
        </div>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const input = canvas.getByRole("combobox", { name: /empty fruit search/i });

    await userEvent.click(input);
    await userEvent.type(input, "xyz");
    await waitFor(() => expect(body.getByText(/no fruits match/i)).toBeVisible());
  },
};

/* ─── 6. WithLabel ──────────────────────────────────────────────────── */
export const WithLabel: Story = {
  name: "With label (Field)",
  parameters: {
    docs: {
      description: {
        story:
          "Wrapped in a Field. Field.Label, Field.Description, and " +
          "Field.Required all attach to the combobox input via Base UI " +
          "auto-wiring.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With label">
      <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
        <Field>
          <Field.Label>Favorite fruit</Field.Label>
          <Combobox
            placeholder="Type a fruit"
            items={FRUITS as unknown as string[]}
            data-testid="combobox-withlabel"
          >
            {(item: string) => (
              <Combobox.Item key={item} value={item}>
                {item.charAt(0).toUpperCase() + item.slice(1)}
              </Combobox.Item>
            )}
          </Combobox>
          <Field.Description>
            Start typing to filter the list.
          </Field.Description>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 7. Required ───────────────────────────────────────────────────── */
export const Required: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "`<Field required>` cascades to the combobox. Submitting " +
          "empty fires `valueMissing`; Field.Error appears below.",
      },
    },
  },
  render: function RequiredRender() {
    return (
      <div className="zs-story-row" role="group" aria-label="Required">
        <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
          <Form onSubmit={(e) => e.preventDefault()}>
            <Field required>
              <Field.Label>
                Favorite fruit <Field.Required />
              </Field.Label>
              <Combobox
                name="fruit"
                placeholder="Type"
                items={FRUITS as unknown as string[]}
                data-testid="combobox-required"
              >
                {(item: string) => (
                  <Combobox.Item key={item} value={item}>
                    {item.charAt(0).toUpperCase() + item.slice(1)}
                  </Combobox.Item>
                )}
              </Combobox>
              <Field.Error match="valueMissing">Pick a fruit.</Field.Error>
            </Field>
            <div style={{ marginTop: "0.75rem" }}>
              <Button type="submit" data-testid="combobox-required-submit">
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

/* ─── 7b. Required + Field.Error — POST-SUBMIT VISUAL EVIDENCE ─────── */
export const RequiredInvalid: Story = {
  name: "Required — post-submit (invalid)",
  parameters: {
    docs: {
      description: {
        story:
          "Companion to `Required` that auto-submits on mount so the " +
          "capture lands in the validation-failed state. Red error text " +
          "appears below the combobox. The Form's onSubmit " +
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
      <div className="zs-story-row" role="group" aria-label="Required combobox (invalid)">
        <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
          <Form onSubmit={(e) => e.preventDefault()} data-testid="combobox-required-invalid-form">
            <Field required>
              <Field.Label>
                Favorite fruit <Field.Required />
              </Field.Label>
              <Combobox
                name="fruit"
                placeholder="Type"
                items={FRUITS as unknown as string[]}
                data-testid="combobox-required-invalid"
              >
                {(item: string) => (
                  <Combobox.Item key={item} value={item}>
                    {item.charAt(0).toUpperCase() + item.slice(1)}
                  </Combobox.Item>
                )}
              </Combobox>
              <Field.Error match="valueMissing">Pick a fruit.</Field.Error>
            </Field>
            <div style={{ marginTop: "0.75rem" }}>
              <Button
                ref={submitRef}
                type="submit"
                data-testid="combobox-required-invalid-submit"
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

/* ─── 8. LongList ───────────────────────────────────────────────────── */
export const LongList: Story = {
  name: "Long list",
  parameters: {
    docs: {
      description: {
        story:
          "100 items — type to filter narrows the visible set quickly. " +
          "The popup caps at min(50dvb, 24rem) and scrolls.",
      },
    },
  },
  render: () => {
    const items = Array.from({ length: 100 }, (_, i) => `option-${i + 1}`);
    return (
      <div className="zs-story-row" role="group" aria-label="Long list">
        <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
          <Combobox
            placeholder="Type to filter"
            items={items}
            data-testid="combobox-long-list"
          >
            {(v: string) => (
              <Combobox.Item key={v} value={v}>
                {v}
              </Combobox.Item>
            )}
          </Combobox>
        </div>
      </div>
    );
  },
};

/* ─── 9. Disabled ───────────────────────────────────────────────────── */
export const Disabled: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "`disabled` greys the input row and prevents both typing and " +
          "popup opening.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled">
      <div className="zs-story-cell" style={{ minWidth: "16rem" }}>
        <Combobox
          disabled
          placeholder="Disabled"
          aria-label="Disabled fruit"
          items={FRUITS as unknown as string[]}
          data-testid="combobox-disabled"
        >
          {(item: string) => (
            <Combobox.Item key={item} value={item}>
              {item.charAt(0).toUpperCase() + item.slice(1)}
            </Combobox.Item>
          )}
        </Combobox>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const input = canvas.getByRole("combobox", { name: /disabled fruit/i });

    await expect(input).toBeDisabled();
    await userEvent.click(input);
    await expect(body.queryByRole("option", { name: /apple/i })).not.toBeInTheDocument();
  },
};

/* ─── 10. RTL ───────────────────────────────────────────────────────── */
export const RTL: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew labels in an RTL container. Logical properties carry " +
          "the layout flip — the chevron moves to the inline-end (visual " +
          "left); chip remove buttons follow the same flip.",
      },
    },
  },
  render: () => (
    <div dir="rtl" className="zs-story-row" role="group" aria-label="RTL">
      <div className="zs-story-cell" style={{ minWidth: "16rem" }}>
        <Combobox
          placeholder="הקלד פרי"
          items={["תפוח", "תפוז", "בננה", "לימון"]}
          data-testid="combobox-rtl"
        >
          {(v: string) => (
            <Combobox.Item key={v} value={v}>
              {v}
            </Combobox.Item>
          )}
        </Combobox>
      </div>
    </div>
  ),
};

/* ─── 11. AriaPropagation ──────────────────────────────────────────── *
 *
 * Slice-6 review-fix regression: a consumer-passed `aria-label` MUST
 * propagate to the focusable `<input>` (NOT the InputGroup `<div
 * role="group">`). Pre-fix the wrapper stamped aria-* on InputGroup; the
 * focused control therefore had no accessible name. The aria-wiring
 * script queries `input[aria-label="Pick a fruit"]` to prove the post-
 * fix wiring lands on the right element. */
export const AriaPropagation: Story = {
  name: "Aria-* propagates to input",
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
      <div className="zs-story-cell" style={{ minWidth: "16rem" }}>
        <Combobox
          placeholder="Type a fruit"
          items={FRUITS as unknown as string[]}
          aria-label="Pick a fruit"
          data-testid="combobox-aria-propagation"
        >
          {(item: string) => (
            <Combobox.Item key={item} value={item}>
              {item.charAt(0).toUpperCase() + item.slice(1)}
            </Combobox.Item>
          )}
        </Combobox>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const input = canvas.getByRole("combobox", { name: /pick a fruit/i });

    await userEvent.type(input, "ap");
    await expect(input).toHaveValue("ap");
  },
};

/* ─── 12. FieldAriaAutowiring ──────────────────────────────────────── *
 *
 * Wave-6 review-fix A regression: inside a `<Field>` with `<Field.Label>`
 * and `<Field.Description>`, the consumer passes NO `aria-label` /
 * `aria-labelledby` / `aria-describedby` on `<Combobox>`. Pre-fix the
 * wrapper unconditionally stamped `aria-labelledby={undefined}` and
 * `aria-describedby={undefined}` on the Input, which clobbered the ids
 * Base UI's Field bridge had auto-wired through `mergeProps`. The
 * focused input then had no accessible name. The aria-wiring script
 * resolves the input by its accessible name (`/favorite fruit/i`,
 * supplied by `<Field.Label>`) and asserts BOTH `aria-labelledby` and
 * `aria-describedby` are non-empty on the input. */
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
          <Combobox
            placeholder="Type a fruit"
            items={FRUITS as unknown as string[]}
            data-testid="combobox-field-aria-autowiring"
          >
            {(item: string) => (
              <Combobox.Item key={item} value={item}>
                {item.charAt(0).toUpperCase() + item.slice(1)}
              </Combobox.Item>
            )}
          </Combobox>
          <Field.Description data-testid="combobox-field-aria-autowiring-desc">
            Field-wired description.
          </Field.Description>
        </Field>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const input = canvas.getByRole("combobox", {
      name: /favorite fruit \(field-wired\)/i,
    });
    // jest-dom's `toHaveAttribute(name, value)` compares the attribute
    // string against `value` with strict equality unless `value` is an
    // asymmetric matcher — a bare RegExp is NOT one, so
    // `toHaveAttribute("aria-labelledby", /\S+/)` reduces to
    // `getAttribute(...) === /\S+/`, which is always false even when the
    // id is correctly wired. Use `expect.stringMatching` (a real
    // asymmetric matcher) so jest-dom pattern-matches the value: this
    // asserts the auto-wired id is present AND non-empty.
    await expect(input).toHaveAttribute(
      "aria-labelledby",
      expect.stringMatching(/\S+/),
    );
    await expect(input).toHaveAttribute(
      "aria-describedby",
      expect.stringMatching(/\S+/),
    );
  },
};

/* ─── 13. PlaceholderFallback ──────────────────────────────────────── *
 *
 * Wave-6 review-fix B regression: a standalone `<Combobox>` with NO
 * Field, NO `aria-label`, and NO `aria-labelledby` must still expose an
 * accessible name. Placeholder text is not an accessible name on its
 * own (axe `aria-input-field-name`), so the wrapper promotes
 * `placeholder` → `aria-label` as a last-resort fallback. The
 * aria-wiring script resolves the input by `role="combobox"` + name
 * matching the placeholder string. */
export const PlaceholderFallback: Story = {
  name: "Placeholder falls back to aria-label (no Field, no aria-*)",
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
      aria-label="Placeholder fallback"
    >
      <div className="zs-story-cell" style={{ minWidth: "16rem" }}>
        <Combobox
          placeholder="Search fruits"
          items={FRUITS as unknown as string[]}
          data-testid="combobox-placeholder-fallback"
        >
          {(item: string) => (
            <Combobox.Item key={item} value={item}>
              {item.charAt(0).toUpperCase() + item.slice(1)}
            </Combobox.Item>
          )}
        </Combobox>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const input = canvas.getByRole("combobox", { name: /search fruits/i });
    await expect(input).toHaveAttribute("aria-label", "Search fruits");
  },
};
