import type { Meta, StoryObj } from "@storybook/react";
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
          <form onSubmit={(e) => e.preventDefault()}>
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
          </form>
        </div>
      </div>
    );
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
          "Regression hook for Slice-6 review-fix 4: aria-* on `<Combobox>` " +
          "lands on the focusable `<input>`, not on the surrounding " +
          "`<div role=\"group\">`.",
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
};
