import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import {
  Button,
  Checkbox,
  Field,
  Fieldset,
  Form,
  Input,
  Switch,
} from "../components";

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
        <Field name="street">
          <Field.Label>Street</Field.Label>
          <Input data-testid="fieldset-basic-street" />
        </Field>
      </Fieldset>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const fieldset = canvas.getByRole("group", { name: /mailing address/i });
    const street = canvas.getByRole("textbox", { name: /street/i });

    await expect(fieldset.tagName).toBe("FIELDSET");
    await userEvent.type(street, "42 Launch Way");
    await expect(street).toHaveValue("42 Launch Way");
  },
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
          <Field name={`name-${size}`} size={size}>
            <Field.Label>Name</Field.Label>
            <Input />
          </Field>
        </Fieldset>
      ))}
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    for (const size of ["sm", "md", "lg"]) {
      await expect(
        canvas.getByRole("group", { name: new RegExp(`size ${size}`, "i") }),
      ).toBeInTheDocument();
    }
  },
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
        <Field name="street">
          <Field.Label>Street</Field.Label>
          <Input />
        </Field>
        <Field name="city">
          <Field.Label>City</Field.Label>
          <Input />
        </Field>
        <Field name="postal">
          <Field.Label>Postal code</Field.Label>
          <Input inputMode="numeric" />
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
          "JavaScript wiring needed. Custom controls (Checkbox / Switch / " +
          "Radio / Toggle) consume a `FieldsetDisabledContext` since their " +
          "visible roots are non-native Base UI parts that don't pick up " +
          "the native cascade. Aria-wiring assertions verify the cascade " +
          "reaches both a nested Input AND a nested Checkbox.",
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
        <Field name="email">
          <Field.Label>Email</Field.Label>
          <Input
            type="email"
            defaultValue="locked@example.com"
            data-testid="fieldset-disabled-email"
          />
        </Field>
        <Field name="phone">
          <Field.Label>Phone</Field.Label>
          <Input defaultValue="+1 555 0100" />
        </Field>
        <Checkbox
          name="newsletter"
          label="Subscribe to newsletter"
          data-testid="fieldset-disabled-checkbox"
        />
        <Switch
          name="notifications"
          label="Notifications"
          data-testid="fieldset-disabled-switch"
        />
        <Button type="button" variant="tinted">
          Locked button
        </Button>
      </Fieldset>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const email = canvas.getByRole("textbox", { name: /^email$/i });
    const phone = canvas.getByRole("textbox", { name: /^phone$/i });
    const checkbox = canvas.getByRole("checkbox", {
      name: /subscribe to newsletter/i,
    });
    const notifications = canvas.getByRole("switch", {
      name: /notifications/i,
    });
    const button = canvas.getByRole("button", { name: /locked button/i });

    await expect(email).toBeDisabled();
    await expect(phone).toBeDisabled();
    await expect(checkbox).toHaveAttribute("aria-disabled", "true");
    await expect(notifications).toHaveAttribute("aria-disabled", "true");
    await expect(button).toBeDisabled();
    await userEvent.click(checkbox);
    await expect(checkbox).toHaveAttribute("aria-checked", "false");
  },
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
        <Field name="email" required>
          <Field.Label>
            Email <Field.Required />
          </Field.Label>
          <Input type="email" required />
        </Field>
        <Field name="password" required>
          <Field.Label>
            Password <Field.Required />
          </Field.Label>
          <Input type="password" required minLength={8} />
          <Field.Error match="tooShort">
            Use at least 8 characters.
          </Field.Error>
        </Field>
      </Fieldset>
      <Fieldset>
        <Fieldset.Legend>Profile</Fieldset.Legend>
        <Field name="display">
          <Field.Label>Display name</Field.Label>
          <Input />
        </Field>
      </Fieldset>
      <Button type="submit" variant="filled">
        Create account
      </Button>
    </Form>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const password = canvas.getByLabelText(/password/i);
    await userEvent.type(password, "short");
    await userEvent.click(
      canvas.getByRole("button", { name: /create account/i }),
    );
    await waitFor(() =>
      expect(password).toHaveAttribute("aria-invalid", "true"),
    );
    await expect(
      canvas.getByText("Use at least 8 characters."),
    ).toBeInTheDocument();
  },
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
        <Field name="cardholder">
          <Field.Label>Cardholder name</Field.Label>
          <Input />
        </Field>
        <Fieldset
          size="sm"
          style={{
            background: "var(--zs-fill-quaternary)",
            borderRadius: "var(--zs-radius-3)",
          }}
        >
          <Fieldset.Legend>Card details</Fieldset.Legend>
          <Field name="card-number">
            <Field.Label>Card number</Field.Label>
            <Input inputMode="numeric" />
          </Field>
          <Field name="expiry">
            <Field.Label>Expiry</Field.Label>
            <Input placeholder="MM / YY" />
          </Field>
        </Fieldset>
      </Fieldset>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(
      canvas.getByRole("group", { name: /payment method/i }),
    ).toBeInTheDocument();
    await expect(
      canvas.getByRole("group", { name: /card details/i }),
    ).toBeInTheDocument();
    await userEvent.type(
      canvas.getByRole("textbox", { name: /cardholder name/i }),
      "Ada Lovelace",
    );
    await expect(
      canvas.getByRole("textbox", { name: /cardholder name/i }),
    ).toHaveValue("Ada Lovelace");
  },
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
        <Field name="email">
          <Field.Label>Email</Field.Label>
          <Input type="email" />
        </Field>
        <Field name="phone">
          <Field.Label>Phone</Field.Label>
          <Input />
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

/* ─── 8. aria-labelledby={undefined} preserves Legend wiring ──────── *
 *
 * Regression for the spread-undefined footgun: rendering
 * `<Fieldset aria-labelledby={undefined}>` MUST NOT clobber Base UI's
 * auto-wired Legend id on the underlying `<fieldset>`. The wrapper
 * sifts the aria props and only re-applies each one when defined,
 * so the Legend binding survives whether the consumer passes
 * undefined or simply doesn't mention `aria-labelledby` at all.
 */
export const AriaLabelledByUndefined: Story = {
  name: "aria-labelledby undefined preserves Legend wiring",
  parameters: {
    docs: {
      description: {
        story:
          "Renders a Fieldset with `aria-labelledby={undefined}` " +
          "explicitly passed. Base UI's auto-wired Legend id MUST " +
          "still drive the fieldset's accessible name — the wrapper " +
          "sifts undefined aria props instead of letting them spread " +
          "onto Base UI and clobber the wiring.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Fieldset aria-labelledby undefined"
    >
      <Fieldset
        aria-labelledby={undefined}
        data-testid="fieldset-aria-undef"
        style={{ maxWidth: "26rem", width: "100%" }}
      >
        <Fieldset.Legend data-testid="fieldset-aria-undef-legend">
          Billing address
        </Fieldset.Legend>
        <Field name="street-aria">
          <Field.Label>Street</Field.Label>
          <Input data-testid="fieldset-aria-undef-street" />
        </Field>
      </Fieldset>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // The fieldset MUST still be accessibly-named "Billing address"
    // via Base UI's auto-wired Legend id — the undefined
    // aria-labelledby prop did NOT erase the wiring.
    const fieldset = canvas.getByRole("group", { name: /billing address/i });
    await expect(fieldset.tagName).toBe("FIELDSET");
    const labelledBy = fieldset.getAttribute("aria-labelledby");
    await expect(labelledBy).toBeTruthy();
    const legend = canvasElement.querySelector(
      '[data-testid="fieldset-aria-undef-legend"]',
    );
    await expect(legend?.id).toBe(labelledBy);
  },
};

/* ─── 9. Nested disabled Fieldset (effective cascade) ─────────────── *
 *
 * Regression for the inner-Fieldset disabled-state shadow: an inner
 * `<Fieldset>` placed inside a disabled outer `<Fieldset>` MUST
 * pick up the effective disabled state at its own Base UI Root —
 * `disabled` prop propagates, `data-disabled` attribute paints, and
 * the FieldsetDisabledContext continues forwarding to selection
 * primitives nested even deeper. Without this propagation the inner
 * Root reports enabled even though the native cascade greys every
 * descendant, leaving the chrome / metadata at odds.
 */
export const NestedFieldsetDisabledCascade: Story = {
  name: "Nested Fieldset disabled cascade",
  parameters: {
    docs: {
      description: {
        story:
          "Outer Fieldset is `disabled`; the inner Fieldset has no " +
          "explicit `disabled` prop yet MUST inherit the disabled " +
          "state at its own Base UI Root (carrying `disabled` + " +
          "`data-disabled`) so the cascade reaches descendant " +
          "non-native controls without disagreement.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Nested fieldset disabled cascade"
    >
      <Fieldset
        disabled
        data-testid="fieldset-nested-disabled-outer"
        style={{ maxWidth: "30rem", width: "100%" }}
      >
        <Fieldset.Legend>Outer (disabled)</Fieldset.Legend>
        <Field name="outer-name">
          <Field.Label>Outer name</Field.Label>
          <Input />
        </Field>
        <Fieldset
          size="sm"
          data-testid="fieldset-nested-disabled-inner"
          style={{
            background: "var(--zs-fill-quaternary)",
            borderRadius: "var(--zs-radius-3)",
          }}
        >
          <Fieldset.Legend>Inner (inherits disabled)</Fieldset.Legend>
          <Field name="inner-email">
            <Field.Label>Inner email</Field.Label>
            <Input
              type="email"
              data-testid="fieldset-nested-disabled-inner-email"
            />
          </Field>
          <Checkbox
            name="inner-newsletter"
            label="Inner newsletter"
            data-testid="fieldset-nested-disabled-inner-checkbox"
          />
        </Fieldset>
      </Fieldset>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const inner = canvasElement.querySelector(
      '[data-testid="fieldset-nested-disabled-inner"]',
    ) as HTMLFieldSetElement | null;
    // Inner Fieldset must paint disabled state — Base UI emits
    // `data-disabled=""` whenever its Root prop is true, and
    // propagates the signal into the Legend via context.
    await expect(inner).not.toBeNull();
    await expect(inner?.tagName).toBe("FIELDSET");
    await expect(inner?.hasAttribute("data-disabled")).toBe(true);
    // Resolve the Legend element via aria-labelledby (Base UI wires
    // by id). The Legend MUST also carry data-disabled — Base UI
    // only does this when it sees the inner Root's disabled prop
    // come through.
    const labelledBy = inner?.getAttribute("aria-labelledby");
    const legend = labelledBy
      ? canvasElement.querySelector(`[id="${labelledBy}"]`)
      : null;
    await expect(legend?.hasAttribute("data-disabled")).toBe(true);
    const innerEmail = canvasElement.querySelector(
      '[data-testid="fieldset-nested-disabled-inner-email"]',
    ) as HTMLInputElement | null;
    await expect(innerEmail?.disabled).toBe(true);
  },
};

/* ─── 10. RTL ─────────────────────────────────────────────────────── */
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
        <Field name="name-rtl">
          <Field.Label>שם מלא</Field.Label>
          <Input />
        </Field>
        <Field name="email-rtl">
          <Field.Label>דוא״ל</Field.Label>
          <Input type="email" />
        </Field>
      </Fieldset>
    </div>
  ),
};
