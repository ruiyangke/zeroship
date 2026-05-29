import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import { useEffect, useRef } from "react";
import { Form } from "@base-ui/react/form";
import { Autocomplete, Button, Field } from "../components";

const meta: Meta<typeof Autocomplete> = {
  title: "Components/Autocomplete",
  component: Autocomplete,
  parameters: {
    layout: "fullscreen",
  },
};
export default meta;

type Story = StoryObj<typeof Autocomplete>;

const EMAIL_DOMAINS = [
  "hello@gmail.com",
  "hello@yahoo.com",
  "hello@outlook.com",
  "hello@protonmail.com",
  "hello@icloud.com",
  "hello@fastmail.com",
  "hello@hey.com",
];

/* ─── 1. Basic ──────────────────────────────────────────────────────── */
export const Basic: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Type an email prefix; the popup suggests matching addresses. " +
          "Enter commits the highlighted suggestion to the input value; " +
          "without highlight, Enter accepts whatever the user typed.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic">
      <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
        <Autocomplete
          placeholder="email@example.com"
          items={EMAIL_DOMAINS}
          data-testid="autocomplete-basic"
        >
          {(d: string) => (
            <Autocomplete.Item
              key={d}
              value={d}
              data-testid={`autocomplete-basic-item-${d}`}
            >
              {d}
            </Autocomplete.Item>
          )}
        </Autocomplete>
      </div>
    </div>
  ),
};

/* ─── 2. AllSizes ───────────────────────────────────────────────────── */
export const AllSizes: Story = {
  name: "All sizes",
  parameters: {
    docs: {
      description: { story: "sm / md / lg — matches Input + Combobox." },
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
        <div className="zs-story-cell" key={size} style={{ minWidth: "20rem" }}>
          <span className="zs-story-label">{size.toUpperCase()}</span>
          <Autocomplete
            size={size}
            placeholder="email@…"
            items={EMAIL_DOMAINS}
            data-testid={`autocomplete-size-${size}`}
          >
            {(d: string) => (
              <Autocomplete.Item key={d} value={d}>
                {d}
              </Autocomplete.Item>
            )}
          </Autocomplete>
        </div>
      ))}
    </div>
  ),
};

/* ─── 3. WithLabel ──────────────────────────────────────────────────── */
export const WithLabel: Story = {
  name: "With label (Field)",
  parameters: {
    docs: {
      description: {
        story:
          "Wrapped in a Field. Field.Label labels the autocomplete " +
          "input; Field.Description sits below.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With label">
      <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
        <Field>
          <Field.Label>Email</Field.Label>
          <Autocomplete
            placeholder="email@example.com"
            items={EMAIL_DOMAINS}
            data-testid="autocomplete-withlabel"
          >
            {(d: string) => (
              <Autocomplete.Item key={d} value={d}>
                {d}
              </Autocomplete.Item>
            )}
          </Autocomplete>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 4. Empty ──────────────────────────────────────────────────────── */
export const Empty: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "No suggestions render until the user types. The popup itself " +
          "is anchor-only — no open state on focus by default.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Empty">
      <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
        <Autocomplete
          placeholder="Start typing…"
          items={[]}
          data-testid="autocomplete-empty"
        >
          {/* no items — the popup opens on type but is empty */}
        </Autocomplete>
      </div>
    </div>
  ),
};

/* ─── 5. Disabled ───────────────────────────────────────────────────── */
export const Disabled: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "`disabled` greys the input and blocks both typing and popup " +
          "interaction.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled">
      <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
        <Autocomplete
          disabled
          placeholder="Disabled"
          items={EMAIL_DOMAINS}
          data-testid="autocomplete-disabled"
        >
          {(d: string) => (
            <Autocomplete.Item key={d} value={d}>
              {d}
            </Autocomplete.Item>
          )}
        </Autocomplete>
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
          "Hebrew suggestions in an RTL container. Logical properties " +
          "carry the layout flip.",
      },
    },
  },
  render: () => (
    <div dir="rtl" className="zs-story-row" role="group" aria-label="RTL">
      <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
        <Autocomplete
          placeholder="הקלד דומיין"
          items={["דוגמה@ynet.co.il", "דוגמה@walla.co.il"]}
          data-testid="autocomplete-rtl"
        >
          {(d: string) => (
            <Autocomplete.Item key={d} value={d}>
              {d}
            </Autocomplete.Item>
          )}
        </Autocomplete>
      </div>
    </div>
  ),
};

/* ─── 7. WithDescription ────────────────────────────────────────────── */
export const WithDescription: Story = {
  name: "With description",
  parameters: {
    docs: {
      description: {
        story:
          "Field.Description renders below the autocomplete. Useful for " +
          "hints (\"we don't share this address\").",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With description">
      <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
        <Field>
          <Field.Label>Email</Field.Label>
          <Autocomplete
            placeholder="email@example.com"
            items={EMAIL_DOMAINS}
            data-testid="autocomplete-with-description"
          >
            {(d: string) => (
              <Autocomplete.Item key={d} value={d}>
                {d}
              </Autocomplete.Item>
            )}
          </Autocomplete>
          <Field.Description>
            We use this for sign-in and important account alerts only.
          </Field.Description>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 8. LongList ───────────────────────────────────────────────────── */
export const LongList: Story = {
  name: "Long list",
  parameters: {
    docs: {
      description: {
        story:
          "50 suggestion candidates. Typing narrows them; the popup caps " +
          "at min(50dvb, 24rem) and scrolls.",
      },
    },
  },
  render: () => {
    const items = Array.from({ length: 50 }, (_, i) => `suggestion-${i + 1}`);
    return (
      <div className="zs-story-row" role="group" aria-label="Long list">
        <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
          <Autocomplete
            placeholder="Type to filter"
            items={items}
            data-testid="autocomplete-long-list"
          >
            {(v: string) => (
              <Autocomplete.Item key={v} value={v}>
                {v}
              </Autocomplete.Item>
            )}
          </Autocomplete>
        </div>
      </div>
    );
  },
};

/* ─── 9. Required ───────────────────────────────────────────────────── */
export const Required: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "`<Field required>` cascades to the autocomplete. Submitting " +
          "empty fires `valueMissing`; Field.Error appears below.",
      },
    },
  },
  render: function RequiredRender() {
    return (
      <div className="zs-story-row" role="group" aria-label="Required autocomplete">
        <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
          <Form onSubmit={(e) => e.preventDefault()}>
            <Field required>
              <Field.Label>
                Email <Field.Required />
              </Field.Label>
              <Autocomplete
                name="email"
                placeholder="email@example.com"
                items={EMAIL_DOMAINS}
                data-testid="autocomplete-required"
              >
                {(d: string) => (
                  <Autocomplete.Item key={d} value={d}>
                    {d}
                  </Autocomplete.Item>
                )}
              </Autocomplete>
              <Field.Error match="valueMissing">
                Enter an email address.
              </Field.Error>
            </Field>
            <div style={{ marginTop: "0.75rem" }}>
              <Button type="submit" data-testid="autocomplete-required-submit">
                Submit
              </Button>
            </div>
          </Form>
        </div>
      </div>
    );
  },
};

/* ─── 9b. Required + Field.Error — POST-SUBMIT VISUAL EVIDENCE ─────── */
export const RequiredInvalid: Story = {
  name: "Required — post-submit (invalid)",
  parameters: {
    docs: {
      description: {
        story:
          "Companion to `Required` that auto-submits on mount so the " +
          "capture lands in the validation-failed state. Red error text " +
          "appears below the autocomplete. The Form's onSubmit " +
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
      <div className="zs-story-row" role="group" aria-label="Required autocomplete (invalid)">
        <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
          <Form
            onSubmit={(e) => {
              e.preventDefault();
            }}
            data-testid="autocomplete-required-invalid-form"
          >
            <Field required>
              <Field.Label>
                Email <Field.Required />
              </Field.Label>
              <Autocomplete
                name="email"
                placeholder="email@example.com"
                items={EMAIL_DOMAINS}
                data-testid="autocomplete-required-invalid"
              >
                {(d: string) => (
                  <Autocomplete.Item key={d} value={d}>
                    {d}
                  </Autocomplete.Item>
                )}
              </Autocomplete>
              <Field.Error match="valueMissing">
                Enter an email address.
              </Field.Error>
            </Field>
            <div style={{ marginTop: "0.75rem" }}>
              <Button
                ref={submitRef}
                type="submit"
                data-testid="autocomplete-required-invalid-submit"
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

/* ─── 10. AriaPropagation ───────────────────────────────────────────── *
 *
 * Slice-6 review-fix regression mirror: aria-* on `<Autocomplete>` must
 * propagate to the focusable `<input>`, not the `<div role="group">`
 * InputGroup. Pre-fix the wrapper stamped aria-* on the group. */
export const AriaPropagation: Story = {
  name: "Aria-* propagates to input",
  parameters: {
    docs: {
      description: {
        story:
          "Regression hook for Slice-6 review-fix 4: aria-* on " +
          "`<Autocomplete>` lands on the focusable `<input>`.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Aria propagation"
    >
      <div className="zs-story-cell" style={{ minWidth: "20rem" }}>
        <Autocomplete
          placeholder="email@example.com"
          items={EMAIL_DOMAINS}
          aria-label="Email address"
          data-testid="autocomplete-aria-propagation"
        >
          {(d: string) => (
            <Autocomplete.Item key={d} value={d}>
              {d}
            </Autocomplete.Item>
          )}
        </Autocomplete>
      </div>
    </div>
  ),
};

/* ─── 11. FieldAriaAutowiring ───────────────────────────────────────── *
 *
 * Wave-10 review-fix A regression: inside a `<Field>` with `<Field.Label>`
 * and `<Field.Description>`, the consumer passes NO `aria-label` /
 * `aria-labelledby` / `aria-describedby` on `<Autocomplete>`. Pre-fix
 * the wrapper unconditionally stamped `aria-labelledby={undefined}` and
 * `aria-describedby={undefined}` on the Input, which clobbered the ids
 * Base UI's Field bridge had auto-wired through `mergeProps`. The
 * focused input then had no accessible name. The aria-wiring script
 * resolves the input by its accessible name (`/email \(field-wired\)/i`,
 * supplied by `<Field.Label>`) and asserts BOTH `aria-labelledby` and
 * `aria-describedby` are non-empty on the input. */
export const FieldAriaAutowiring: Story = {
  name: "Field auto-wires labelledby + describedby (no consumer aria-*)",
  parameters: {
    docs: {
      description: {
        story:
          "Regression hook for Wave-10 review-fix A: inside `<Field>` with " +
          "`<Field.Label>` + `<Field.Description>`, Autocomplete must let " +
          "Base UI's Field bridge auto-wire `aria-labelledby` and " +
          "`aria-describedby` on the input. Pre-fix the wrapper passed " +
          "`undefined` and overwrote those ids — the input had no " +
          "accessible name. The story passes NO consumer aria-* props.",
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
          <Field.Label>Email (field-wired)</Field.Label>
          <Autocomplete
            placeholder="email@example.com"
            items={EMAIL_DOMAINS}
            data-testid="autocomplete-field-aria-autowiring"
          >
            {(d: string) => (
              <Autocomplete.Item key={d} value={d}>
                {d}
              </Autocomplete.Item>
            )}
          </Autocomplete>
          <Field.Description data-testid="autocomplete-field-aria-autowiring-desc">
            Field-wired description.
          </Field.Description>
        </Field>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const input = canvas.getByRole("combobox", {
      name: /email \(field-wired\)/i,
    });
    await expect(input).toHaveAttribute("aria-labelledby", /\S+/);
    await expect(input).toHaveAttribute("aria-describedby", /\S+/);
  },
};

/* ─── 12. PlaceholderFallback ───────────────────────────────────────── *
 *
 * Wave-10 review-fix B regression: a standalone `<Autocomplete>` with NO
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
          "Regression hook for Wave-10 review-fix B: a standalone " +
          "Autocomplete without a Field wrapper, without `aria-label`, " +
          "and without `aria-labelledby` falls back to using " +
          "`placeholder` as its accessible name so screen readers still " +
          "announce it.",
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
        <Autocomplete
          placeholder="Search emails"
          items={EMAIL_DOMAINS}
          data-testid="autocomplete-placeholder-fallback"
        >
          {(d: string) => (
            <Autocomplete.Item key={d} value={d}>
              {d}
            </Autocomplete.Item>
          )}
        </Autocomplete>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const input = canvas.getByRole("combobox", { name: /search emails/i });
    await expect(input).toHaveAttribute("aria-label", "Search emails");
  },
};
