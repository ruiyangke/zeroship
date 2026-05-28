import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { useRef, useState } from "react";
import { Button, Field, Form, type FormActions, Input } from "../components";

const meta: Meta<typeof Form> = {
  title: "Components/Form",
  component: Form,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Form>;

/* ─── 1. Basic submit handler ──────────────────────────────────────── */
export const BasicSubmit: Story = {
  name: "Basic submit handler",
  parameters: {
    docs: {
      description: {
        story:
          "Minimal Form. `onFormSubmit` fires AFTER every Field reports " +
          "valid (or immediately when no Field has a validation rule). " +
          "Base UI calls `preventDefault()` on the native submit event " +
          "before invoking the callback — no extra plumbing required.",
      },
    },
  },
  render: function BasicSubmitRender() {
    const [submitted, setSubmitted] = useState<string>("(not submitted)");
    return (
      <div className="zs-story-row" role="group" aria-label="Basic form submit">
        <Form
          data-testid="form-basic"
          className="zs-story-cell"
          style={{ maxWidth: "26rem", display: "grid", gap: "0.75rem" }}
          onFormSubmit={(values) => {
            const entries = Object.entries(values)
              .map(([k, v]) => `${k}=${String(v)}`)
              .join(", ");
            setSubmitted(entries.length === 0 ? "(empty)" : entries);
          }}
        >
          <Field name="email">
            <Field.Label>Email</Field.Label>
            <Input
              type="email"
              defaultValue="hello@example.com"
              data-testid="form-basic-email"
            />
          </Field>
          <Button type="submit" variant="filled">
            Submit
          </Button>
          <span style={{ fontSize: "0.8125rem" }} data-testid="form-basic-result">
            submitted: {submitted}
          </span>
        </Form>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const email = canvas.getByRole("textbox", { name: /email/i });
    await userEvent.clear(email);
    await userEvent.type(email, "updated@example.com");
    await userEvent.click(canvas.getByRole("button", { name: /^submit$/i }));
    await expect(
      canvas.getByText("submitted: email=updated@example.com"),
    ).toBeInTheDocument();
  },
};

/* ─── 2. With server-side validation errors ───────────────────────── */
export const WithValidation: Story = {
  name: "With server errors",
  parameters: {
    docs: {
      description: {
        story:
          "Server-side errors flow back through the `errors` prop keyed " +
          "by Field name. Each matching Field flips `aria-invalid` and " +
          "renders the error subtree on the next commit. Toggling the " +
          "button below simulates a server response cycle.",
      },
    },
  },
  render: function WithValidationRender() {
    const [errors, setErrors] = useState<Record<string, string> | undefined>(
      undefined,
    );
    return (
      <div className="zs-story-row" role="group" aria-label="Form server errors">
        <Form
          data-testid="form-server-errors"
          className="zs-story-cell"
          style={{ maxWidth: "26rem", display: "grid", gap: "0.75rem" }}
          errors={errors}
          onFormSubmit={() => {
            // Simulate a server saying the email is already taken.
            setErrors({ email: "That email is already in use." });
          }}
        >
          <Field name="email">
            <Field.Label>Email</Field.Label>
            <Input
              type="email"
              defaultValue="taken@example.com"
              data-testid="form-server-errors-email"
            />
            <Field.Error data-testid="form-server-errors-msg" />
          </Field>
          <Button type="submit" variant="filled" data-testid="form-server-errors-submit">
            Submit
          </Button>
        </Form>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const email = canvas.getByRole("textbox", { name: /email/i });
    await expect(email).not.toHaveAttribute("aria-invalid", "true");
    await userEvent.click(canvas.getByRole("button", { name: /^submit$/i }));
    await waitFor(() => expect(email).toHaveAttribute("aria-invalid", "true"));
    await expect(
      canvas.getByText("That email is already in use."),
    ).toBeInTheDocument();
  },
};

/* ─── 3. Validation modes ─────────────────────────────────────────── */
export const ValidationModes: Story = {
  name: "Validation modes",
  parameters: {
    docs: {
      description: {
        story:
          "Three siblings, each driven by a different `validationMode`. " +
          "`onSubmit` (default) waits for submit, then re-validates on " +
          "every change. `onBlur` fires when the field loses focus. " +
          "`onChange` fires on every keystroke.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Form validation modes">
      {(["onSubmit", "onBlur", "onChange"] as const).map((mode) => (
        <Form
          key={mode}
          validationMode={mode}
          className="zs-story-cell"
          style={{ maxWidth: "18rem", display: "grid", gap: "0.5rem" }}
        >
          <span className="zs-story-label">{mode}</span>
          <Field name={`email-${mode}`}>
            <Field.Label>Email</Field.Label>
            <Input type="email" required />
            <Field.Error match="typeMismatch">
              Enter a valid email.
            </Field.Error>
            <Field.Error match="valueMissing">
              Email is required.
            </Field.Error>
          </Field>
          <Button type="submit" size="small" variant="filled">
            Submit
          </Button>
        </Form>
      ))}
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const emailInputs = canvas.getAllByRole("textbox", { name: /email/i });
    const submitButtons = canvas.getAllByRole("button", { name: /^submit$/i });

    await userEvent.click(submitButtons[0]);
    await waitFor(() =>
      expect(emailInputs[0]).toHaveAttribute("aria-invalid", "true"),
    );
    await expect(canvas.getByText("Email is required.")).toBeInTheDocument();

    await userEvent.type(emailInputs[2], "not-an-email");
    await waitFor(() =>
      expect(emailInputs[2]).toHaveAttribute("aria-invalid", "true"),
    );
    await expect(canvas.getByText("Enter a valid email.")).toBeInTheDocument();
  },
};

/* ─── 4. Variants — default and card ──────────────────────────────── */
export const Variants: Story = {
  name: "Variants",
  parameters: {
    docs: {
      description: {
        story:
          "`variant=\"default\"` is a layout-only shell — no surface chrome. " +
          "`variant=\"card\"` opts in to an opaque surface + Card-style rim " +
          "so the form can stand alone as a page-level container without " +
          "wrapping in a Card.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Form variants">
      <div className="zs-story-cell" style={{ maxWidth: "26rem" }}>
        <span className="zs-story-label">default</span>
        <Form style={{ display: "grid", gap: "0.75rem" }}>
          <Field name="name">
            <Field.Label>Name</Field.Label>
            <Input defaultValue="Ada Lovelace" />
          </Field>
          <Button type="submit" variant="filled">
            Save
          </Button>
        </Form>
      </div>
      <div className="zs-story-cell" style={{ maxWidth: "26rem" }}>
        <span className="zs-story-label">card</span>
        <Form
          variant="card"
          data-testid="form-card"
          style={{ display: "grid", gap: "0.75rem" }}
        >
          <Field name="name">
            <Field.Label>Name</Field.Label>
            <Input defaultValue="Grace Hopper" />
          </Field>
          <Button type="submit" variant="filled">
            Save
          </Button>
        </Form>
      </div>
    </div>
  ),
};

/* ─── 5. With Fields (decomposed form-row composition) ────────────── */
export const WithFields: Story = {
  name: "With Fields",
  parameters: {
    docs: {
      description: {
        story:
          "The canonical composition: a Form wrapping multiple Field + " +
          "Input rows. Validation flows from each Field's native control " +
          "constraint (HTML `required`, `type=\"email\"`, `pattern`) " +
          "through Base UI's Form coordinator.",
      },
    },
  },
  render: () => (
    <Form
      className="zs-story-cell"
      style={{ maxWidth: "28rem", display: "grid", gap: "0.75rem" }}
    >
      <Field name="name" required>
        <Field.Label>
          Full name <Field.Required />
        </Field.Label>
        <Input required />
        <Field.Error match="valueMissing">Name is required.</Field.Error>
      </Field>
      <Field name="email" required>
        <Field.Label>
          Email <Field.Required />
        </Field.Label>
        <Input type="email" required />
        <Field.Error match="typeMismatch">Enter a valid email.</Field.Error>
        <Field.Error match="valueMissing">Email is required.</Field.Error>
      </Field>
      <Field name="company">
        <Field.Label>Company (optional)</Field.Label>
        <Input />
      </Field>
      <Button type="submit" variant="filled">
        Sign up
      </Button>
    </Form>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const fullName = canvas.getByRole("textbox", { name: /full name/i });
    const email = canvas.getByRole("textbox", { name: /^email/i });

    await userEvent.click(canvas.getByRole("button", { name: /sign up/i }));
    await waitFor(() =>
      expect(fullName).toHaveAttribute("aria-invalid", "true"),
    );
    await expect(email).toHaveAttribute("aria-invalid", "true");
    await expect(canvas.getByText("Name is required.")).toBeInTheDocument();
    await expect(canvas.getByText("Email is required.")).toBeInTheDocument();
  },
};

/* ─── 6. actionsRef.validate() ────────────────────────────────────── */
export const ActionsRefValidate: Story = {
  name: "actionsRef.validate()",
  parameters: {
    docs: {
      description: {
        story:
          "Imperative `actionsRef.current.validate()` triggers each " +
          "Field's validation outside the submit flow — useful before " +
          "kicking off a manual `fetch`. Pass a field name to validate " +
          "just one.",
      },
    },
  },
  render: function ActionsRefValidateRender() {
    const actionsRef = useRef<FormActions | null>(null);
    const [didValidate, setDidValidate] = useState(false);
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Form actionsRef validate"
      >
        <Form
          actionsRef={actionsRef}
          data-testid="form-actions-ref"
          className="zs-story-cell"
          style={{ maxWidth: "26rem", display: "grid", gap: "0.75rem" }}
        >
          <Field name="email">
            <Field.Label>Email (required)</Field.Label>
            <Input
              type="email"
              required
              data-testid="form-actions-ref-email"
            />
            <Field.Error match="valueMissing">Email is required.</Field.Error>
            <Field.Error match="typeMismatch">Enter a valid email.</Field.Error>
          </Field>
          <Button
            type="button"
            variant="tinted"
            data-testid="form-actions-ref-button"
            onClick={() => {
              actionsRef.current?.validate();
              setDidValidate(true);
            }}
          >
            Validate now
          </Button>
          <span style={{ fontSize: "0.8125rem" }} data-testid="form-actions-ref-status">
            {didValidate ? "validate() called" : "(not yet)"}
          </span>
        </Form>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const email = canvas.getByRole("textbox", { name: /email \(required\)/i });
    await userEvent.click(canvas.getByRole("button", { name: /validate now/i }));
    await waitFor(() =>
      expect(email).toHaveAttribute("aria-invalid", "true"),
    );
    await expect(canvas.getByText("validate() called")).toBeInTheDocument();
    await expect(canvas.getByText("Email is required.")).toBeInTheDocument();
  },
};

/* ─── 7. Disabled — every control disabled via Field cascade ──────── */
export const Disabled: Story = {
  name: "Disabled (Field cascade)",
  parameters: {
    docs: {
      description: {
        story:
          "Form itself doesn't carry a `disabled` prop — that's the " +
          "Fieldset's job (see the Fieldset stories). For per-Field " +
          "disabling, set `disabled` on the Field. Submit button " +
          "stays enabled so the consumer can drive the disabled flag " +
          "from outside the form tree.",
      },
    },
  },
  render: () => (
    <Form
      className="zs-story-cell"
      style={{ maxWidth: "26rem", display: "grid", gap: "0.75rem" }}
    >
      <Field name="email" disabled>
        <Field.Label>Email</Field.Label>
        <Input defaultValue="locked@example.com" />
      </Field>
      <Field name="password" disabled>
        <Field.Label>Password</Field.Label>
        <Input type="password" />
      </Field>
      <Button type="submit" variant="filled" disabled>
        Submit (disabled)
      </Button>
    </Form>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const email = canvas.getByRole("textbox", { name: /^email$/i });
    const password = canvas.getByLabelText(/password/i);
    const submit = canvas.getByRole("button", { name: /submit \(disabled\)/i });

    await expect(email).toBeDisabled();
    await expect(password).toBeDisabled();
    await expect(submit).toBeDisabled();
    await userEvent.click(submit);
    await expect(submit).not.toHaveFocus();
  },
};

/* ─── 8. RTL ──────────────────────────────────────────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew labels. Form + Field both use logical properties so the " +
          "vertical rhythm and inline padding flip without any direction- " +
          "conditional CSS.",
      },
    },
  },
  render: () => (
    <div dir="rtl" className="zs-story-row" role="group" aria-label="RTL form">
      <Form
        variant="card"
        className="zs-story-cell"
        style={{ maxWidth: "28rem", display: "grid", gap: "0.75rem" }}
      >
        <Field name="name-rtl" required>
          <Field.Label>
            שם מלא <Field.Required />
          </Field.Label>
          <Input required />
        </Field>
        <Field name="email-rtl" required>
          <Field.Label>
            דוא״ל <Field.Required />
          </Field.Label>
          <Input type="email" required />
        </Field>
        <Button type="submit" variant="filled">
          הרשמה
        </Button>
      </Form>
    </div>
  ),
};
