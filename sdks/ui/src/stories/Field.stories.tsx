import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, within } from "@storybook/test";
import { Field, Input } from "../components";

const meta: Meta<typeof Field> = {
  title: "Components/Field",
  component: Field,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Field>;

export const RequiredFallbackAndControl: Story = {
  name: "Required fallback and bare control (play)",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Field required fallback and bare control"
    >
      <div
        className="zs-story-cell"
        style={{ flex: "1 1 18rem", minWidth: "14rem" }}
      >
        <Field required>
          <Field.Label>
            Billing email <Field.Required />
          </Field.Label>
          <Input type="email" placeholder="you@example.com" />
          <Field.Description>Used for receipts.</Field.Description>
        </Field>
      </div>
      <div
        className="zs-story-cell"
        style={{ flex: "1 1 18rem", minWidth: "14rem" }}
      >
        <Field>
          <Field.Label>
            Team bio <Field.Required fallback="(optional)" />
          </Field.Label>
          <Input placeholder="What this team ships" />
        </Field>
      </div>
      <div
        className="zs-story-cell"
        style={{ flex: "1 1 18rem", minWidth: "14rem" }}
      >
        <Field name="alias">
          <Field.Label>Team alias</Field.Label>
          <Field.Control placeholder="platform" />
          <Field.Description>Rendered through Field.Control.</Field.Description>
        </Field>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const email = canvas.getByRole("textbox", { name: /billing email/i });
    const optional = canvas.getByText("(optional)");
    const alias = canvas.getByRole("textbox", { name: /team alias/i });

    await expect(email).toHaveAttribute("aria-required", "true");
    await expect(optional).toHaveClass("zs-field__required--fallback");

    await userEvent.type(alias, "console");
    await expect(alias).toHaveValue("console");
  },
};

export const ValidityRenderProp: Story = {
  name: "Validity render prop (play)",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Field validity render prop"
    >
      <div
        className="zs-story-cell"
        style={{ flex: "1 1 22rem", minWidth: "18rem" }}
      >
        <Field
          validationMode="onBlur"
          validate={(value) =>
            value === "admin" ? "The admin alias is reserved." : null
          }
        >
          <Field.Label>Alias</Field.Label>
          <Input placeholder="Choose an alias" />
          <Field.Error />
          <Field.Validity>
            {(validity) => (
              <output role="status" aria-label="Alias validity">
                {validity.validity.valid === false
                  ? `invalid: ${validity.error}`
                  : validity.validity.valid === true
                    ? "valid"
                    : "pending"}
              </output>
            )}
          </Field.Validity>
        </Field>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const alias = canvas.getByRole("textbox", { name: /alias/i });
    const status = canvas.getByRole("status", { name: /alias validity/i });

    await userEvent.type(alias, "admin");
    await userEvent.tab();
    await expect(status).toHaveTextContent(/invalid: the admin alias is reserved/i);
  },
};

export const CallbackClassParts: Story = {
  name: "Callback class parts (play)",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Field callback class parts"
    >
      <div
        className="zs-story-cell"
        style={{ flex: "1 1 22rem", minWidth: "18rem" }}
      >
        <Field required validationMode="onBlur">
          <Field.Label className={() => "field-label-callback"}>
            Support email <Field.Required>required</Field.Required>
          </Field.Label>
          <Field.Control
            type="email"
            className={() => "field-control-callback"}
            placeholder="help@example.com"
          />
          <Field.Description className={() => "field-description-callback"}>
            Must be a reachable inbox.
          </Field.Description>
          <Field.Error
            match="typeMismatch"
            className={() => "field-error-callback"}
          >
            Enter a valid support email.
          </Field.Error>
        </Field>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const email = canvas.getByRole("textbox", { name: /support email/i });

    await expect(email).toHaveClass("zs-field__control");
    await expect(email).toHaveClass("field-control-callback");
    await userEvent.type(email, "not-email");
    await userEvent.tab();

    const alert = await canvas.findByRole("alert");
    await expect(alert).toHaveClass("zs-field__error");
    await expect(alert).toHaveClass("field-error-callback");
  },
};
