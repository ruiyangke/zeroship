import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import { FeatureGrid, type FeatureItem } from "../sections";
import { Icon } from "../components";
import {
  Zap,
  ShieldCheck,
  Gauge,
  Boxes,
  GitBranch,
  Globe,
  CreditCard,
  Sparkles,
} from "lucide-react";

const meta: Meta<typeof FeatureGrid> = {
  title: "Sections/FeatureGrid",
  component: FeatureGrid,
  parameters: { layout: "fullscreen" },
  argTypes: {
    tone: {
      control: "inline-radio",
      options: ["default", "muted", "accent"],
      description:
        "Full-bleed band tone (the shared page-rhythm system): default " +
        "(transparent), muted (subtle surface panel), accent (accent fill " +
        "with ink remapped to accent-ink).",
    },
  },
};

export default meta;

type Story = StoryObj<typeof FeatureGrid>;

/* The three-feature set used by the flagship story. Each item carries a
 * decorative Lucide icon (no label → aria-hidden), a title, and a short
 * description. */
const threeFeatures: FeatureItem[] = [
  {
    id: "fast",
    icon: <Icon as={Zap} size="lg" />,
    title: "Instant cold starts",
    description: "Sub-second boot on every request — no warm-up tax.",
  },
  {
    id: "secure",
    icon: <Icon as={ShieldCheck} size="lg" />,
    title: "Isolated by default",
    description: "One isolate per app, sandboxed end to end.",
  },
  {
    id: "scale",
    icon: <Icon as={Gauge} size="lg" />,
    title: "Scales to zero",
    description: "Pay for what runs; idle apps cost nothing.",
  },
];

/* ─── 1. ThreeUp — header lead-in + 3 centered features ──────────────────── */
export const ThreeUp: Story = {
  name: "ThreeUp (eyebrow + title + description, 3 features)",
  parameters: {
    docs: {
      description: {
        story:
          "The default band: an eyebrow + `<h2>` title + description above a " +
          "responsive Grid of three features that collapses to a single " +
          "column below `--zs-bp-md`. Each feature is a decorative Lucide " +
          "icon in a tinted badge + an `<h3>` title + a muted description, " +
          "centered. The section is labelled by the real `<h2>` title.",
      },
    },
  },
  render: () => (
    <FeatureGrid
      data-testid="fg-three"
      eyebrow="Why zeroship"
      title="Everything you need to ship"
      description="A platform that handles hosting, scaling, and security so you can focus on the app."
      features={threeFeatures}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("fg-three");
    await expect(root.tagName).toBe("SECTION");
    await expect(root).toHaveAttribute("data-slot", "feature-grid");

    // With a `title`, the section IS labelled by that real <h2>.
    const sectionTitle = canvas.getByRole("heading", {
      name: "Everything you need to ship",
    });
    await expect(sectionTitle.tagName).toBe("H2");
    const labelledby = root.getAttribute("aria-labelledby");
    await expect(labelledby).toBeTruthy();
    await expect(sectionTitle.id).toBe(labelledby);

    // Feature titles are real <h3>s.
    const fast = canvas.getByRole("heading", { name: "Instant cold starts" });
    await expect(fast.tagName).toBe("H3");
    await expect(canvas.getAllByRole("heading", { level: 3 }).length).toBe(3);

    // The icons are decorative — no accessible name leaks (an icon with an
    // accessible name would surface as an img role). There are none.
    await expect(canvas.queryAllByRole("img").length).toBe(0);
  },
};

/* ─── 2. FourColumns — columns=4 ─────────────────────────────────────────── */
export const FourColumns: Story = {
  name: "FourColumns (columns=4)",
  parameters: {
    docs: {
      description: {
        story:
          "A four-column grid (`columns={4}`) of eight features. The Grid " +
          "promotes to four columns at `--zs-bp-md` and up; below it the Grid " +
          "primitive itself collapses to a single stacked column.",
      },
    },
  },
  render: () => (
    <FeatureGrid
      data-testid="fg-four"
      title="A complete platform"
      columns={4}
      features={[
        {
          id: "build",
          icon: <Icon as={Boxes} size="lg" />,
          title: "Build",
          description: "Describe it; AI builds it.",
        },
        {
          id: "deploy",
          icon: <Icon as={GitBranch} size="lg" />,
          title: "Deploy",
          description: "Push and it's live.",
        },
        {
          id: "domains",
          icon: <Icon as={Globe} size="lg" />,
          title: "Domains",
          description: "Custom domains, managed TLS.",
        },
        {
          id: "billing",
          icon: <Icon as={CreditCard} size="lg" />,
          title: "Billing",
          description: "Stripe Connect, metered.",
        },
        {
          id: "fast",
          icon: <Icon as={Zap} size="lg" />,
          title: "Fast",
          description: "Sub-second cold starts.",
        },
        {
          id: "secure",
          icon: <Icon as={ShieldCheck} size="lg" />,
          title: "Secure",
          description: "Isolated per tenant.",
        },
        {
          id: "scale",
          icon: <Icon as={Gauge} size="lg" />,
          title: "Scalable",
          description: "Scales to zero.",
        },
        {
          id: "ai",
          icon: <Icon as={Sparkles} size="lg" />,
          title: "AI-native",
          description: "Built around generation.",
        },
      ]}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Eight feature <h3>s render.
    await expect(canvas.getAllByRole("heading", { level: 3 }).length).toBe(8);
    // Icons stay decorative.
    await expect(canvas.queryAllByRole("img").length).toBe(0);
  },
};

/* ─── 3. StartAligned — align="start", no eyebrow ────────────────────────── */
export const StartAligned: Story = {
  name: "StartAligned (align=start, no eyebrow)",
  parameters: {
    docs: {
      description: {
        story:
          "Start-aligned band (`align=\"start\"`) with a title + description " +
          "but no eyebrow. The header and each item's icon + text hug the " +
          "inline-start edge.",
      },
    },
  },
  render: () => (
    <FeatureGrid
      data-testid="fg-start"
      align="start"
      title="Built for builders"
      description="Opinionated defaults, full control when you need it."
      features={threeFeatures}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("fg-start");
    await expect(root).toHaveAttribute("data-align", "start");
    // The section is still labelled by its real <h2> title.
    const title = canvas.getByRole("heading", { name: "Built for builders" });
    await expect(title.tagName).toBe("H2");
    await expect(root.getAttribute("aria-labelledby")).toBe(title.id);
  },
};

/* ─── 4. Compound — FeatureGrid.Item parts + additive fall-through ───────── */
export const Compound: Story = {
  name: "Compound (FeatureGrid.Item parts, additive)",
  parameters: {
    docs: {
      description: {
        story:
          "The compound surface: items expressed via `<FeatureGrid.Item>` " +
          "parts. The surfaces are ADDITIVE — here one `features`-prop item " +
          "(Build) renders FIRST, then the compound Deploy/Scale items fall " +
          "through after it (no suppression). Feature titles render in order.",
      },
    },
  },
  render: () => (
    <FeatureGrid
      data-testid="fg-compound"
      title="From idea to production"
      features={[
        {
          id: "build",
          icon: <Icon as={Boxes} size="lg" />,
          title: "Build",
          description: "Describe what you want.",
        },
      ]}
    >
      <FeatureGrid.Item icon={<Icon as={GitBranch} size="lg" />} title="Deploy">
        Push and it's live.
      </FeatureGrid.Item>
      <FeatureGrid.Item
        icon={<Icon as={Gauge} size="lg" />}
        title="Scale"
        description="Scales to zero when idle."
      />
    </FeatureGrid>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    // The section <h2> plus the three feature <h3>s — prop item (Build)
    // FIRST, then the two compound items in order (additive fall-through).
    const itemTitles = canvas.getAllByRole("heading", { level: 3 });
    const names = itemTitles.map((h) => h.textContent);
    await expect(names).toEqual(["Build", "Deploy", "Scale"]);

    // The compound item's children-as-description renders.
    await expect(canvas.getByText("Push and it's live.")).toBeInTheDocument();
    // And the compound item's `description` prop renders.
    await expect(
      canvas.getByText("Scales to zero when idle."),
    ).toBeInTheDocument();
    // Icons stay decorative.
    await expect(canvas.queryAllByRole("img").length).toBe(0);
  },
};

/* ─── 5. Headerless — no eyebrow/title/description, no aria-labelledby ────── */
export const Headerless: Story = {
  name: "Headerless (no header → no aria-labelledby)",
  parameters: {
    docs: {
      description: {
        story:
          "Used headerless (no eyebrow/title/description), the `<section>` " +
          "carries NO `aria-labelledby` — the attr is gated on a real `<h2>` " +
          "title rendering, so there is never a dangling label reference (the " +
          "Hero R6 / PricingTable lesson). A consumer wraps the band under " +
          "their own page heading.",
      },
    },
  },
  render: () => (
    <FeatureGrid data-testid="fg-headerless" features={threeFeatures} />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("fg-headerless");
    // No header → no dangling aria-labelledby.
    await expect(root.hasAttribute("aria-labelledby")).toBe(false);
    // No section <h2> rendered; only the item <h3>s.
    await expect(root.querySelector('[data-slot="feature-grid-title"]')).toBeNull();
    await expect(canvas.queryAllByRole("heading", { level: 2 }).length).toBe(0);
    await expect(canvas.getAllByRole("heading", { level: 3 }).length).toBe(3);
  },
};
