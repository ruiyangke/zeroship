import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { useEffect, useRef, type CSSProperties } from "react";
import { Form } from "@base-ui/react/form";
import { Field, OtpField } from "../components";

// Visually-hidden Field.Label — cell 0 of an OtpField has no
// announceable name without a <label> association (Base UI ignores
// aria-label on the first input by design). The bare stories use this
// helper so the row stays visually pristine while still passing axe.
const srOnly: CSSProperties = {
  position: "absolute",
  inlineSize: 1,
  blockSize: 1,
  margin: -1,
  padding: 0,
  overflow: "hidden",
  clip: "rect(0 0 0 0)",
  whiteSpace: "nowrap",
  border: 0,
};

const meta: Meta<typeof OtpField> = {
  title: "Components/OtpField",
  component: OtpField,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof OtpField>;

/* ─── 1. Basic — 6-digit code ──────────────────────────────────────── */
export const Basic: Story = {
  name: "Basic (6-digit)",
  parameters: {
    docs: {
      description: {
        story:
          "Six-cell code entry — the canonical 2FA / email-verification " +
          "shape. Base UI handles auto-advance focus, paste-splitting " +
          "across cells (paste of '123456' fills all six), and " +
          "backspace-deletes-and-rewinds.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic OTP">
      <div className="zs-story-cell">
        <Field>
          <Field.Label style={srOnly}>Verification code</Field.Label>
          <OtpField data-testid="otp-basic" />
        </Field>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const cells = canvas.getAllByRole("textbox");

    await expect(cells).toHaveLength(6);
    await userEvent.click(cells[0]);
    await userEvent.type(cells[0], "123456");
    for (const [index, cell] of cells.entries()) {
      await expect(cell).toHaveValue(String(index + 1));
    }
    await expect(cells[5]).toHaveFocus();
    await userEvent.keyboard("{Backspace}");
    await expect(cells[5]).toHaveValue("");
  },
};

/* ─── 2. CustomLength — 4-digit PIN ────────────────────────────────── */
export const CustomLength: Story = {
  name: "Custom length (4-digit PIN)",
  parameters: {
    docs: {
      description: {
        story:
          "Pass `length={4}` for a 4-digit PIN (banking apps, simple " +
          "verifications). Pasting 4 digits fills all four; pasting a " +
          "longer string is clamped by Base UI.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="4-digit PIN">
      <div className="zs-story-cell">
        <Field>
          <Field.Label style={srOnly}>PIN</Field.Label>
          <OtpField length={4} data-testid="otp-pin" />
        </Field>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const cells = canvas.getAllByRole("textbox");

    await expect(cells).toHaveLength(4);
    await userEvent.click(cells[0]);
    await userEvent.paste("98765");
    await expect(cells[0]).toHaveValue("9");
    await expect(cells[1]).toHaveValue("8");
    await expect(cells[2]).toHaveValue("7");
    await expect(cells[3]).toHaveValue("6");
  },
};

/* ─── 3. AllSizes — sm / md / lg ───────────────────────────────────── */
export const AllSizes: Story = {
  name: "All sizes",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="All sizes"
      style={{ flexDirection: "column", alignItems: "stretch", gap: "1.5rem" }}
    >
      <div className="zs-story-cell">
        <span className="zs-story-label">Small</span>
        <Field>
          <Field.Label style={srOnly}>Small code</Field.Label>
          <OtpField size="sm" data-testid="otp-sm" />
        </Field>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Medium (default)</span>
        <Field>
          <Field.Label style={srOnly}>Medium code</Field.Label>
          <OtpField size="md" data-testid="otp-md" />
        </Field>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Large</span>
        <Field>
          <Field.Label style={srOnly}>Large code</Field.Label>
          <OtpField size="lg" data-testid="otp-lg" />
        </Field>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const firstCell = canvas.getAllByRole("textbox")[0];
    await userEvent.click(firstCell);
    await userEvent.type(firstCell, "123456");
    await waitFor(() =>
      expect(firstCell.closest(".zs-otp-field")).toHaveAttribute(
        "data-complete",
        "",
      ),
    );
  },
};

/* ─── 4. AllVariants — default / outline ───────────────────────────── */
export const AllVariants: Story = {
  name: "All variants",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="All variants"
      style={{ flexDirection: "column", alignItems: "stretch", gap: "1.5rem" }}
    >
      <div className="zs-story-cell">
        <span className="zs-story-label">Default (filled)</span>
        <Field>
          <Field.Label style={srOnly}>Default variant</Field.Label>
          <OtpField variant="default" data-testid="otp-default" />
        </Field>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Outline</span>
        <Field>
          <Field.Label style={srOnly}>Outline variant</Field.Label>
          <OtpField variant="outline" data-testid="otp-outline" />
        </Field>
      </div>
    </div>
  ),
};

/* ─── 5. WithLabel — Field-wrapped ─────────────────────────────────── */
export const WithLabel: Story = {
  name: "With label (Field)",
  parameters: {
    docs: {
      description: {
        story:
          "Inside a `<Field>`, the OtpField inherits size and required " +
          "and the Field.Label auto-associates with the first cell. " +
          "The Field.Description sits below the row.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With label">
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <Field>
          <Field.Label>Verification code</Field.Label>
          <OtpField data-testid="otp-with-label" />
          <Field.Description>
            We sent a 6-digit code to your email.
          </Field.Description>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 6. RequiredInvalid — submitted-empty visible error ───────────── */
export const RequiredInvalid: Story = {
  name: "Required + invalid (submit empty)",
  parameters: {
    docs: {
      description: {
        story:
          "Auto-submits on mount so the Field.Error message paints. " +
          "Validates the aria-wiring contract: incomplete OTP + " +
          "required + form submit → live-region announcement. Uses " +
          "Base UI's `Form` component so Field.Error's `valueMissing` " +
          "match fires against the hidden validation input.",
      },
    },
  },
  render: function RequiredInvalidRender() {
    const submitRef = useRef<HTMLButtonElement>(null);
    useEffect(() => {
      // Two RAFs — same pattern Checkbox / Radio use. One frame for
      // layout, one for Base UI's Field validation subscription to
      // wire up.
      const id = requestAnimationFrame(() => {
        requestAnimationFrame(() => {
          submitRef.current?.click();
        });
      });
      return () => cancelAnimationFrame(id);
    }, []);
    return (
      <div className="zs-story-row" role="group" aria-label="Required invalid">
        <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
          <Form
            onSubmit={(e) => {
              e.preventDefault();
            }}
          >
            <Field required>
              <Field.Label>
                Code <Field.Required />
              </Field.Label>
              <OtpField name="code" data-testid="otp-required-invalid" />
              <Field.Error match="valueMissing">
                Enter your verification code.
              </Field.Error>
            </Field>
            <button
              ref={submitRef}
              type="submit"
              style={{
                position: "absolute",
                inlineSize: 1,
                blockSize: 1,
                opacity: 0,
                pointerEvents: "none",
              }}
            >
              Submit
            </button>
          </Form>
        </div>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const firstCell = canvas.getAllByRole("textbox")[0];
    await waitFor(() =>
      expect(firstCell).toHaveAttribute("aria-invalid", "true"),
    );
    await expect(
      canvas.getByText("Enter your verification code."),
    ).toBeInTheDocument();
  },
};

/* ─── 7. Disabled ──────────────────────────────────────────────────── */
export const Disabled: Story = {
  name: "Disabled",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled">
      <div className="zs-story-cell">
        <Field>
          <Field.Label style={srOnly}>Disabled code</Field.Label>
          <OtpField
            disabled
            defaultValue="123"
            data-testid="otp-disabled"
          />
        </Field>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const cells = canvas.getAllByRole("textbox");

    for (const cell of cells) {
      await expect(cell).toBeDisabled();
    }
    await userEvent.click(cells[0]);
    await expect(cells[0]).not.toHaveFocus();
  },
};

/* ─── 7.5 Readonly — non-editable presentation ──────────────────────── */
export const Readonly: Story = {
  name: "Readonly",
  parameters: {
    docs: {
      description: {
        story:
          "Readonly state — the code is presented but cannot be edited. " +
          "Used for confirmation screens where the entered code is " +
          "echoed back. The forced-colors mirror keeps the row legible " +
          "under Windows High Contrast.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Readonly">
      <div className="zs-story-cell">
        <Field>
          <Field.Label style={srOnly}>Readonly code</Field.Label>
          <OtpField
            readOnly
            defaultValue="123456"
            data-testid="otp-readonly"
          />
        </Field>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const cells = canvas.getAllByRole("textbox");
    for (const cell of cells) {
      await expect(cell).toHaveAttribute("readonly");
    }
  },
};

/* ─── 8. Standalone aria paths ─────────────────────────────────────── */
export const StandaloneAriaPaths: Story = {
  name: "Standalone aria paths",
  parameters: {
    docs: {
      description: {
        story:
          "Bare OtpField rows without Field context. One relies on the " +
          "built-in hidden label fallback, one forwards aria-label plus " +
          "aria-describedby, and one forwards aria-labelledby.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Standalone OTP aria paths"
      style={{ flexDirection: "column", alignItems: "stretch", gap: "1.5rem" }}
    >
      <div className="zs-story-cell">
        <span className="zs-story-label">Hidden-label fallback</span>
        <OtpField length={3} />
      </div>
      <div className="zs-story-cell">
        <span id="otp-explicit-description" className="zs-story-label">
          Backup code is three digits.
        </span>
        <OtpField
          length={3}
          aria-label="Backup code"
          aria-describedby="otp-explicit-description"
        />
      </div>
      <div className="zs-story-cell">
        <span id="otp-labelledby" className="zs-story-label">
          Recovery code
        </span>
        <OtpField length={3} aria-labelledby="otp-labelledby" />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const cells = canvas.getAllByRole("textbox");

    await expect(cells).toHaveLength(9);
    await userEvent.click(cells[0]);
    await userEvent.type(cells[0], "135");
    await expect(cells[0]).toHaveValue("1");
    await expect(cells[1]).toHaveValue("3");
    await expect(cells[2]).toHaveValue("5");

    await userEvent.click(cells[3]);
    await userEvent.type(cells[3], "246");
    await expect(cells[3]).toHaveValue("2");
    await expect(cells[4]).toHaveValue("4");
    await expect(cells[5]).toHaveValue("6");

    await userEvent.click(cells[6]);
    await userEvent.type(cells[6], "789");
    await expect(cells[6]).toHaveValue("7");
    await expect(cells[7]).toHaveValue("8");
    await expect(cells[8]).toHaveValue("9");
  },
};

/* ─── 9. RTL ──────────────────────────────────────────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Right-to-left writing-mode. Cell order reverses (cell 1 sits " +
          "on the right); auto-advance still walks left-to-right " +
          "(Base UI follows direction context). The focus ring and " +
          "value badge follow logical properties.",
      },
    },
  },
  render: () => (
    <div dir="rtl" className="zs-story-row" role="group" aria-label="RTL OTP">
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <Field>
          <Field.Label>رمز التحقق</Field.Label>
          <OtpField data-testid="otp-rtl" />
          <Field.Description>
            تم إرسال رمز مكون من 6 أرقام إلى بريدك.
          </Field.Description>
        </Field>
      </div>
    </div>
  ),
};
