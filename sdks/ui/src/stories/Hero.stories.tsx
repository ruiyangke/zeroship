import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import { Hero } from "../sections";
import { Button, Badge, Icon } from "../components";
import { ArrowRight, Sparkles } from "lucide-react";

/* A decorative media placeholder for the split demos — a soft token-tinted
 * panel standing in for a product shot / illustration. SVG attribute units
 * (viewBox space), not CSS px; the fill reads a token via currentColor. */
const MediaPlaceholder = () => (
  <svg
    viewBox="0 0 480 360"
    role="presentation"
    style={{
      inlineSize: "100%",
      blockSize: "auto",
      display: "block",
      background: "var(--zs-fill-tertiary)",
      color: "var(--zs-label-quaternary)",
    }}
  >
    <rect x="40" y="48" width="400" height="40" rx="8" fill="currentColor" opacity="0.5" />
    <rect x="40" y="112" width="280" height="24" rx="6" fill="currentColor" opacity="0.35" />
    <rect x="40" y="156" width="340" height="24" rx="6" fill="currentColor" opacity="0.35" />
    <rect x="40" y="220" width="160" height="56" rx="12" fill="currentColor" opacity="0.5" />
  </svg>
);

const meta: Meta<typeof Hero> = {
  title: "Sections/Hero",
  component: Hero,
  parameters: { layout: "fullscreen" },
};

export default meta;

type Story = StoryObj<typeof Hero>;

/* ─── 1. Centered — eyebrow + h1 + description + 2 CTAs, no media ───────── */
export const Centered: Story = {
  name: "Centered (eyebrow + title + description + CTAs)",
  parameters: {
    docs: {
      description: {
        story:
          "The default centered band: a `Badge` eyebrow, an `<h1>` " +
          "headline, a muted description, and a primary + secondary CTA " +
          "row — all centered on a readable measure. No media, so the " +
          "band is a single centered column. The `<section>` is " +
          "`aria-labelledby` its `<h1>`.",
      },
    },
  },
  render: () => (
    <Hero
      data-testid="hero-centered"
      eyebrow={
        <Badge intent="info" variant="soft">
          <Icon as={Sparkles} size="sm" />
          New
        </Badge>
      }
      title="Ship software without writing code"
      description="Describe what you want in plain language. AI builds it, and the platform handles hosting, data, auth, and payments."
      actions={
        <>
          <Button>
            Get started
            <Icon as={ArrowRight} size="sm" />
          </Button>
          <Button variant="gray">Read the docs</Button>
        </>
      }
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("hero-centered");
    await expect(root).toHaveAttribute("data-slot", "hero");
    await expect(root.tagName).toBe("SECTION");
    await expect(root).toHaveAttribute("data-layout", "single");

    // The section is labelled by its real <h1>: aria-labelledby points at
    // the heading's id.
    const heading = canvas.getByRole("heading", {
      name: /ship software without writing code/i,
    });
    await expect(heading.tagName).toBe("H1");
    const labelledby = root.getAttribute("aria-labelledby");
    await expect(labelledby).toBeTruthy();
    await expect(heading.id).toBe(labelledby);

    // A CTA button is present and clickable.
    const cta = canvas.getByRole("button", { name: /get started/i });
    await expect(cta).toBeInTheDocument();
    await expect(cta).toBeEnabled();
  },
};

/* ─── 2. Split — text column + media column ─────────────────────────────── */
export const Split: Story = {
  name: "Split (text + media → two columns)",
  parameters: {
    docs: {
      description: {
        story:
          "With `media` set the band becomes a two-column split (text + " +
          "media) that collapses to a single stacked column below " +
          "`--zs-bp-md`. The text column start-aligns under a split. The " +
          "media wrapper is decorative (`aria-hidden`).",
      },
    },
  },
  render: () => (
    <Hero
      data-testid="hero-split"
      eyebrow={<Badge variant="soft">Platform</Badge>}
      title="From idea to live app in minutes"
      description="One prompt, one deploy. Your app gets a database, auth, storage, and a global edge runtime — no infrastructure to manage."
      actions={
        <>
          <Button>Start building</Button>
          <Button variant="gray">See an example</Button>
        </>
      }
      media={<MediaPlaceholder />}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("hero-split");
    await expect(root).toHaveAttribute("data-layout", "split");

    // Both regions render: the text column and the media column.
    const text = root.querySelector("[data-slot='hero-text']");
    await expect(text).not.toBeNull();
    const media = root.querySelector("[data-slot='hero-media']");
    await expect(media).not.toBeNull();
    // The media wrapper is decorative.
    await expect(media).toHaveAttribute("aria-hidden", "true");

    // Still labelled by its <h1>.
    const heading = canvas.getByRole("heading", {
      name: /from idea to live app/i,
    });
    await expect(heading.tagName).toBe("H1");
    await expect(heading.id).toBe(root.getAttribute("aria-labelledby"));
  },
};

/* ─── 3. Minimal — title + one CTA ──────────────────────────────────────── */
export const Minimal: Story = {
  name: "Minimal (title + one CTA)",
  parameters: {
    docs: {
      description: {
        story:
          "The leanest band: just a headline and a single CTA. No " +
          "eyebrow, description, or media.",
      },
    },
  },
  render: () => (
    <Hero
      data-testid="hero-minimal"
      title="Build something today"
      actions={<Button>Get started</Button>}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("hero-minimal");
    await expect(root).toHaveAttribute("data-layout", "single");
    const heading = canvas.getByRole("heading", {
      name: /build something today/i,
    });
    await expect(heading.tagName).toBe("H1");
    await expect(heading.id).toBe(root.getAttribute("aria-labelledby"));
    await expect(
      canvas.getByRole("button", { name: /get started/i }),
    ).toBeInTheDocument();
  },
};

/* ─── 4. Compound — the parts form ──────────────────────────────────────── */
export const Compound: Story = {
  name: "Compound (parts form, relevelled h2)",
  parameters: {
    docs: {
      description: {
        story:
          "The compound form gives full control over composition. Here " +
          "the headline is relevelled to `<h2>` via `Hero.Title asChild` " +
          "(the band is not the page's primary heading) — the section " +
          "still `aria-labelledby` the relevelled heading. A " +
          "`<Hero.Media>` child is lifted into the media column, so the " +
          "band reads as a split.",
      },
    },
  },
  render: () => (
    <Hero data-testid="hero-compound" align="start">
      <Hero.Eyebrow>
        <Badge variant="soft">Compound</Badge>
      </Hero.Eyebrow>
      <Hero.Title asChild>
        <h2>Composed from parts</h2>
      </Hero.Title>
      <Hero.Description>
        Use the compound parts when you need to relevel the heading or
        control ordering. The two surfaces are additive — pick one mode.
      </Hero.Description>
      <Hero.Actions>
        <Button>Primary</Button>
        <Button variant="plain">Secondary</Button>
      </Hero.Actions>
      <Hero.Media>
        <MediaPlaceholder />
      </Hero.Media>
    </Hero>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("hero-compound");
    // A compound Hero.Media child lifts into the media column → split.
    await expect(root).toHaveAttribute("data-layout", "split");

    // The relevelled heading is an <h2> and STILL labels the section.
    const heading = canvas.getByRole("heading", {
      name: /composed from parts/i,
    });
    await expect(heading.tagName).toBe("H2");
    await expect(heading.id).toBe(root.getAttribute("aria-labelledby"));

    // Media column present + decorative.
    const media = root.querySelector("[data-slot='hero-media']");
    await expect(media).not.toBeNull();
    await expect(media).toHaveAttribute("aria-hidden", "true");

    await expect(
      canvas.getByRole("button", { name: /primary/i }),
    ).toBeInTheDocument();
  },
};

/* ─── Regression stories (dual-review fixes) ────────────────────────────── */

/* R1 + R5 — every media source lands additively in the media column. The
 * `media` prop AND multiple `<Hero.Media>` children all render in the media
 * region; none are stranded in the text flow. Pre-fix: the prop wins and the
 * lifted child renders nowhere; a second `<Hero.Media>` strands in the text. */
export const MediaAdditive: Story = {
  tags: ["!autodocs"],
  render: () => (
    <Hero
      data-testid="hero-media-additive"
      title="Additive media"
      media={<div data-testid="prop-media" />}
    >
      <Hero.Media>
        <div data-testid="child-media-a" />
      </Hero.Media>
      <Hero.Media>
        <div data-testid="child-media-b" />
      </Hero.Media>
    </Hero>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("hero-media-additive");
    await expect(root).toHaveAttribute("data-layout", "split");

    const mediaRegion = root.querySelector("[data-slot='hero-media']");
    await expect(mediaRegion).not.toBeNull();
    const textRegion = root.querySelector("[data-slot='hero-text']");
    await expect(textRegion).not.toBeNull();

    // All three media sources render inside a media region (collected
    // additively); none appear inside the text column.
    for (const id of ["prop-media", "child-media-a", "child-media-b"]) {
      const el = canvas.getByTestId(id);
      await expect(el.closest("[data-slot='hero-media']")).not.toBeNull();
      await expect(el.closest("[data-slot='hero-text']")).toBeNull();
    }
  },
};

/* R2 — a Fragment-wrapped `<Hero.Media>` is still lifted into the media
 * column (the normalizer descends Fragments). Pre-fix: the flat walk does not
 * descend the Fragment, so the media stays in the text flow and the band is a
 * single column. */
export const FragmentMedia: Story = {
  tags: ["!autodocs"],
  render: () => (
    <Hero data-testid="hero-fragment-media" title="Fragment media">
      {
        <>
          <Hero.Media>
            <div data-testid="frag-media" />
          </Hero.Media>
        </>
      }
    </Hero>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("hero-fragment-media");
    await expect(root).toHaveAttribute("data-layout", "split");
    const fragMedia = canvas.getByTestId("frag-media");
    await expect(fragMedia.closest("[data-slot='hero-media']")).not.toBeNull();
  },
};

/* R3 — supplying BOTH a `title` prop and a `<Hero.Title>` child must not
 * produce two elements sharing the section label id. The root owns the id and
 * assigns it to exactly one primary headline (the prop, by precedence).
 * Pre-fix: both Titles read the shared `titleId` from context → two elements
 * with the same id. */
export const DualTitleNoDuplicateId: Story = {
  tags: ["!autodocs"],
  render: () => (
    <Hero data-testid="hero-dual-title" title="Prop headline">
      <Hero.Title>Compound headline</Hero.Title>
    </Hero>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("hero-dual-title");
    const labelledby = root.getAttribute("aria-labelledby");
    await expect(labelledby).toBeTruthy();

    // Exactly one element carries the section's labelling id.
    const matches = root.querySelectorAll(`#${CSS.escape(labelledby as string)}`);
    await expect(matches.length).toBe(1);

    // …and it is the primary headline (the `title` prop, by precedence).
    const labelled = matches[0] as HTMLElement;
    await expect(labelled.getAttribute("data-slot")).toBe("hero-title");
    await expect(labelled.textContent).toBe("Prop headline");
  },
};

/* R4 — a custom `id` on the PRIMARY `<Hero.Title>` does not desync the section
 * label: the root manages the label id, so the heading's id always equals the
 * section's aria-labelledby. Pre-fix: heading keeps `id="my-custom"` while the
 * section points at the minted titleId → dangling label. */
export const CustomTitleId: Story = {
  tags: ["!autodocs"],
  render: () => (
    <Hero data-testid="hero-custom-id">
      <Hero.Title id="my-custom">Headline</Hero.Title>
    </Hero>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("hero-custom-id");
    const heading = canvas.getByRole("heading", { name: /headline/i });
    await expect(heading.id).toBe(root.getAttribute("aria-labelledby"));
  },
};

/* R6 — with no headline at all the section must NOT emit a dangling
 * `aria-labelledby` (gated on hasHeadline). Pre-fix: the attribute is always
 * present, pointing at an id no element carries. */
export const NoHeadlineNoDanglingLabel: Story = {
  tags: ["!autodocs"],
  render: () => (
    <Hero data-testid="hero-no-headline">
      <Hero.Description>Body only</Hero.Description>
    </Hero>
  ),
  play: async ({ canvasElement }) => {
    // Suppress the expected dev-warn (no headline) so the spy noise stays out
    // of the test log; assert the label is absent.
    const warn = console.warn;
    // eslint-disable-next-line no-console
    console.warn = () => {};
    try {
      const canvas = within(canvasElement);
      const root = canvas.getByTestId("hero-no-headline");
      await expect(root.hasAttribute("aria-labelledby")).toBe(false);
    } finally {
      // eslint-disable-next-line no-console
      console.warn = warn;
    }
  },
};

/* R7 — `Hero.Title asChild` with a Fragment child is rejected and renders
 * NOTHING (a Fragment cannot carry the id/className/heading semantics).
 * Pre-fix: `isValidElement(<></>)` is true, so the guard passes and Slot
 * renders the Fragment's children bare — the marker text leaks into the DOM
 * with no real heading. Post-fix: the guard rejects the Fragment and returns
 * null, so the marker text never renders. */
const FRAG_MARKER = "frag-marker-xyz";
export const AsChildFragmentRejected: Story = {
  tags: ["!autodocs"],
  // Intentionally degenerate: the asChild Fragment is rejected so NO heading
  // renders, leaving the section's aria-labelledby unresolved. That dangling
  // reference is exactly the authoring mistake this story exercises, so the
  // axe pass would (correctly) flag it — disable a11y for this guard story.
  parameters: { a11y: { disable: true } },
  render: () => (
    <Hero data-testid="hero-aschild-fragment">
      <Hero.Title asChild>
        <>{FRAG_MARKER}</>
      </Hero.Title>
    </Hero>
  ),
  play: async ({ canvasElement }) => {
    // Spy on warn to keep the (dev-only) rejection notice out of the test log.
    const warn = console.warn;
    // eslint-disable-next-line no-console
    console.warn = () => {};
    try {
      const canvas = within(canvasElement);
      const root = canvas.getByTestId("hero-aschild-fragment");
      // No heading element is produced…
      await expect(root.querySelector("[data-slot='hero-title']")).toBeNull();
      await expect(root.querySelector("h1,h2,h3,h4,h5,h6")).toBeNull();
      // …and the Fragment's content never leaks into the DOM (pre-fix, Slot
      // renders the Fragment children bare → the marker text would appear).
      await expect(root.textContent).not.toContain(FRAG_MARKER);
    } finally {
      // eslint-disable-next-line no-console
      console.warn = warn;
    }
  },
};
