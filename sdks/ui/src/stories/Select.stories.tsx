import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { useEffect, useRef, useState } from "react";
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
          <form
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
          </form>
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

/* ─── 12. RTL ───────────────────────────────────────────────────────── */
export const RTL: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew labels in an RTL container. Logical properties carry " +
          "the layout flip — the chevron moves to the inline-end (visual " +
          "left); start-aligned popups now align to the visual right.",
      },
    },
  },
  render: () => (
    <div dir="rtl" className="zs-story-row" role="group" aria-label="RTL">
      <div className="zs-story-cell" style={{ minWidth: "16rem" }}>
        <Select placeholder="בחר פרי" data-testid="select-rtl">
          <Select.Item value="apple">תפוח</Select.Item>
          <Select.Item value="orange">תפוז</Select.Item>
          <Select.Item value="banana">בננה</Select.Item>
          <Select.Item value="lemon">לימון</Select.Item>
        </Select>
      </div>
    </div>
  ),
};
