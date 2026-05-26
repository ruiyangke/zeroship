import type { Meta, StoryObj } from "@storybook/react";
import { Button, Card, Input, Select, Textarea } from "../index";

function ContactForm() {
  return (
    <Card className="zs-story-form zs-story-card-pad">
      <header className="zs-story-form__header">
        <h2 className="zs-story-title">Contact support</h2>
        <p className="zs-story-subtle">We usually reply within one business day.</p>
      </header>
      <div className="zs-story-form__row">
        <Input label="Name" placeholder="Ada Lovelace" />
        <Input
          label="Email"
          type="email"
          defaultValue="ada@example"
          error="Enter a valid email address."
        />
      </div>
      <Select
        label="Topic"
        defaultValue="billing"
        items={[
          { value: "billing", label: "Billing" },
          { value: "bug", label: "Bug report" },
          { value: "other", label: "Something else" },
        ]}
      />
      <Textarea
        label="Message"
        rows={4}
        placeholder="How can we help?"
        hint="Include steps to reproduce if you're reporting a bug."
      />
      <div className="zs-story-form__actions">
        <Button variant="ghost">Clear</Button>
        <Button>Send message</Button>
      </div>
    </Card>
  );
}

const meta = {
  title: "Primitives/Forms",
  component: ContactForm,
  tags: ["autodocs"],
} satisfies Meta<typeof ContactForm>;

export default meta;
type Story = StoryObj<typeof meta>;

export const ContactSupport: Story = {};
