import type { Meta, StoryObj } from "@storybook/react";
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
