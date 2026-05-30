import type { Meta, StoryObj } from "@storybook/react";
import { expect, fn, spyOn, userEvent, within } from "@storybook/test";
import { PricingTable, type PricingTier } from "../sections";
import { Button } from "../components";

const meta: Meta<typeof PricingTable> = {
  title: "Sections/PricingTable",
  component: PricingTable,
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

type Story = StoryObj<typeof PricingTable>;

/* A stable module-level spy for the FeaturedHighlight CTA so the same
 * reference is wired at render time AND asserted in play(). */
const upgradeSpy = fn();

/* The console.error spy for the KeyCollision (F2) story. Installed in that
 * story's `beforeEach` (so it captures React's render-time duplicate-key
 * warning) and asserted in its `play()`. */
let keyCollisionErrorSpy: ReturnType<typeof spyOn> | null = null;

/* The three-tier set used by the flagship stories. The middle tier is
 * featured ("Most popular"). Each tier carries a price + period, a one-line
 * description, a mix of included/excluded features, and a CTA. */
const threeTiers: PricingTier[] = [
  {
    id: "free",
    name: "Free",
    price: "$0",
    period: "/mo",
    description: "For trying things out.",
    features: [
      { label: "1 project" },
      { label: "Community support" },
      { label: "1 GB storage", note: "soft cap" },
      { label: "Custom domains", included: false },
      { label: "Priority support", included: false },
    ],
    ctaLabel: "Get started",
  },
  {
    id: "pro",
    name: "Pro",
    price: "$29",
    period: "/mo",
    featured: true,
    badge: "Most popular",
    description: "For growing teams shipping in production.",
    features: [
      { label: "Unlimited projects" },
      { label: "Email support" },
      { label: "100 GB storage" },
      { label: "Custom domains" },
      { label: "Priority support", included: false },
    ],
    ctaLabel: "Start free trial",
  },
  {
    id: "enterprise",
    name: "Enterprise",
    price: "Custom",
    description: "For organizations with advanced needs.",
    features: [
      { label: "Unlimited projects" },
      { label: "Dedicated support" },
      { label: "Unlimited storage" },
      { label: "Custom domains" },
      { label: "Priority support" },
      { label: "SSO & audit logs" },
    ],
    cta: (
      <Button variant="tinted" className="zs-pricing__cta-button">
        Contact sales
      </Button>
    ),
  },
];

/* ─── 1. ThreeTiers — Free / Pro[featured] / Enterprise ─────────────────── */
export const ThreeTiers: Story = {
  name: "ThreeTiers (Free / Pro[featured] / Enterprise)",
  parameters: {
    docs: {
      description: {
        story:
          "The default band: three tiers in a responsive Grid that collapses " +
          "to a single column below `--zs-bp-md`. The featured Pro tier is " +
          "elevated + accent-ringed and carries a 'Most popular' badge. Each " +
          "tier name is an `<h3>`; the feature list is a real `<ul>` where " +
          "inclusion is conveyed by an icon + a visually-hidden word, never " +
          "color alone. CTAs pin to the card bottom for cross-tier alignment.",
      },
    },
  },
  render: () => <PricingTable data-testid="pricing-three" tiers={threeTiers} />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("pricing-three");
    await expect(root.tagName).toBe("SECTION");
    await expect(root).toHaveAttribute("data-slot", "pricing-table");
    // No section-level headline → no dangling aria-labelledby.
    await expect(root).not.toHaveAttribute("aria-labelledby");

    // Tier names are headings (h3 by default).
    const proHeading = canvas.getByRole("heading", { name: "Pro" });
    await expect(proHeading.tagName).toBe("H3");
    await expect(
      canvas.getByRole("heading", { name: "Free" }),
    ).toBeInTheDocument();
    await expect(
      canvas.getByRole("heading", { name: "Enterprise" }),
    ).toBeInTheDocument();

    // The feature lists are real <ul>s (one per tier).
    const lists = canvas.getAllByRole("list");
    await expect(lists.length).toBe(3);

    // Inclusion is conveyed beyond color: an excluded feature exposes the
    // visually-hidden "Not included" word, and included ones expose
    // "Included".
    await expect(canvas.getAllByText("Not included").length).toBeGreaterThan(0);
    await expect(canvas.getAllByText("Included").length).toBeGreaterThan(0);

    // The featured tier renders its visible badge text.
    await expect(canvas.getByText("Most popular")).toBeInTheDocument();

    // CTAs are real, enabled buttons.
    const trialCta = canvas.getByRole("button", { name: "Start free trial" });
    await expect(trialCta).toBeEnabled();
    await expect(
      canvas.getByRole("button", { name: "Contact sales" }),
    ).toBeInTheDocument();
  },
};

/* ─── 2. Compound — the same band via PricingTable.Tier / .Feature ──────── */
export const Compound: Story = {
  name: "Compound (PricingTable.Tier + .Feature parts)",
  parameters: {
    docs: {
      description: {
        story:
          "The compound surface: tiers expressed via `<PricingTable.Tier>` + " +
          "`<PricingTable.Feature>` parts. The surfaces are ADDITIVE — here " +
          "one `tiers`-prop tier (Free) renders FIRST, then the compound " +
          "Pro/Enterprise tiers fall through after it (no suppression).",
      },
    },
  },
  render: () => (
    <PricingTable
      data-testid="pricing-compound"
      tiers={[
        {
          id: "free",
          name: "Free",
          price: "$0",
          period: "/mo",
          description: "For trying things out.",
          features: [
            { label: "1 project" },
            { label: "Priority support", included: false },
          ],
          ctaLabel: "Get started",
        },
      ]}
    >
      <PricingTable.Tier
        name="Pro"
        price="$29"
        period="/mo"
        featured
        badge="Most popular"
        description="For growing teams."
        cta={<Button className="zs-pricing__cta-button">Upgrade</Button>}
      >
        <PricingTable.Feature>Unlimited projects</PricingTable.Feature>
        <PricingTable.Feature note="100 GB">Storage</PricingTable.Feature>
        <PricingTable.Feature included={false}>SSO</PricingTable.Feature>
      </PricingTable.Tier>
      <PricingTable.Tier
        name="Enterprise"
        price="Custom"
        description="For large organizations."
        cta={
          <Button variant="tinted" className="zs-pricing__cta-button">
            Contact sales
          </Button>
        }
      >
        <PricingTable.Feature>Unlimited projects</PricingTable.Feature>
        <PricingTable.Feature>SSO &amp; audit logs</PricingTable.Feature>
        <PricingTable.Feature>Dedicated support</PricingTable.Feature>
      </PricingTable.Tier>
    </PricingTable>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    // All three tiers render — the prop tier (Free) FIRST, the two compound
    // tiers after (additive fall-through, no suppression).
    const headings = canvas.getAllByRole("heading");
    const names = headings.map((h) => h.textContent);
    await expect(names).toEqual(["Free", "Pro", "Enterprise"]);
    await expect(headings[0].tagName).toBe("H3");

    // Compound features render the same row markup — the excluded SSO row
    // exposes the visually-hidden word.
    await expect(canvas.getByText("SSO")).toBeInTheDocument();
    await expect(canvas.getAllByText("Not included").length).toBeGreaterThan(0);

    // The featured compound tier renders its badge + a consumer CTA.
    await expect(canvas.getByText("Most popular")).toBeInTheDocument();
    await expect(
      canvas.getByRole("button", { name: "Upgrade" }),
    ).toBeInTheDocument();
  },
};

/* ─── 3. TwoTiers — proves the column count adapts ──────────────────────── */
export const TwoTiers: Story = {
  name: "TwoTiers (Free / Pro — column count adapts)",
  parameters: {
    docs: {
      description: {
        story:
          "Two tiers — the responsive Grid promotes to a two-column row at " +
          "`--zs-bp-md` (one column per tier, capped at four). Proves the " +
          "column count derives from the tier count.",
      },
    },
  },
  render: () => (
    <PricingTable
      data-testid="pricing-two"
      title="Simple, transparent pricing"
      description="Start free. Upgrade when you grow."
      tiers={[
        {
          id: "free",
          name: "Free",
          price: "$0",
          period: "/mo",
          description: "For individuals.",
          features: [{ label: "1 project" }, { label: "SSO", included: false }],
          ctaLabel: "Get started",
        },
        {
          id: "pro",
          name: "Pro",
          price: "$29",
          period: "/mo",
          featured: true,
          badge: "Most popular",
          description: "For teams.",
          features: [{ label: "Unlimited projects" }, { label: "SSO" }],
          ctaLabel: "Start free trial",
        },
      ]}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("pricing-two");

    // With a `title` lead-in, the section IS labelled by that real heading.
    const sectionTitle = canvas.getByRole("heading", {
      name: "Simple, transparent pricing",
    });
    await expect(sectionTitle.tagName).toBe("H2");
    const labelledby = root.getAttribute("aria-labelledby");
    await expect(labelledby).toBeTruthy();
    await expect(sectionTitle.id).toBe(labelledby);

    // Exactly two tiers → two feature <ul>s.
    await expect(canvas.getAllByRole("list").length).toBe(2);
  },
};

/* ─── 4. FeaturedHighlight — showcase the featured treatment + spy CTA ──── */
export const FeaturedHighlight: Story = {
  name: "FeaturedHighlight (featured ring + badge + CTA spy)",
  parameters: {
    docs: {
      description: {
        story:
          "Showcases the featured treatment: the recommended tier is elevated " +
          "+ accent-ringed and carries a 'Most popular' badge — and that " +
          "'recommended' meaning is carried by the VISIBLE badge text, never " +
          "the ring alone. Clicking the featured CTA fires `onCtaClick`.",
      },
    },
  },
  render: () => {
    return (
      <PricingTable
        data-testid="pricing-featured"
        tiers={[
          {
            id: "starter",
            name: "Starter",
            price: "$9",
            period: "/mo",
            description: "The basics.",
            features: [{ label: "1 seat" }, { label: "Analytics", included: false }],
            ctaLabel: "Choose Starter",
          },
          {
            id: "pro",
            name: "Pro",
            price: "$29",
            period: "/mo",
            featured: true,
            badge: "Most popular",
            description: "Everything in Starter, plus the good stuff.",
            features: [
              { label: "5 seats" },
              { label: "Analytics" },
              { label: "Priority support" },
            ],
            ctaLabel: "Upgrade to Pro",
            onCtaClick: upgradeSpy,
          },
        ]}
      />
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    upgradeSpy.mockClear();

    // The featured tier renders its visible badge text (the AT-facing
    // "recommended" signal).
    await expect(canvas.getByText("Most popular")).toBeInTheDocument();

    // The featured CTA is clickable and fires onCtaClick.
    const cta = canvas.getByRole("button", { name: "Upgrade to Pro" });
    await expect(cta).toBeEnabled();
    await userEvent.click(cta);
    await expect(upgradeSpy).toHaveBeenCalledTimes(1);
  },
};

/* ─── 5. FeaturedNoBadge — F1 regression ─────────────────────────────────── */
/* A featured tier with NO consumer-supplied `badge` must STILL carry a
 * visible/AT-readable "recommended" label — never the accent ring / elevation
 * alone (the house "never color alone" rule). Asserts a default visible
 * "Most popular" badge text renders for a featured tier that omits `badge`. */
export const FeaturedNoBadge: Story = {
  name: "FeaturedNoBadge (F1: featured defaults a visible badge)",
  parameters: {
    docs: {
      description: {
        story:
          "Regression for F1: a `featured` tier that omits `badge` must still " +
          "render a visible 'Most popular' label so 'recommended' is never " +
          "signalled by the ring/elevation alone (color-blind + SR users).",
      },
    },
  },
  render: () => (
    <PricingTable
      data-testid="pricing-featured-nobadge"
      // h2 tier names so the isolated story has a valid heading outline
      // (no surrounding page h1/h2) — keeps axe's heading-order check happy
      // so the test bites ONLY on the F1 default-badge contract.
      headingLevel="h2"
      tiers={[
        {
          id: "free",
          name: "Free",
          price: "$0",
          period: "/mo",
          features: [{ label: "1 project" }],
          ctaLabel: "Get started",
        },
        {
          // featured, but NO badge supplied → must default a visible label.
          id: "pro",
          name: "Pro",
          price: "$29",
          period: "/mo",
          featured: true,
          features: [{ label: "Unlimited projects" }],
          ctaLabel: "Upgrade",
        },
      ]}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // The featured tier renders a default visible "Most popular" badge even
    // though the consumer supplied no `badge` — "recommended" is never the
    // ring/elevation alone.
    await expect(canvas.getByText("Most popular")).toBeInTheDocument();
  },
};

/* ─── 6. KeyCollision — F2 regression ────────────────────────────────────── */
/* The two additive surfaces (prop tiers + compound tiers) must namespace
 * their React keys so a prop tier `id="pro"` and a compound tier whose key
 * resolves to "pro" do NOT collide into a duplicate-key React warning. */
export const KeyCollision: Story = {
  name: "KeyCollision (F2: namespaced keys across surfaces)",
  parameters: {
    docs: {
      description: {
        story:
          "Regression for F2: a prop tier `id=\"pro\"` AND a compound " +
          "`<PricingTable.Tier key=\"pro\">` must render without a React " +
          "duplicate-key warning — keys are namespaced by source surface.",
      },
    },
  },
  // Install the console.error spy BEFORE the story renders — React logs the
  // duplicate-key warning at render time (before play() runs), so the spy
  // must already be in place to capture it. Restore on teardown.
  beforeEach: () => {
    const spy = spyOn(console, "error");
    keyCollisionErrorSpy = spy;
    return () => {
      spy.mockRestore();
      keyCollisionErrorSpy = null;
    };
  },
  render: () => (
    <PricingTable
      data-testid="pricing-keycollision"
      // h2 tier names → valid heading outline for this isolated story.
      headingLevel="h2"
      tiers={[
        {
          id: "pro",
          name: "Prop Pro",
          price: "$0",
          period: "/mo",
          features: [{ label: "1 project" }],
          ctaLabel: "Get started",
        },
      ]}
    >
      <PricingTable.Tier
        key="pro"
        name="Compound Pro"
        price="$29"
        period="/mo"
        cta={<Button className="zs-pricing__cta-button">Upgrade</Button>}
      >
        <PricingTable.Feature>Unlimited projects</PricingTable.Feature>
      </PricingTable.Tier>
    </PricingTable>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Both surface tiers render — prop tier first, compound after.
    const headings = canvas.getAllByRole("heading");
    const names = headings.map((h) => h.textContent);
    await expect(names).toEqual(["Prop Pro", "Compound Pro"]);
    // No duplicate-key React warning was emitted during render. React's
    // warning text is "Encountered two children with the same key, `…`."
    const sawDuplicateKey = (keyCollisionErrorSpy?.mock.calls ?? []).some(
      (args) =>
        args.some(
          (arg) =>
            typeof arg === "string" &&
            arg.includes("two children with the same key"),
        ),
    );
    await expect(sawDuplicateKey).toBe(false);
  },
};

/* ─── 7. FalseTitleNoLabel — F3 regression ───────────────────────────────── */
/* A conditional `title={showTitle && "Pricing"}` yields `title={false}` when
 * the flag is off. `false` (and `""`) must be treated as "no title" — no empty
 * <h2> heading, and no dangling `aria-labelledby` pointing at it. */
export const FalseTitleNoLabel: Story = {
  name: "FalseTitleNoLabel (F3: false/empty title is not a heading)",
  parameters: {
    docs: {
      description: {
        story:
          "Regression for F3: `title={false}` (the common " +
          "`showTitle && \"…\"` idiom when the flag is off) must render NO " +
          "heading and leave NO `aria-labelledby` on the section.",
      },
    },
  },
  render: () => (
    <PricingTable
      data-testid="pricing-falsetitle"
      // The common conditional-title idiom collapsing to `false`.
      title={false as unknown as undefined}
      // h2 tier names → valid heading outline once the (empty) title heading
      // is gone, so post-fix the story is axe-clean and the test bites ONLY
      // on the F3 contract (no empty heading, no dangling aria-labelledby).
      headingLevel="h2"
      tiers={[
        {
          id: "free",
          name: "Free",
          price: "$0",
          period: "/mo",
          features: [{ label: "1 project" }],
          ctaLabel: "Get started",
        },
      ]}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("pricing-falsetitle");
    // No dangling aria-labelledby when the title is false/empty.
    await expect(root.hasAttribute("aria-labelledby")).toBe(false);
    // No empty title heading rendered.
    await expect(
      root.querySelector('[data-slot="pricing-table-title"]'),
    ).toBeNull();
  },
};
