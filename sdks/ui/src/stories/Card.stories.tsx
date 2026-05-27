import type { Meta, StoryObj } from "@storybook/react";
import { Button, Card, Field, Input } from "../components";

const meta: Meta<typeof Card> = {
  title: "Components/Card",
  component: Card,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Card>;

/* A reusable colored "media" placeholder so stories stay self-contained
 * (no external image fetches; reproducible PNGs). Uses a solid accent
 * background — axe-glass invariant forbids gradients on user-content
 * surfaces, but here the placeholder is `aria-hidden` and has no
 * text descendants in the AT tree. The label is rendered inside an
 * inner badge with an opaque background to keep contrast resolvable. */
function MediaPlaceholder({ label }: { label: string }) {
  return (
    <div
      aria-hidden="true"
      style={{
        blockSize: "8rem",
        backgroundColor: "var(--zs-accent)",
        display: "grid",
        placeItems: "center",
        fontFamily: "var(--zs-font-system)",
      }}
    >
      <span
        style={{
          backgroundColor: "var(--zs-surface)",
          color: "var(--zs-label)",
          paddingInline: "var(--zs-space-3)",
          paddingBlock: "var(--zs-space-1)",
          borderRadius: "var(--zs-radius-2)",
          fontSize: "var(--zs-text-footnote-size)",
        }}
      >
        {label}
      </span>
    </div>
  );
}

/* ─── 1. All variants ────────────────────────────────────────────────── */
export const AllVariants: Story = {
  name: "All variants",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All card variants">
      {(["surface", "elevated", "outline", "ghost"] as const).map((variant) => (
        <Card
          key={variant}
          variant={variant}
          style={{ flex: "1 1 14rem", minInlineSize: "14rem" }}
        >
          <Card.Header>
            <div>
              <Card.Title>{variant}</Card.Title>
              <Card.Description>Lorem ipsum dolor sit amet.</Card.Description>
            </div>
          </Card.Header>
          <Card.Body>Tab tab content sit here.</Card.Body>
        </Card>
      ))}
    </div>
  ),
};

/* ─── 2. All sizes ───────────────────────────────────────────────────── */
export const AllSizes: Story = {
  name: "All sizes",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All card sizes">
      {(["sm", "md", "lg"] as const).map((size) => (
        <Card
          key={size}
          size={size}
          style={{ flex: "1 1 14rem", minInlineSize: "14rem" }}
        >
          <Card.Header>
            <div>
              <Card.Title>Card {size}</Card.Title>
              <Card.Description>
                Padding, gap, and radius scale with the size token.
              </Card.Description>
            </div>
          </Card.Header>
          <Card.Body>Body content goes here.</Card.Body>
        </Card>
      ))}
    </div>
  ),
};

/* ─── 3. Decomposed ──────────────────────────────────────────────────── */
export const Decomposed: Story = {
  name: "Decomposed",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Decomposed card">
      <Card style={{ inlineSize: "32rem", maxInlineSize: "100%" }}>
        <Card.Header>
          <div>
            <Card.Title>Project settings</Card.Title>
            <Card.Description>
              Identity, plan, and danger zone for this app.
            </Card.Description>
          </div>
          <Card.Action>
            <Button size="small" variant="tinted">
              Edit
            </Button>
          </Card.Action>
        </Card.Header>
        <Card.Body>
          <p>
            The Header / Body / Footer split is a layout convention; no
            ARIA on these layers. The accessible heading hierarchy comes
            from Card.Title.
          </p>
        </Card.Body>
        <Card.Footer>
          <Button variant="plain">Cancel</Button>
          <Button>Save changes</Button>
        </Card.Footer>
      </Card>
    </div>
  ),
};

/* ─── 4. With media ──────────────────────────────────────────────────── */
export const WithMedia: Story = {
  name: "With media",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Card with media slot">
      <Card style={{ inlineSize: "20rem" }}>
        <Card.Media side="top">
          <MediaPlaceholder label="cover image" />
        </Card.Media>
        <Card.Header>
          <div>
            <Card.Title>Sunset cover</Card.Title>
            <Card.Description>Edge-bleed top media slot.</Card.Description>
          </div>
        </Card.Header>
        <Card.Body>
          The card's overflow: hidden clips the media to the corner
          radius — no negative-margin overflow trick.
        </Card.Body>
      </Card>
    </div>
  ),
};

/* ─── 5. Interactive ─────────────────────────────────────────────────── */
export const Interactive: Story = {
  name: "Interactive",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Interactive card">
      <Card
        interactive
        variant="elevated"
        style={{ inlineSize: "20rem", cursor: "pointer" }}
      >
        <Card.Header>
          <div>
            <Card.Title>Hover & focus me</Card.Title>
            <Card.Description>
              tabIndex=0; focus-visible ring; hover background shift; active scale.
            </Card.Description>
          </div>
        </Card.Header>
        <Card.Body>
          The Card never adds its own onClick — the consumer wires it.
        </Card.Body>
      </Card>
    </div>
  ),
};

/* ─── 6. AsChild ─────────────────────────────────────────────────────── */
export const AsChild: Story = {
  name: "asChild",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Card asChild">
      <Card
        asChild
        interactive
        variant="outline"
        style={{ inlineSize: "20rem" }}
      >
        <a href="#cards" data-testid="card-as-child">
          <Card.Header>
            <div>
              <Card.Title>Whole-card link</Card.Title>
              <Card.Description>
                Renders as an &lt;a&gt; — the entire card is the click target.
              </Card.Description>
            </div>
          </Card.Header>
          <Card.Body>
            Consumer is responsible for nested-interactive concerns.
          </Card.Body>
        </a>
      </Card>
    </div>
  ),
};

/* ─── 7. Ghost ───────────────────────────────────────────────────────── */
export const Ghost: Story = {
  name: "Ghost",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Ghost card nesting">
      <Card variant="surface" style={{ inlineSize: "30rem" }}>
        <Card.Header>
          <div>
            <Card.Title>Outer surface</Card.Title>
            <Card.Description>Opaque base; safe for axe contrast.</Card.Description>
          </div>
        </Card.Header>
        <Card.Body>
          <Card variant="ghost" size="sm">
            <Card.Header>
              <div>
                <Card.Title>Inner ghost</Card.Title>
                <Card.Description>
                  Transparent — intended for nesting inside an opaque parent.
                </Card.Description>
              </div>
            </Card.Header>
            <Card.Body>Body content reads through the parent surface.</Card.Body>
          </Card>
        </Card.Body>
      </Card>
    </div>
  ),
};

/* ─── 8. With form inside ────────────────────────────────────────────── */
export const WithFormInside: Story = {
  name: "With form inside",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Card with form">
      <Card style={{ inlineSize: "26rem" }}>
        <Card.Header>
          <div>
            <Card.Title>Invite a teammate</Card.Title>
            <Card.Description>
              They'll get an email with a join link.
            </Card.Description>
          </div>
        </Card.Header>
        <Card.Body>
          <Field>
            <Field.Label>Email address</Field.Label>
            <Input type="email" placeholder="teammate@company.com" />
            <Field.Description>
              We'll never share their address.
            </Field.Description>
          </Field>
        </Card.Body>
        <Card.Footer>
          <Button variant="plain">Cancel</Button>
          <Button>Send invite</Button>
        </Card.Footer>
      </Card>
    </div>
  ),
};
