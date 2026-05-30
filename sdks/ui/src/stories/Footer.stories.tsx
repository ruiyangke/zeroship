import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
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
  render: () => (
    <Footer
      data-testid="footer-default"
      brand="zeroship"
      description="Ship software without writing code. AI builds it; we host it."
      columns={threeColumns}
      copyright="© 2026 zeroship, Inc."
      actions={<SocialLinks />}
    />
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
