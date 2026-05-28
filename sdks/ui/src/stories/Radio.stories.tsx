import type { Meta, StoryObj } from "@storybook/react";
import { useEffect, useRef, useState } from "react";
import { Form } from "@base-ui/react/form";
import { Button, Field, Radio } from "../components";

const meta: Meta<typeof Radio> = {
  title: "Components/Radio",
  component: Radio,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Radio>;

/* ─── 1. Two options ───────────────────────────────────────────────── */
export const TwoOptions: Story = {
  name: "Two options",
  parameters: {
    docs: {
      description: {
        story:
          "The canonical Radio shape — pick exactly one. The Radio.Group " +
          "carries the value; each Radio carries its discriminant `value` " +
          "and an inline label. Arrow keys move focus AND selection " +
          "between siblings (Base UI roving), verified by the " +
          "aria-wiring assertion.",
      },
    },
  },
  render: function TwoOptionsRender() {
    return (
      <div className="zs-story-row" role="group" aria-label="Two options">
        <div className="zs-story-cell" style={{ maxWidth: "22rem" }}>
          <Radio.Group
            defaultValue="email"
            name="contact-method"
            data-testid="radio-group-two"
            aria-label="Preferred contact method"
          >
            <Radio value="email" label="Email" data-testid="radio-two-email" />
            <Radio value="sms" label="Text message" data-testid="radio-two-sms" />
          </Radio.Group>
        </div>
      </div>
    );
  },
};

/* ─── 2. Five options ──────────────────────────────────────────────── */
export const FiveOptions: Story = {
  name: "Five options (max comfortable)",
  parameters: {
    docs: {
      description: {
        story:
          "Five options is the comfortable upper bound. Beyond that, a " +
          "Select is the better surface (later slice). The wrapping " +
          "Field carries the group label and description.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Five options">
      <div className="zs-story-cell" style={{ maxWidth: "22rem" }}>
        <Field>
          <Field.Label>Plan</Field.Label>
          <Radio.Group defaultValue="pro" name="plan">
            <Radio value="free" label="Free — 1 project" />
            <Radio value="starter" label="Starter — 5 projects" />
            <Radio value="pro" label="Pro — 20 projects" />
            <Radio value="team" label="Team — 100 projects" />
            <Radio value="enterprise" label="Enterprise — unlimited" />
          </Radio.Group>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 3. Horizontal ────────────────────────────────────────────────── */
export const Horizontal: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Three options on a single row (`orientation=\"horizontal\"`). " +
          "Useful for tight binary-ish choice (yes / no / maybe) — keep " +
          "the option labels short so the row stays readable.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Horizontal layout">
      <div className="zs-story-cell" style={{ maxWidth: "32rem" }}>
        <Field>
          <Field.Label>Workspace visibility</Field.Label>
          <Radio.Group
            orientation="horizontal"
            defaultValue="private"
            name="visibility"
          >
            <Radio value="private" label="Private" />
            <Radio value="team" label="Team" />
            <Radio value="public" label="Public" />
          </Radio.Group>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 4. All sizes ─────────────────────────────────────────────────── */
export const AllSizes: Story = {
  name: "All sizes",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All radio sizes">
      <div className="zs-story-cell">
        <span className="zs-story-label">Small</span>
        <Radio.Group size="sm" defaultValue="a" name="sizes-sm">
          <Radio value="a" label="Alpha" />
          <Radio value="b" label="Bravo" />
        </Radio.Group>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Medium</span>
        <Radio.Group size="md" defaultValue="a" name="sizes-md">
          <Radio value="a" label="Alpha" />
          <Radio value="b" label="Bravo" />
        </Radio.Group>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Large</span>
        <Radio.Group size="lg" defaultValue="a" name="sizes-lg">
          <Radio value="a" label="Alpha" />
          <Radio value="b" label="Bravo" />
        </Radio.Group>
      </div>
    </div>
  ),
};

/* ─── 5. With label (Field integration) ────────────────────────────── */
export const WithLabel: Story = {
  name: "With label",
  parameters: {
    docs: {
      description: {
        story:
          "A Field wrapping the entire Radio.Group. Field.Label labels " +
          "the group; each Radio carries its own per-option label. The " +
          "group is what `aria-required` and validation attach to.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With label">
      <div className="zs-story-cell" style={{ maxWidth: "24rem" }}>
        <Field>
          <Field.Label>Theme</Field.Label>
          <Radio.Group defaultValue="system" name="theme">
            <Radio value="light" label="Light" />
            <Radio value="dark" label="Dark" />
            <Radio value="system" label="System default" />
          </Radio.Group>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 6. With description ──────────────────────────────────────────── */
export const WithDescription: Story = {
  name: "With description",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With description">
      <div className="zs-story-cell" style={{ maxWidth: "26rem" }}>
        <Field>
          <Field.Label>Deploy target</Field.Label>
          <Radio.Group defaultValue="prod" name="deploy-target">
            <Radio value="dev" label="Development" />
            <Radio value="staging" label="Staging" />
            <Radio value="prod" label="Production" />
          </Radio.Group>
          <Field.Description>
            Switching target rebuilds the bundle. Save your work first.
          </Field.Description>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 7. Required + Field.Error ────────────────────────────────────── */
export const Required: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Required Radio.Group inside a form. Submitting without a " +
          "selection triggers `valueMissing` on the group's hidden input " +
          "and renders the Field.Error. The aria-wiring assertion " +
          "submits the form and verifies aria-invalid + the Field.Error " +
          "text both become visible.",
      },
    },
  },
  render: function RequiredRender() {
    return (
      <div className="zs-story-row" role="group" aria-label="Required radio">
        <Form
          className="zs-story-cell"
          style={{ maxWidth: "24rem", display: "flex", flexDirection: "column", gap: "0.75rem" }}
          onSubmit={(e) => {
            e.preventDefault();
          }}
          data-testid="radio-required-form"
        >
          <Field required>
            <Field.Label>
              Account type <Field.Required />
            </Field.Label>
            {/* `required` intentionally omitted on the Radio.Group —
                each contained Radio reads required from the Field
                context. Slice-4 review fix item 7 (cascade test). */}
            <Radio.Group
              name="account-type"
              data-testid="radio-required-group"
            >
              <Radio value="personal" label="Personal" />
              <Radio value="business" label="Business" />
            </Radio.Group>
            <Field.Error match="valueMissing">
              Pick one to continue.
            </Field.Error>
          </Field>
          <Button
            type="submit"
            variant="filled"
            data-testid="radio-required-submit"
          >
            Continue
          </Button>
        </Form>
      </div>
    );
  },
};

/* ─── 7b. Required + Field.Error — POST-SUBMIT VISUAL EVIDENCE ────────
 *
 * Slice-4 visual-polish item 1. Companion to `Required` that auto-
 * submits on mount so the screenshot lands in the validation-failed
 * state. Red error text appears below the group; the hidden inputs
 * carry `aria-invalid="true"`. Same useEffect → ref.current.click()
 * pattern the Checkbox RequiredInvalid story uses — robust across
 * Storybook 8 dev + static captures. */
export const RequiredInvalid: Story = {
  name: "Required — post-submit (invalid)",
  parameters: {
    docs: {
      description: {
        story:
          "Companion to `Required` that auto-submits on mount so the " +
          "capture lands in the validation-failed state. Red error text " +
          "renders below the radio group and each radio's hidden input " +
          "carries `aria-invalid=\"true\"`. The Form's onSubmit " +
          "preventDefault's so nothing navigates. Visual-polish item 1.",
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
      <div className="zs-story-row" role="group" aria-label="Required radio (invalid)">
        <Form
          className="zs-story-cell"
          style={{ maxWidth: "24rem", display: "flex", flexDirection: "column", gap: "0.75rem" }}
          onSubmit={(e) => {
            e.preventDefault();
          }}
          data-testid="radio-required-invalid-form"
        >
          <Field required>
            <Field.Label>
              Account type <Field.Required />
            </Field.Label>
            <Radio.Group
              name="account-type-invalid"
              data-testid="radio-required-invalid-group"
            >
              <Radio value="personal" label="Personal" />
              <Radio value="business" label="Business" />
            </Radio.Group>
            <Field.Error match="valueMissing">
              Pick one to continue.
            </Field.Error>
          </Field>
          <Button
            ref={submitRef}
            type="submit"
            variant="filled"
            data-testid="radio-required-invalid-submit"
          >
            Continue
          </Button>
        </Form>
      </div>
    );
  },
};

/* ─── 8. Disabled ──────────────────────────────────────────────────── */
export const Disabled: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Whole group disabled via the `<Radio.Group disabled>` cascade. " +
          "The row reads grayed-out together — labels, chips, and " +
          "wrapping rows all pick up the disabled token.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled group">
      <div className="zs-story-cell" style={{ maxWidth: "22rem" }}>
        <Field>
          <Field.Label>Plan</Field.Label>
          <Radio.Group disabled defaultValue="pro" name="disabled-group">
            <Radio value="free" label="Free" />
            <Radio value="pro" label="Pro" />
            <Radio value="enterprise" label="Enterprise" />
          </Radio.Group>
          <Field.Description>Upgrade your account to change.</Field.Description>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 9. Disabled item only ────────────────────────────────────────── */
export const DisabledItem: Story = {
  name: "Disabled item only",
  parameters: {
    docs: {
      description: {
        story:
          "Group enabled, one Radio disabled (e.g. option not available " +
          "on the current plan). The disabled chip still focuses via " +
          "arrow keys but doesn't accept selection.",
      },
    },
  },
  render: function DisabledItemRender() {
    const [value, setValue] = useState("pro");
    return (
      <div className="zs-story-row" role="group" aria-label="Disabled item">
        <div className="zs-story-cell" style={{ maxWidth: "26rem" }}>
          <Field>
            <Field.Label>Plan</Field.Label>
            <Radio.Group
              value={value}
              onValueChange={(v) => setValue(String(v))}
              name="disabled-item"
            >
              <Radio value="free" label="Free" />
              <Radio value="pro" label="Pro" />
              <Radio value="enterprise" label="Enterprise (contact sales)" disabled />
            </Radio.Group>
          </Field>
        </div>
      </div>
    );
  },
};

/* ─── 10. RTL ──────────────────────────────────────────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew option labels. The chip stays on the inline-start side " +
          "of each row (visually right in RTL); the centered dot inside " +
          "the chip stays centered regardless of direction.",
      },
    },
  },
  render: () => (
    <div dir="rtl" className="zs-story-row" role="group" aria-label="RTL radio group">
      <div className="zs-story-cell" style={{ maxWidth: "24rem" }}>
        <Field>
          <Field.Label>שפת ממשק</Field.Label>
          <Radio.Group defaultValue="he" name="language-rtl">
            <Radio value="he" label="עברית" />
            <Radio value="en" label="אנגלית" />
            <Radio value="ar" label="ערבית" />
          </Radio.Group>
        </Field>
      </div>
    </div>
  ),
};
