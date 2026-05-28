import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, within } from "@storybook/test";
import { useRef, useState, type CSSProperties } from "react";
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
function MediaPlaceholder({
  label,
  fill = false,
}: {
  label: string;
  /**
   * When `true`, the placeholder fills its parent (used inside
   * `Card.Media side="fill"` where the parent is `position: absolute;
   * inset: 0`). Otherwise it has a fixed 8rem block-size, which is the
   * right shape for top/bottom edge-bleed cells.
   */
  fill?: boolean;
}) {
  return (
    <div
      aria-hidden="true"
      style={{
        blockSize: fill ? "100%" : "8rem",
        inlineSize: fill ? "100%" : undefined,
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
            <Card.Title>{variant}</Card.Title>
            <Card.Description>Lorem ipsum dolor sit amet.</Card.Description>
          </Card.Header>
          <Card.Content>Tab content sits here.</Card.Content>
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
            <Card.Title>Card {size}</Card.Title>
            <Card.Description>
              Padding, gap, and radius scale with the size token.
            </Card.Description>
          </Card.Header>
          <Card.Content>Body content goes here.</Card.Content>
        </Card>
      ))}
    </div>
  ),
};

/* ─── 3. Decomposed ──────────────────────────────────────────────────── */
/* Direct Title + Description + Action — Header is a CSS Grid (col 1 ↔
 * Title/Description stack, col 2 ↔ Action spanning rows). No anonymous
 * wrapper div needed (slice-3 review-fix item 3). */
export const Decomposed: Story = {
  name: "Decomposed",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Decomposed card">
      <Card style={{ inlineSize: "32rem", maxInlineSize: "100%" }}>
        <Card.Header>
          <Card.Title>Project settings</Card.Title>
          <Card.Description>
            Identity, plan, and danger zone for this app.
          </Card.Description>
          <Card.Action>
            <Button size="small" variant="tinted">
              Edit
            </Button>
          </Card.Action>
        </Card.Header>
        <Card.Content>
          <p>
            The Header / Body / Footer split is a layout convention; no
            ARIA on these layers. The accessible heading hierarchy comes
            from Card.Title.
          </p>
        </Card.Content>
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
          <Card.Title>Sunset cover</Card.Title>
          <Card.Description>Edge-bleed top media slot.</Card.Description>
        </Card.Header>
        <Card.Content>
          The card's overflow: hidden clips the media to the corner
          radius — no negative-margin overflow trick.
        </Card.Content>
      </Card>
    </div>
  ),
};

/* ─── 4b. Media sides (review-fix item 20) ───────────────────────────── */
/* Covers the surviving `side` values — top, bottom, fill — after item
 * 7 removed left/right (never implemented; type narrowed pre-launch). */
export const MediaSides: Story = {
  name: "Media sides",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Card media sides">
      <Card style={{ inlineSize: "16rem" }}>
        <Card.Media side="top">
          <MediaPlaceholder label="side=top" />
        </Card.Media>
        <Card.Header>
          <Card.Title>Top</Card.Title>
          <Card.Description>Edge-bleed above Header.</Card.Description>
        </Card.Header>
        <Card.Content>Default placement.</Card.Content>
      </Card>
      <Card style={{ inlineSize: "16rem" }}>
        <Card.Header>
          <Card.Title>Bottom</Card.Title>
          <Card.Description>Edge-bleed below Body.</Card.Description>
        </Card.Header>
        <Card.Content>Footer-adjacent.</Card.Content>
        <Card.Media side="bottom">
          <MediaPlaceholder label="side=bottom" />
        </Card.Media>
      </Card>
      <Card
        variant="elevated"
        style={{ inlineSize: "16rem", color: "var(--zs-accent-ink)" }}
      >
        <Card.Media side="fill">
          <MediaPlaceholder label="side=fill (decorative)" fill />
        </Card.Media>
        <Card.Header>
          <Card.Title style={{ color: "var(--zs-accent-ink)" }}>
            Fill
          </Card.Title>
          <Card.Description style={{ color: "var(--zs-accent-ink)" }}>
            aria-hidden by default.
          </Card.Description>
        </Card.Header>
        <Card.Content>Content sits above the fill layer.</Card.Content>
      </Card>
    </div>
  ),
};

/* ─── 5. Interactive ─────────────────────────────────────────────────── */
/* Recommended pattern: asChild with a real anchor. The whole card is
 * the click target, and the browser owns focusability + keyboard
 * activation natively (Enter on anchors). Replaces the old "interactive
 * plain div without onClick" story which was a keyboard trap; that
 * scenario now lives in the CAUTION story below to document the
 * dev-mode warning. */
export const Interactive: Story = {
  name: "Interactive",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Interactive card (asChild anchor)">
      <Card
        asChild
        interactive
        variant="elevated"
        style={{ inlineSize: "20rem" }}
      >
        <a href="#cards" data-testid="card-interactive-anchor">
          <Card.Header>
            <Card.Title>Open project</Card.Title>
            <Card.Description>
              The whole card is the click target — a real &lt;a href&gt;.
            </Card.Description>
          </Card.Header>
          <Card.Content>
            Browser handles focusability + Enter activation natively.
          </Card.Content>
        </a>
      </Card>
    </div>
  ),
};

/* ─── 5b. Interactive with keyboard (review-fix items 1 + 20) ───────── */
/* When the consumer DOES want a plain-div interactive Card (e.g., the
 * whole card needs to trigger a JS-only action like opening a Dialog),
 * pass `onClick`. The Card sets role="button", tabIndex=0, and wires
 * Enter/Space to onClick — keyboard users can activate the same way
 * mouse users can. */
function InteractiveWithKeyboardImpl() {
  const [count, setCount] = useState(0);
  return (
    <div className="zs-story-row" role="group" aria-label="Interactive card with keyboard">
      <Card
        interactive
        variant="elevated"
        onClick={() => setCount((n) => n + 1)}
        data-testid="card-interactive-keyboard"
        style={{ inlineSize: "20rem" }}
      >
        <Card.Header>
          <Card.Title>Activate me</Card.Title>
          <Card.Description>
            Click, or focus + Enter, or focus + Space.
          </Card.Description>
        </Card.Header>
        <Card.Content>
          <p
            role="status"
            aria-label="Card activation count"
            data-testid="card-interactive-counter"
          >
            Activations: <strong>{count}</strong>
          </p>
        </Card.Content>
      </Card>
    </div>
  );
}
export const InteractiveWithKeyboard: Story = {
  name: "Interactive with keyboard",
  render: () => <InteractiveWithKeyboardImpl />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const card = canvas.getByRole("button", { name: /activate me/i });
    const status = canvas.getByRole("status", {
      name: /card activation count/i,
    });

    await userEvent.click(card);
    await expect(status).toHaveTextContent("Activations: 1");

    await userEvent.keyboard("{Enter}");
    await expect(status).toHaveTextContent("Activations: 2");

    await userEvent.keyboard(" ");
    await expect(status).toHaveTextContent("Activations: 3");
  },
};

/* ─── 5c. Interactive without onClick (CAUTION) ──────────────────────── */
/* DOCUMENTED ANTI-PATTERN: `interactive` without `onClick` puts a
 * focusable card in the tab order with no way to activate it. The
 * Card emits a dev-mode console.warn on render; keep this story so
 * the warning is exercised in the aria-wiring check. */
function InteractiveWithoutOnClickImpl() {
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Interactive card without onClick (caution)"
    >
      <Card
        interactive
        variant="outline"
        data-testid="card-interactive-no-onclick"
        style={{ inlineSize: "20rem" }}
      >
        <Card.Header>
          <Card.Title>Caution</Card.Title>
          <Card.Description>
            interactive=true without onClick — keyboard users can't
            activate. Dev console emits a warning.
          </Card.Description>
        </Card.Header>
        <Card.Content>
          Prefer asChild with a real &lt;a&gt; / &lt;button&gt;, or pass
          onClick.
        </Card.Content>
      </Card>
    </div>
  );
}
export const InteractiveWithoutOnClick: Story = {
  name: "Interactive without onClick (caution)",
  render: () => <InteractiveWithoutOnClickImpl />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const card = canvas.getByRole("button", { name: /caution/i });

    await userEvent.tab();
    await expect(card).toHaveFocus();
    await userEvent.keyboard("{Enter}");
    await expect(card).toHaveFocus();
  },
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
            <Card.Title>Whole-card link</Card.Title>
            <Card.Description>
              Renders as an &lt;a&gt; — the entire card is the click target.
            </Card.Description>
          </Card.Header>
          <Card.Content>
            Consumer is responsible for nested-interactive concerns.
          </Card.Content>
        </a>
      </Card>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const link = canvas.getByRole("link", { name: /whole-card link/i });

    await expect(link.tagName).toBe("A");
    await expect(link).toHaveAttribute("href", "#cards");
    await expect(link).toHaveAttribute("data-interactive");
    await userEvent.click(link);
    await expect(link).toHaveFocus();
  },
};

/* ─── 6b. AsChild ref composition (review-fix item 4) ───────────────── */
/* React-19 deprecated `element.ref` for function components; refs now
 * live on `element.props.ref`. This story wires BOTH a consumer-
 * supplied ref AND uses the Card root's own ref, and proves both
 * resolve to the rendered <a> via a tiny status panel.
 *
 * The verification region is itself a labeled Card so the
 * subject-Card and the verifier-Card sit as two equal, clearly-bounded
 * cells (visual-polish item 3 — fixes the "half-rendered" appearance
 * codex flagged when the previous bare-div verifier floated next to
 * the subject Card). */
function AsChildRefCompositionImpl() {
  const consumerRef = useRef<HTMLAnchorElement | null>(null);
  const [status, setStatus] = useState("idle");
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Card asChild ref composition"
    >
      <Card asChild variant="outline" style={{ inlineSize: "20rem" }}>
        <a
          href="#refs"
          ref={consumerRef}
          data-testid="card-aschild-ref-anchor"
        >
          <Card.Header>
            <Card.Title>Ref composition</Card.Title>
            <Card.Description>
              Consumer ref + Slot ref both land on the rendered anchor.
            </Card.Description>
          </Card.Header>
          <Card.Content>Click verify to compare.</Card.Content>
        </a>
      </Card>
      <Card variant="surface" style={{ inlineSize: "20rem" }}>
        <Card.Header>
          <Card.Title>Verification</Card.Title>
          <Card.Description>
            Click verify — the ref should resolve to the rendered &lt;a&gt;.
          </Card.Description>
        </Card.Header>
        <Card.Content>
          <Button
            size="small"
            variant="tinted"
            onClick={() => {
              const el = consumerRef.current;
              setStatus(
                el && el.tagName === "A" && el.getAttribute("href") === "#refs"
                  ? "ref-attached"
                  : "ref-missing",
              );
            }}
          >
            Verify consumer ref
          </Button>
          <span
            role="status"
            aria-label="Card ref status"
            data-testid="card-aschild-ref-status"
            style={
              {
                fontFamily: "var(--zs-font-system)",
                fontSize: "var(--zs-text-footnote-size)",
                color: "var(--zs-label-secondary)",
              } satisfies CSSProperties
            }
          >
            {status}
          </span>
        </Card.Content>
      </Card>
    </div>
  );
}
export const AsChildRefComposition: Story = {
  name: "asChild ref composition",
  render: () => <AsChildRefCompositionImpl />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const verify = canvas.getByRole("button", { name: /verify consumer ref/i });
    const status = canvas.getByRole("status", { name: /card ref status/i });

    await expect(status).toHaveTextContent("idle");
    await userEvent.click(verify);
    await expect(status).toHaveTextContent("ref-attached");
  },
};

function KeyboardPreventDefaultImpl() {
  const [status, setStatus] = useState("idle");
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Interactive card keyboard prevent default"
    >
      <Card
        interactive
        onClick={() => setStatus("activated")}
        onKeyDown={(event) => {
          if (event.key === " ") {
            event.preventDefault();
            setStatus("space-prevented");
          }
        }}
        style={{ inlineSize: "20rem" }}
      >
        <Card.Header>
          <Card.Title>Guarded card</Card.Title>
          <Card.Description>
            Space is intercepted by the caller; Enter still activates.
          </Card.Description>
        </Card.Header>
        <Card.Content>
          <output role="status" aria-label="Guarded card status">
            {status}
          </output>
        </Card.Content>
      </Card>
    </div>
  );
}
export const KeyboardPreventDefault: Story = {
  name: "Keyboard preventDefault (play)",
  render: () => <KeyboardPreventDefaultImpl />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const card = canvas.getByRole("button", { name: /guarded card/i });
    const status = canvas.getByRole("status", {
      name: /guarded card status/i,
    });

    await card.focus();
    await userEvent.keyboard(" ");
    await expect(status).toHaveTextContent("space-prevented");

    await userEvent.keyboard("{Enter}");
    await expect(status).toHaveTextContent("activated");
  },
};

export const TitleAsChildAndMediaOverride: Story = {
  name: "Title asChild and media override (play)",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Card asChild subparts"
    >
      <Card variant="outline" style={{ inlineSize: "22rem" }}>
        <Card.Media side="fill" aria-hidden={false}>
          <MediaPlaceholder label="meaningful fill" fill />
        </Card.Media>
        <Card.Header>
          <Card.Title asChild>
            <h2>Custom heading level</h2>
          </Card.Title>
          <Card.Description>
            The title renders as the supplied h2 and the fill media opts
            into the accessibility tree.
          </Card.Description>
        </Card.Header>
        <Card.Content>Subpart render-as behavior.</Card.Content>
        <Card.Footer align="between" divider="top">
          <Button size="small" variant="plain">
            Back
          </Button>
          <Button size="small">Continue</Button>
        </Card.Footer>
      </Card>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const heading = canvas.getByRole("heading", {
      name: /custom heading level/i,
      level: 2,
    });
    const footer = heading
      .closest("[data-slot='card']")
      ?.querySelector("[data-slot='card-footer']");
    const media = heading
      .closest("[data-slot='card']")
      ?.querySelector("[data-slot='card-media']");

    await expect(heading.tagName).toBe("H2");
    await expect(footer).toHaveAttribute("data-align", "between");
    await expect(footer).toHaveAttribute("data-divider", "top");
    await expect(media).toHaveAttribute("aria-hidden", "false");
  },
};

/* ─── 7. Ghost ───────────────────────────────────────────────────────── */
export const Ghost: Story = {
  name: "Ghost",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Ghost card nesting">
      <Card variant="surface" style={{ inlineSize: "30rem" }}>
        <Card.Header>
          <Card.Title>Outer surface</Card.Title>
          <Card.Description>Opaque base; safe for axe contrast.</Card.Description>
        </Card.Header>
        <Card.Content>
          <Card variant="ghost" size="sm">
            <Card.Header>
              <Card.Title>Inner ghost</Card.Title>
              <Card.Description>
                Transparent — intended for nesting inside an opaque parent.
              </Card.Description>
            </Card.Header>
            <Card.Content>Body content reads through the parent surface.</Card.Content>
          </Card>
        </Card.Content>
      </Card>
    </div>
  ),
};

/* ─── 8. With form inside ────────────────────────────────────────────── */
/* Convention: forms embedded inside a Card use smaller
 * controls (Input size="sm", Button size="small"). macOS list-row
 * forms ship mini controls; reading a form inside a Card with default
 * (md) controls feels chunky because the card already supplies the
 * outer container. The story models the recommended pattern so the
 * AI agent / consumer mimics it. Visual-polish item 7. */
export const WithFormInside: Story = {
  name: "With form inside",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Card with form">
      <Card style={{ inlineSize: "26rem" }}>
        <Card.Header>
          <Card.Title>Invite a teammate</Card.Title>
          <Card.Description>
            They'll get an email with a join link.
          </Card.Description>
        </Card.Header>
        <Card.Content>
          <Field>
            <Field.Label>Email address</Field.Label>
            <Input
              type="email"
              size="sm"
              placeholder="teammate@company.com"
            />
            <Field.Description>
              We'll never share their address.
            </Field.Description>
          </Field>
        </Card.Content>
        <Card.Footer divider="top">
          <Button size="small" variant="plain">
            Cancel
          </Button>
          <Button size="small">Send invite</Button>
        </Card.Footer>
      </Card>
    </div>
  ),
};
