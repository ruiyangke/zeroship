import type { Meta, StoryObj } from "@storybook/react";
import { Badge, Button, Card, Chip, EmptyState, Spinner } from "../index";

function SurfacesDemo() {
  return (
    <div className="zs-story-shell zs-story-stack">
      <div className="zs-story-grid">
        <Card className="zs-story-card-pad">
          <div className="zs-story-stack">
            <Badge>Draft</Badge>
            <h2 className="zs-story-title">Recipe Journal</h2>
            <p className="zs-story-subtle">
              Posts, photos, reactions, and a warm cookbook tone.
            </p>
          </div>
        </Card>
        <Card tone="accent" className="zs-story-card-pad">
          <div className="zs-story-stack">
            <Badge tone="success">Live</Badge>
            <h2 className="zs-story-title">supper-society.zeroship.app</h2>
            <p className="zs-story-subtle">Published just now.</p>
          </div>
        </Card>
        <Card tone="danger" className="zs-story-card-pad">
          <div className="zs-story-stack">
            <Badge tone="danger">Danger</Badge>
            <h2 className="zs-story-title">Delete project</h2>
            <p className="zs-story-subtle">Permanent operations sit apart.</p>
          </div>
        </Card>
      </div>
      <div className="zs-story-row">
        <Chip active>All</Chip>
        <Chip>Drafts</Chip>
        <Chip tone="success">Live</Chip>
        <Chip tone="warn">Needs review</Chip>
        <Spinner />
      </div>
      <EmptyState
        title="Quiet on this front"
        description="Deploy once and the ledger will start collecting plain-language events."
        action={<Button variant="secondary">Open preview</Button>}
      />
    </div>
  );
}

const meta = {
  title: "Primitives/Surfaces",
  component: SurfacesDemo,
  tags: ["autodocs"],
} satisfies Meta<typeof SurfacesDemo>;

export default meta;
type Story = StoryObj<typeof meta>;

export const CardsBadgesChipsEmptyStateSpinner: Story = {};
