import type { Meta, StoryObj } from "@storybook/react";
import { Button } from "../components/Button";

const meta: Meta<typeof Button> = {
  title: "Components/Button",
  component: Button,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Button>;

export const AllStyles: Story = {
  name: "All styles",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All button styles">
      <div className="zs-story-cell">
        <span className="zs-story-label">Filled</span>
        <Button variant="filled" role="primary">Save</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Tinted</span>
        <Button variant="tinted">Continue</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Gray</span>
        <Button variant="gray">More</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Plain</span>
        <Button variant="plain" role="cancel">Cancel</Button>
      </div>
    </div>
  ),
};

export const AllSizes: Story = {
  name: "All sizes",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All button sizes">
      <div className="zs-story-cell">
        <span className="zs-story-label">Small</span>
        <Button size="small">Save</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Medium</span>
        <Button size="medium">Save</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Large</span>
        <Button size="large">Save</Button>
      </div>
    </div>
  ),
};

export const AllStates: Story = {
  name: "All states",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All button states">
      <div className="zs-story-cell">
        <span className="zs-story-label">Default</span>
        <Button>Save</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Disabled</span>
        <Button disabled>Save</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Loading</span>
        <Button loading>Save</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Tinted disabled</span>
        <Button variant="tinted" disabled>Continue</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Plain loading</span>
        <Button variant="plain" loading>Cancel</Button>
      </div>
    </div>
  ),
};

export const Destructive: Story = {
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Destructive buttons">
      <div className="zs-story-cell">
        <span className="zs-story-label">Filled</span>
        <Button variant="filled" role="destructive">Delete</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Tinted</span>
        <Button variant="tinted" role="destructive">Delete</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Gray</span>
        <Button variant="gray" role="destructive">Delete</Button>
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Plain</span>
        <Button variant="plain" role="destructive">Delete</Button>
      </div>
    </div>
  ),
};
