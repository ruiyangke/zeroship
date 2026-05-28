import type { Meta, StoryObj } from "@storybook/react";
import { Button, Field, Fieldset, Form, Input } from "../components";

const meta: Meta<typeof Fieldset> = {
  title: "Components/Fieldset",
  component: Fieldset,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Fieldset>;

/* ─── 1. Basic Fieldset with Legend ───────────────────────────────── */
export const BasicWithLegend: Story = {
  name: "Basic with Legend",
  parameters: {
    docs: {
      description: {
        story:
          "Minimal Fieldset wrapping a single Field. The Legend is a " +
          "Base UI `<div>` whose id is automatically referenced by the " +
          "<fieldset>'s `aria-labelledby`. Verified by the aria-wiring " +
          "assertion.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic fieldset">
      <Fieldset
        data-testid="fieldset-basic"
        style={{ maxWidth: "26rem", width: "100%" }}
      >
        <Fieldset.Legend>Mailing address</Fieldset.Legend>
        <Field>
          <Field.Label>Street</Field.Label>
          <Input name="street" data-testid="fieldset-basic-street" />
        </Field>
      </Fieldset>
    </div>
  ),
};

/* ─── 2. All sizes ────────────────────────────────────────────────── */
export const AllSizes: Story = {
  name: "All sizes",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Fieldset sizes">
      {(["sm", "md", "lg"] as const).map((size) => (
        <Fieldset
          key={size}
          size={size}
          style={{ maxWidth: "20rem", width: "100%" }}
        >
          <Fieldset.Legend>Size {size}</Fieldset.Legend>
          <Field size={size}>
            <Field.Label>Name</Field.Label>
            <Input name={`name-${size}`} />
          </Field>
        </Fieldset>
      ))}
    </div>
  ),
};

/* ─── 3. Nested Fields ────────────────────────────────────────────── */
export const NestedFields: Story = {
  name: "Nested Fields",
  parameters: {
    docs: {
      description: {
        story:
          "Three Fields under one Legend. Common form pattern: address " +
          "block, preferences group, contact info.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Fieldset nested fields">
      <Fieldset style={{ maxWidth: "26rem", width: "100%" }}>
        <Fieldset.Legend>Shipping address</Fieldset.Legend>
        <Field>
          <Field.Label>Street</Field.Label>
          <Input name="street" />
        </Field>
        <Field>
          <Field.Label>City</Field.Label>
          <Input name="city" />
        </Field>
        <Field>
          <Field.Label>Postal code</Field.Label>
          <Input name="postal" inputMode="numeric" />
        </Field>
      </Fieldset>
    </div>
  ),
};

/* ─── 4. Disabled cascade ─────────────────────────────────────────── */
export const DisabledCascade: Story = {
  name: "Disabled (cascade)",
  parameters: {
    docs: {
      description: {
        story:
          "Native `<fieldset disabled>` cascades the disabled state to " +
          "every interactive descendant at the browser layer — no " +
          "JavaScript wiring needed. The aria-wiring assertion verifies " +
          "the cascade reaches a nested Input via the `:disabled` " +
          "pseudo.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Fieldset disabled cascade">
      <Fieldset
        disabled
        data-testid="fieldset-disabled"
        style={{ maxWidth: "26rem", width: "100%" }}
      >
        <Fieldset.Legend>Locked</Fieldset.Legend>
        <Field>
          <Field.Label>Email</Field.Label>
          <Input
            type="email"
            name="email"
            defaultValue="locked@example.com"
            data-testid="fieldset-disabled-email"
          />
        </Field>
        <Field>
          <Field.Label>Phone</Field.Label>
          <Input name="phone" defaultValue="+1 555 0100" />
        </Field>
        <Button type="button" variant="tinted">
          Locked button
        </Button>
      </Fieldset>
    </div>
  ),
};

/* ─── 5. With Form integration ────────────────────────────────────── */
export const WithFormIntegration: Story = {
  name: "With Form integration",
  parameters: {
    docs: {
      description: {
        story:
          "Fieldset inside a Form. The Form coordinates validation across " +
          "every nested Field regardless of Fieldset boundaries — Fieldset " +
          "is purely structural / visual.",
      },
    },
  },
  render: () => (
    <Form
      variant="card"
      className="zs-story-cell"
      style={{ maxWidth: "28rem", display: "grid", gap: "0.75rem" }}
    >
      <Fieldset>
        <Fieldset.Legend>Account</Fieldset.Legend>
        <Field required>
          <Field.Label>
            Email <Field.Required />
          </Field.Label>
          <Input type="email" name="email" required />
        </Field>
        <Field required>
          <Field.Label>
            Password <Field.Required />
          </Field.Label>
          <Input type="password" name="password" required minLength={8} />
          <Field.Error match="tooShort">
            Use at least 8 characters.
          </Field.Error>
        </Field>
      </Fieldset>
      <Fieldset>
        <Fieldset.Legend>Profile</Fieldset.Legend>
        <Field>
          <Field.Label>Display name</Field.Label>
          <Input name="display" />
        </Field>
      </Fieldset>
      <Button type="submit" variant="filled">
        Create account
      </Button>
    </Form>
  ),
};

/* ─── 6. Nested Fieldset ──────────────────────────────────────────── */
export const NestedFieldset: Story = {
  name: "Nested Fieldset",
  parameters: {
    docs: {
      description: {
        story:
          "Fieldset inside a Fieldset. Each level carries its own Legend; " +
          "the inner fieldset's `aria-labelledby` points at its own legend " +
          "(NOT the outer one — Base UI's RootContext.Provider isolates the " +
          "binding to the nearest Root).",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Nested fieldset">
      <Fieldset style={{ maxWidth: "30rem", width: "100%" }}>
        <Fieldset.Legend>Payment method</Fieldset.Legend>
        <Field>
          <Field.Label>Cardholder name</Field.Label>
          <Input name="cardholder" />
        </Field>
        <Fieldset
          size="sm"
          style={{
            background: "var(--zs-fill-quaternary)",
            borderRadius: "var(--zs-radius-3)",
          }}
        >
          <Fieldset.Legend>Card details</Fieldset.Legend>
          <Field>
            <Field.Label>Card number</Field.Label>
            <Input name="card-number" inputMode="numeric" />
          </Field>
          <Field>
            <Field.Label>Expiry</Field.Label>
            <Input name="expiry" placeholder="MM / YY" />
          </Field>
        </Fieldset>
      </Fieldset>
    </div>
  ),
};

/* ─── 7. Custom legend position (bottom) ──────────────────────────── */
export const CustomLegendPosition: Story = {
  name: "Custom Legend position",
  parameters: {
    docs: {
      description: {
        story:
          "Base UI renders the Legend as a `<div>` bound via " +
          "`aria-labelledby` — so its DOM position is decoupled from the " +
          "label binding. Here the Legend sits at the BOTTOM of the " +
          "fieldset (via flex `order`); the fieldset is still " +
          "accessibly labelled.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Custom legend position">
      <Fieldset style={{ maxWidth: "26rem", width: "100%" }}>
        <Field>
          <Field.Label>Email</Field.Label>
          <Input type="email" name="email" />
        </Field>
        <Field>
          <Field.Label>Phone</Field.Label>
          <Input name="phone" />
        </Field>
        <Fieldset.Legend
          style={{
            order: 999,
            color: "var(--zs-label-secondary)",
            fontWeight: 400,
            fontSize: "var(--zs-text-footnote-size)",
            lineHeight: "var(--zs-text-footnote-line)",
          }}
        >
          Contact details
        </Fieldset.Legend>
      </Fieldset>
    </div>
  ),
};

/* ─── 8. RTL ──────────────────────────────────────────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew labels. Fieldset uses logical properties for padding + " +
          "gap so the layout flips without any direction-conditional CSS.",
      },
    },
  },
  render: () => (
    <div dir="rtl" className="zs-story-row" role="group" aria-label="RTL fieldset">
      <Fieldset style={{ maxWidth: "26rem", width: "100%" }}>
        <Fieldset.Legend>פרטי התקשרות</Fieldset.Legend>
        <Field>
          <Field.Label>שם מלא</Field.Label>
          <Input name="name-rtl" />
        </Field>
        <Field>
          <Field.Label>דוא״ל</Field.Label>
          <Input type="email" name="email-rtl" />
        </Field>
      </Fieldset>
    </div>
  ),
};
