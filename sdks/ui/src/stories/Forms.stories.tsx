import type { Meta, StoryObj } from "@storybook/react";
import { Input, Select, Textarea } from "../index";

function FormsDemo() {
  return (
    <div className="zs-story-shell zs-story-grid">
      <Input label="Project name" defaultValue="Supper Society" />
      <Input label="Slug" defaultValue="supper-society" hint="Used in the public URL." />
      <Input label="API key" defaultValue="sk_live_hidden" readOnly />
      <Input label="Budget" defaultValue="$15" error="Enter a monthly limit." />
      <Textarea
        label="Brief"
        defaultValue="A recipe journal for our supper club, with ratings and a monthly host vote."
      />
      <Select label="Plan" defaultValue="maker">
        <option value="free">Free</option>
        <option value="maker">Maker</option>
        <option value="pro">Pro</option>
      </Select>
    </div>
  );
}

const meta = {
  title: "Primitives/Forms",
  component: FormsDemo,
  tags: ["autodocs"],
} satisfies Meta<typeof FormsDemo>;

export default meta;
type Story = StoryObj<typeof meta>;

export const InputTextareaSelect: Story = {};
