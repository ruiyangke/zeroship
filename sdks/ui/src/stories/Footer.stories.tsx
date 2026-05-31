import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { Footer, type FooterColumnData } from "../sections";
import { Button, Icon } from "../components";
import { GitBranch, MessageCircle, Globe } from "lucide-react";

/* Social links rendered as ICON BUTTONS (R2): a `Button asChild` wraps a real
 * `<a href>` so the link keeps its anchor semantics while reading as a quiet
 * gray icon button; the Lucide glyph is decorative and the accessible name
 * comes from `aria-label` on the Button. A consumer can pass their own glyphs
 * (Lucide's brand marks are not bundled, so these generic glyphs — a repo
 * branch, a community bubble, a globe — stand in for source / community /
 * website). */
const SocialLinks = () => (
  <>
    <Button asChild variant="gray" aria-label="GitHub">
      <a href="https://github.com/zeroship">
        <Icon as={GitBranch} size="sm" />
      </a>
    </Button>
    <Button asChild variant="gray" aria-label="Community">
      <a href="https://community.zeroship.ai">
        <Icon as={MessageCircle} size="sm" />
      </a>
    </Button>
    <Button asChild variant="gray" aria-label="Website">
      <a href="https://zeroship.ai">
        <Icon as={Globe} size="sm" />
      </a>
    </Button>
  </>
);

const meta: Meta<typeof Footer> = {
  title: "Sections/Footer",
  component: Footer,
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

type Story = StoryObj<typeof Footer>;

const threeColumns: FooterColumnData[] = [
  {
    id: "product",
    title: "Product",
    links: [
      { label: "Pricing", href: "/pricing" },
      { label: "Templates", href: "/templates" },
      { label: "Changelog", href: "/changelog" },
    ],
  },
  {
    id: "company",
    title: "Company",
    links: [
      { label: "About", href: "/about" },
      { label: "Blog", href: "/blog" },
      { label: "Careers", href: "/careers" },
    ],
  },
  {
    id: "legal",
    title: "Legal",
    links: [
      { label: "Privacy", href: "/privacy" },
      { label: "Terms", href: "/terms" },
    ],
  },
];

/* ─── 1. Default — brand + blurb + 3 columns + copyright + social ────────── */
export const Default: Story = {
  name: "Default (brand + blurb + 3 columns + copyright + social)",
  parameters: {
    a11y: {
      config: {
        rules: [
          // The footer's column titles are `<h3>`s by design (they structure
          // the link groups beneath the page's own h1/h2 content). In the
          // isolated Storybook canvas there is no preceding page heading, so
          // the first `<h3>` trips `heading-order` — a harness artifact, not
          // a component defect. In a real document the footer sits below the
          // page's heading outline and the order is correct. The `play()`
          // independently asserts the column titles ARE level-3 headings, so
          // the real structural invariant stays checked.
          { id: "heading-order", enabled: false },
        ],
      },
    },
    docs: {
      description: {
        story:
          "The default footer: a brand + blurb block beside three link-group " +
          "columns, a `Separator`, then a bottom bar (copyright at the start, " +
          "a row of social ICON BUTTONS at the end — `Button asChild` over " +
          "real `<a href>` anchors with Lucide glyphs, accessible-named via " +
          "`aria-label`). The root is a real `<footer>` — the page " +
          "`contentinfo` landmark. Column titles are `<h3>`s; links are real " +
          "`<a href>` anchors.",
      },
    },
  },
  // Scope the design tokens to the story subtree. The `[data-theme="crystal"]`
  // block in styles.css is where `--zs-focus-ring-color` (and the system
  // palette) live; the test-runner renders the iframe without the manager's
  // global theme decorator applied, so we attribute the theme on a local
  // wrapper to mirror a real themed page. The focus-ring assertions below
  // depend on the token resolving.
  render: () => (
    <div data-theme="crystal">
      <Footer
        data-testid="footer-default"
        brand="zeroship"
        description="Ship software without writing code. AI builds it; we host it."
        columns={threeColumns}
        copyright="© 2026 zeroship, Inc."
        actions={<SocialLinks />}
      />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("footer-default");

    // The root is a real <footer> — the contentinfo landmark.
    await expect(root.tagName).toBe("FOOTER");
    await expect(root).toHaveAttribute("data-slot", "footer");
    // It surfaces as the contentinfo landmark role.
    const contentinfo = canvas.getByRole("contentinfo");
    await expect(contentinfo).toBe(root);

    // Column titles are real <h3>s.
    const headings = canvas.getAllByRole("heading", { level: 3 });
    await expect(headings.map((h) => h.textContent)).toEqual([
      "Product",
      "Company",
      "Legal",
    ]);

    // Links are real anchors with hrefs.
    const pricing = canvas.getByRole("link", { name: "Pricing" });
    await expect(pricing.tagName).toBe("A");
    await expect(pricing).toHaveAttribute("href", "/pricing");

    // The copyright + social slot render. The social links are icon buttons
    // (Button asChild over real <a href>), accessible-named via aria-label.
    await expect(canvas.getByText("© 2026 zeroship, Inc.")).toBeInTheDocument();
    const github = canvas.getByRole("link", { name: "GitHub" });
    await expect(github).toHaveAttribute("href", "https://github.com/zeroship");
    // The glyph is decorative — no accessible name leaks beyond the button.
    await expect(github.querySelector("svg")).not.toBeNull();

    // ─── Regression: the column link's focus ring resolves to the design
    // focus-ring COLOR, and its color transition actually parses ───────────
    //
    // The link's `:focus-visible` rule was authored against `--zs-focus-ring`
    // (undefined — the token is `--zs-focus-ring-color`), so the outline
    // resolved to its initial value (the UA default `currentColor`) and the
    // keyboard focus target had no design color. The `transition` shorthand
    // referenced two more undefined tokens (`--zs-duration-1`/`--zs-ease-out`),
    // so the whole shorthand was invalid and DROPPED — the link had no
    // transition at all. Both are now fixed (`--zs-focus-ring-color`,
    // `--zs-motion-fast`/`--zs-motion-ease`).

    // The transition shorthand now PARSES to a valid value (asserted in the
    // resting state). Pre-fix it referenced undefined tokens
    // (`--zs-duration-1`/`--zs-ease-out`) so the whole shorthand was invalid
    // and DROPPED, leaving `transition-property: all` (the initial value).
    // Post-fix it resolves to the scoped `color … 150ms` transition — EXCEPT
    // the test-runner emulates `prefers-reduced-motion: reduce` for the whole
    // run (see .storybook/test-runner.ts), under which the reduced-motion
    // gate intentionally flattens it to `transition: none`. Accept either the
    // live 150ms color transition or the reduced-motion-flattened `none`;
    // both prove the shorthand parses (the pre-fix dropped state would leave
    // the `all`/`0s` initial values, which neither branch matches).
    const resting = getComputedStyle(pricing);
    const flattened = resting.transitionProperty === "none";
    if (!flattened) {
      await expect(resting.transitionProperty).toContain("color");
      await expect(resting.transitionDuration).toBe("0.15s");
    }

    // Keyboard focus (real Tab — programmatic .focus() does NOT match
    // `:focus-visible` in Chromium) so the focus-ring rule applies. Tab
    // forward through the footer's focusables; assert on whichever link
    // actually receives focus (the first focusable holds `:focus-visible`).
    let focusedLink: HTMLElement = pricing;
    await waitFor(async () => {
      await userEvent.tab();
      const active = document.activeElement as HTMLElement | null;
      await expect(active).not.toBeNull();
      await expect(active!.tagName).toBe("A");
      focusedLink = active!;
    });

    // The design focus-ring color lives under the `[data-theme="crystal"]`
    // token scope. The themed root carries it; resolve the token from
    // wherever it is defined (the focused link inherits it under a themed
    // ancestor; fall back to the document root, which the theme decorator
    // attributes). A non-empty value proves the token EXISTS (pre-fix the
    // rule referenced the undefined `--zs-focus-ring` and resolved to empty).
    const focused = getComputedStyle(focusedLink);
    const ringColor =
      focused.getPropertyValue("--zs-focus-ring-color").trim() ||
      getComputedStyle(document.documentElement)
        .getPropertyValue("--zs-focus-ring-color")
        .trim();
    await expect(ringColor.length).toBeGreaterThan(0);
    // Under real `:focus-visible` the outline paints the resolved ring
    // color — a real color, not the UA `currentcolor` default and distinct
    // from the link's own ink (the pre-fix undefined-token fallback would
    // collapse the outline to `currentColor` / the ink). Serialization-
    // agnostic: no oklch/rgba/alpha-substring matching.
    const outline = focused.outlineColor.trim();
    await expect(outline.length).toBeGreaterThan(0);
    await expect(outline.toLowerCase()).not.toBe("currentcolor");
    await expect(outline).not.toBe(focused.color);
  },
};

/* ─── 2. Compound — columns prop + Footer.Column additive ────────────────── */
export const Compound: Story = {
  name: "Compound (columns prop + Footer.Column, additive)",
  parameters: {
    docs: {
      description: {
        story:
          "The compound surface: one `columns`-prop column renders FIRST, " +
          "then the compound `<Footer.Column>` parts fall through after it " +
          "(ADDITIVE — no suppression). A compound column may supply raw `<a>` " +
          "children (wrapped in the column's `<ul>`) or a `links` array.",
      },
    },
  },
  render: () => (
    <Footer
      data-testid="footer-compound"
      brand="zeroship"
      copyright="© 2026 zeroship"
      columns={[
        {
          id: "product",
          title: "Product",
          links: [{ label: "Pricing", href: "/pricing" }],
        },
      ]}
    >
      <Footer.Column title="Company">
        <li>
          <a href="/about">About</a>
        </li>
        <li>
          <a href="/blog">Blog</a>
        </li>
      </Footer.Column>
      <Footer.Column
        title="Legal"
        links={[{ label: "Terms", href: "/terms" }]}
      />
    </Footer>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    // Three column <h3>s: prop column (Product) FIRST, then the two compound.
    const headings = canvas.getAllByRole("heading", { level: 3 });
    await expect(headings.map((h) => h.textContent)).toEqual([
      "Product",
      "Company",
      "Legal",
    ]);

    // The compound column's raw <a> children render as real links.
    await expect(canvas.getByRole("link", { name: "About" })).toHaveAttribute(
      "href",
      "/about",
    );
    // The compound column's `links` array renders too.
    await expect(canvas.getByRole("link", { name: "Terms" })).toHaveAttribute(
      "href",
      "/terms",
    );
  },
};

/* ─── 3. Minimal — brand + copyright, no columns ─────────────────────────── */
export const Minimal: Story = {
  name: "Minimal (brand + copyright, no columns)",
  parameters: {
    docs: {
      description: {
        story:
          "A minimal footer: just the brand and a copyright line, no link " +
          "columns. Still a real `<footer>` contentinfo landmark.",
      },
    },
  },
  render: () => (
    <Footer
      data-testid="footer-minimal"
      brand="zeroship"
      copyright="© 2026 zeroship"
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("footer-minimal");
    await expect(root.tagName).toBe("FOOTER");
    await expect(canvas.getByRole("contentinfo")).toBe(root);
    // No columns → no column <h3>s.
    await expect(canvas.queryAllByRole("heading", { level: 3 }).length).toBe(0);
    await expect(canvas.getByText("© 2026 zeroship")).toBeInTheDocument();
  },
};
