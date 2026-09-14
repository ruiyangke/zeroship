import type { Meta, StoryObj } from "@storybook/react";
import { useState } from "react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { Checkbox, CheckboxGroup, Fieldset } from "../components";

const meta: Meta<typeof CheckboxGroup> = {
  title: "Components/CheckboxGroup",
  component: CheckboxGroup,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof CheckboxGroup>;

const NEWSLETTER_OPTIONS: Array<{ value: string; label: string }> = [
  { value: "newsletter", label: "Newsletter" },
  { value: "product", label: "Product updates" },
  { value: "beta", label: "Beta invites" },
  { value: "events", label: "Events and meetups" },
];

/* ─── 1. Basic ──────────────────────────────────────────────────────── */
export const Basic: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Uncontrolled CheckboxGroup. Each Checkbox child carries a " +
          "`name`; the group manages the value array via Base UI's " +
          "CheckboxGroupContext. Click any chip to toggle; `Newsletter` " +
          "and `Beta invites` ship pre-checked via `defaultValue`.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Basic checkbox group"
    >
      <div className="zs-story-cell" style={{ inlineSize: "min(20rem, 100%)" }}>
        <CheckboxGroup
          defaultValue={["newsletter", "beta"]}
          data-testid="checkboxgroup-basic"
        >
          {NEWSLETTER_OPTIONS.map((opt) => (
            <Checkbox
              key={opt.value}
              name={opt.value}
              label={opt.label}
              data-testid={`checkboxgroup-basic-${opt.value}`}
            />
          ))}
        </CheckboxGroup>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const newsletter = canvas.getByRole("checkbox", { name: /newsletter/i });
    const product = canvas.getByRole("checkbox", { name: /product updates/i });
    const beta = canvas.getByRole("checkbox", { name: /beta invites/i });

    await expect(newsletter).toHaveAttribute("aria-checked", "true");
    await expect(beta).toHaveAttribute("aria-checked", "true");
    await expect(product).toHaveAttribute("aria-checked", "false");

    await userEvent.click(product);
    await waitFor(() =>
      expect(product).toHaveAttribute("aria-checked", "true"),
    );
  },
};

/* ─── 2. Controlled ─────────────────────────────────────────────────── */
export const Controlled: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Controlled CheckboxGroup — the consumer owns `value` and " +
          "patches it on every change. The displayed selection summary " +
          "below reads the live value array so consumers can see the " +
          "round-trip without DevTools.",
      },
    },
  },
  render: function ControlledRender() {
    const [value, setValue] = useState<string[]>(["product"]);
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Controlled checkbox group"
      >
        <div
          className="zs-story-cell"
          style={{
            inlineSize: "min(20rem, 100%)",
            display: "flex",
            flexDirection: "column",
            gap: "0.75rem",
          }}
        >
          <CheckboxGroup
            value={value}
            onValueChange={(next) => setValue(next)}
            data-testid="checkboxgroup-controlled"
          >
            {NEWSLETTER_OPTIONS.map((opt) => (
              <Checkbox
                key={opt.value}
                name={opt.value}
                label={opt.label}
                data-testid={`checkboxgroup-controlled-${opt.value}`}
              />
            ))}
          </CheckboxGroup>
          <span
            className="zs-story-label"
            data-testid="checkboxgroup-controlled-readout"
          >
            Selected: [{value.join(", ")}]
          </span>
        </div>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const readout = canvas.getByTestId("checkboxgroup-controlled-readout");
    await expect(readout).toHaveTextContent("Selected: [product]");
    await userEvent.click(canvas.getByRole("checkbox", { name: /newsletter/i }));
    await waitFor(() =>
      expect(readout).toHaveTextContent("Selected: [product, newsletter]"),
    );
  },
};

/* ─── 3. Disabled ───────────────────────────────────────────────────── */
export const Disabled: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "`disabled` cascades from the group to every contained " +
          "Checkbox via Base UI's CheckboxGroupContext. Children pick " +
          "up the disabled visual without per-Checkbox `disabled` props.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Disabled checkbox group"
    >
      <div className="zs-story-cell" style={{ inlineSize: "min(20rem, 100%)" }}>
        <CheckboxGroup
          disabled
          defaultValue={["newsletter"]}
          data-testid="checkboxgroup-disabled"
        >
          {NEWSLETTER_OPTIONS.map((opt) => (
            <Checkbox key={opt.value} name={opt.value} label={opt.label} />
          ))}
        </CheckboxGroup>
      </div>
    </div>
  ),
};

/* ─── 4. Horizontal ─────────────────────────────────────────────────── */
export const Horizontal: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "`orientation=\"horizontal\"` flips the layout to a wrapping " +
          "row. Useful for short option labels where vertical stacking " +
          "wastes block space. Wrap is automatic — narrow viewports flow " +
          "to a second row rather than clipping.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Horizontal checkbox group"
    >
      <div className="zs-story-cell" style={{ inlineSize: "min(26rem, 100%)" }}>
        <CheckboxGroup
          orientation="horizontal"
          defaultValue={["mon", "wed", "fri"]}
          data-testid="checkboxgroup-horizontal"
        >
          {[
            { value: "mon", label: "Mon" },
            { value: "tue", label: "Tue" },
            { value: "wed", label: "Wed" },
            { value: "thu", label: "Thu" },
            { value: "fri", label: "Fri" },
            { value: "sat", label: "Sat" },
            { value: "sun", label: "Sun" },
          ].map((opt) => (
            <Checkbox
              key={opt.value}
              name={opt.value}
              label={opt.label}
              data-testid={`checkboxgroup-horizontal-${opt.value}`}
            />
          ))}
        </CheckboxGroup>
      </div>
    </div>
  ),
};

/* ─── 5. NestedFieldset ─────────────────────────────────────────────── */
export const NestedFieldset: Story = {
  name: "Nested in Fieldset",
  parameters: {
    docs: {
      description: {
        story:
          "CheckboxGroup wrapped in a `<Fieldset>` so the group has a " +
          "<legend>-style heading. Description copy sits under the " +
          "legend; the group itself stays layout-only.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Fieldset checkbox group"
    >
      <div className="zs-story-cell" style={{ inlineSize: "min(22rem, 100%)" }}>
        <Fieldset>
          <Fieldset.Legend>Email preferences</Fieldset.Legend>
          <p className="zs-story-description">
            Pick the categories you would like to hear from us about.
          </p>
          <CheckboxGroup
            defaultValue={["newsletter"]}
            data-testid="checkboxgroup-nested"
          >
            {NEWSLETTER_OPTIONS.map((opt) => (
              <Checkbox
                key={opt.value}
                name={opt.value}
                label={opt.label}
              />
            ))}
          </CheckboxGroup>
        </Fieldset>
      </div>
    </div>
  ),
};

/* ─── 6. RTL ────────────────────────────────────────────────────────── */
export const RTL: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew labels in an RTL container. Logical properties on the " +
          "row carry the orientation flip; chips stay on the inline-start " +
          "of each item.",
      },
    },
  },
  render: () => (
    <div
      dir="rtl"
      className="zs-story-row"
      role="group"
      aria-label="RTL checkbox group"
    >
      <div className="zs-story-cell" style={{ inlineSize: "min(20rem, 100%)" }}>
        <CheckboxGroup
          defaultValue={["news-he"]}
          data-testid="checkboxgroup-rtl"
        >
          <Checkbox name="news-he" label="ניוזלטר" />
          <Checkbox name="product-he" label="עדכוני מוצר" />
          <Checkbox name="beta-he" label="הזמנות בטא" />
        </CheckboxGroup>
      </div>
    </div>
  ),
};

/* ─── 7. AllSelected ────────────────────────────────────────────────── */
export const AllSelected: Story = {
  name: "All selected",
  parameters: {
    docs: {
      description: {
        story:
          "Every option pre-ticked. The `value` arrives identical to " +
          "`allValues` order from Base UI; the checked glyphs render " +
          "consistently across themes.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="All selected checkbox group"
    >
      <div className="zs-story-cell" style={{ inlineSize: "min(20rem, 100%)" }}>
        <CheckboxGroup
          defaultValue={NEWSLETTER_OPTIONS.map((opt) => opt.value)}
          data-testid="checkboxgroup-all-selected"
        >
          {NEWSLETTER_OPTIONS.map((opt) => (
            <Checkbox key={opt.value} name={opt.value} label={opt.label} />
          ))}
        </CheckboxGroup>
      </div>
    </div>
  ),
};

/* ─── 8. NoneSelected ───────────────────────────────────────────────── */
export const NoneSelected: Story = {
  name: "None selected",
  parameters: {
    docs: {
      description: {
        story:
          "Default value omitted; the group ships empty. Useful as the " +
          "resting state for opt-in flows (consent rows).",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="None selected checkbox group"
    >
      <div className="zs-story-cell" style={{ inlineSize: "min(20rem, 100%)" }}>
        <CheckboxGroup data-testid="checkboxgroup-none-selected">
          {NEWSLETTER_OPTIONS.map((opt) => (
            <Checkbox key={opt.value} name={opt.value} label={opt.label} />
          ))}
        </CheckboxGroup>
      </div>
    </div>
  ),
};
