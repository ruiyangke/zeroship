import type { Meta, StoryObj } from "@storybook/react";
import { Button } from "../index";

const meta = {
  title: "Primitives/Button",
  component: Button,
  tags: ["autodocs"],
  args: {
    children: "Launch app",
  },
} satisfies Meta<typeof Button>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Variants: Story = {
  render: () => (
    <div className="zs-story-shell zs-story-row">
      <Button>Primary</Button>
      <Button variant="secondary">Secondary</Button>
      <Button variant="ghost">Ghost</Button>
      <Button variant="danger">Danger</Button>
    </div>
  ),
};

export const SizesAndStates: Story = {
  render: () => (
    <div className="zs-story-shell zs-story-stack">
      <div className="zs-story-row">
        <Button size="sm">Small</Button>
        <Button size="md">Medium</Button>
        <Button size="lg">Large</Button>
      </div>
      <div className="zs-story-row">
        <Button loading>Building</Button>
        <Button disabled>Disabled</Button>
      </div>
    </div>
  ),
};
