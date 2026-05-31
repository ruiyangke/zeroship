import { forwardRef, type ComponentPropsWithoutRef } from "react";
import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, within } from "@storybook/test";
import { Breadcrumbs } from "../components";

const meta: Meta<typeof Breadcrumbs> = {
  title: "Components/Breadcrumbs",
  component: Breadcrumbs,
  parameters: { layout: "fullscreen" },
};

export default meta;

type Story = StoryObj<typeof Breadcrumbs>;

/* ─── 1. Basic — ergonomic items, last is current ─────────────────────── */
export const Basic: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "The ergonomic `items` API. Each entry is " +
          "`{ label, href?, current? }`. Crumbs with `href` render as " +
          "links; the last crumb (no flag) is treated as the current page " +
          "— `<span aria-current=\"page\">`, not a link. Separators are " +
          "auto-inserted between crumbs (aria-hidden chevrons).",
      },
    },
  },
  render: () => (
    <Breadcrumbs
      data-testid="breadcrumbs-basic"
      items={[
        { label: "Home", href: "#home" },
        { label: "Projects", href: "#projects" },
        { label: "Acme", href: "#acme" },
        { label: "Settings" },
      ]}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Landmark + ordered list.
    const nav = canvas.getByRole("navigation", { name: /breadcrumb/i });
    await expect(nav.tagName).toBe("NAV");
    await expect(nav.querySelector("ol")).not.toBeNull();
    // The last crumb is the current page: aria-current="page" and NOT a
    // link.
    const current = canvas.getByText("Settings");
    await expect(current.getAttribute("aria-current")).toBe("page");
    await expect(current.tagName).not.toBe("A");
    await expect(current.closest("a")).toBeNull();
    // Upstream crumbs are links.
    const home = canvas.getByRole("link", { name: "Home" });
    await expect(home.tagName).toBe("A");
    // Separators are aria-hidden / presentation (skipped by AT).
    const seps = nav.querySelectorAll('[data-slot="breadcrumbs-separator"]');
    await expect(seps.length).toBe(3);
    seps.forEach((s) => {
      expect(s.getAttribute("aria-hidden")).toBe("true");
      expect(s.getAttribute("role")).toBe("presentation");
    });
  },
};

/* ─── 2. Compound parts ───────────────────────────────────────────────── */
export const Compound: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Full-control compound form: `Breadcrumbs.Item` / `.Link` / " +
          "`.Page`. Separators are AUTO-INSERTED between adjacent " +
          "`Breadcrumbs.Item`s — consumers do not hand-roll them. The " +
          "current page is `Breadcrumbs.Page` (`aria-current=\"page\"`).",
      },
    },
  },
  render: () => (
    <Breadcrumbs data-testid="breadcrumbs-compound">
      <Breadcrumbs.Item>
        <Breadcrumbs.Link href="#home">Home</Breadcrumbs.Link>
      </Breadcrumbs.Item>
      <Breadcrumbs.Item>
        <Breadcrumbs.Link href="#library">Library</Breadcrumbs.Link>
      </Breadcrumbs.Item>
      <Breadcrumbs.Item>
        <Breadcrumbs.Page>Data</Breadcrumbs.Page>
      </Breadcrumbs.Item>
    </Breadcrumbs>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const nav = canvas.getByRole("navigation", { name: /breadcrumb/i });
    // Two auto-inserted separators between three items.
    const seps = nav.querySelectorAll('[data-slot="breadcrumbs-separator"]');
    await expect(seps.length).toBe(2);
    // The current page is a span with aria-current, not a link.
    const current = canvas.getByText("Data");
    await expect(current.getAttribute("aria-current")).toBe("page");
    await expect(current.closest("a")).toBeNull();
  },
};

/* ─── 2b. Compound parts wrapped in a Fragment ────────────────────────────
 * Regression for the auto-separator Fragment-flatten fix: the common
 * map/conditional shape yields the Items inside a single `<>…</>`. The walk
 * must flatten the Fragment FIRST, so separators still land between the
 * adjacent Items (count = items - 1). Pre-fix this rendered 0 separators
 * because `Children.toArray` does not flatten Fragments.
 */
export const CompoundFragment: Story = {
  name: "Compound (Fragment children)",
  parameters: {
    docs: {
      description: {
        story:
          "Compound parts wrapped in a `<>…</>` Fragment (the common " +
          "`{items.map(...)}` shape). Auto-separators must flatten the " +
          "Fragment before walking the Items, so separators still land " +
          "between adjacent crumbs (count = items - 1).",
      },
    },
  },
  render: () => (
    <Breadcrumbs data-testid="breadcrumbs-fragment">
      <>
        <Breadcrumbs.Item>
          <Breadcrumbs.Link href="#home">Home</Breadcrumbs.Link>
        </Breadcrumbs.Item>
        <Breadcrumbs.Item>
          <Breadcrumbs.Link href="#library">Library</Breadcrumbs.Link>
        </Breadcrumbs.Item>
        <Breadcrumbs.Item>
          <Breadcrumbs.Page>Data</Breadcrumbs.Page>
        </Breadcrumbs.Item>
      </>
    </Breadcrumbs>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const nav = canvas.getByRole("navigation", { name: /breadcrumb/i });
    // Three Items inside a Fragment → two auto-inserted separators.
    const seps = nav.querySelectorAll('[data-slot="breadcrumbs-separator"]');
    await expect(seps.length).toBe(2);
    // All three crumb labels render in order inside the <ol>.
    const items = nav.querySelectorAll('[data-slot="breadcrumbs-item"]');
    await expect(items.length).toBe(3);
    const current = canvas.getByText("Data");
    await expect(current.getAttribute("aria-current")).toBe("page");
  },
};

/* ─── 3. Custom separator ─────────────────────────────────────────────── */
export const CustomSeparator: Story = {
  name: "Custom separator",
  parameters: {
    docs: {
      description: {
        story:
          "`separator` overrides the default chevron. Here a slash. The " +
          "glyph stays aria-hidden so AT only hears the crumb labels.",
      },
    },
  },
  render: () => (
    <Breadcrumbs
      data-testid="breadcrumbs-slash"
      separator="/"
      items={[
        { label: "Home", href: "#home" },
        { label: "Docs", href: "#docs" },
        { label: "API", current: true },
      ]}
    />
  ),
  // Regression guard: a separator is a flex item; without `flex: 0 0 auto`
  // a custom Icon separator can squish when the trail wraps. Assert it
  // never shrinks (flex-shrink: 0). Fails pre-fix.
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const nav = canvas.getByTestId("breadcrumbs-slash");
    const separator = nav.querySelector<HTMLElement>(
      ".zs-breadcrumbs__separator",
    );
    await expect(separator).not.toBeNull();
    if (!separator) return;
    await expect(getComputedStyle(separator).flexShrink).toBe("0");
  },
};

/* ─── 4. Collapsed (maxItems) — inline expand ─────────────────────────── */
export const Collapsed: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "`maxItems` collapses the middle of a long trail into a single " +
          "ellipsis crumb: a real `<button aria-expanded>` with an " +
          "accessible label. The first crumb and the last `(maxItems - 1)` " +
          "stay visible. Activating the button expands the FULL trail " +
          "INLINE (no portal/Menu). play(): the ellipsis is present + " +
          "`aria-expanded=\"false\"`; click → hidden crumbs appear + " +
          "`aria-expanded=\"true\"`.",
      },
    },
  },
  render: () => (
    <Breadcrumbs
      data-testid="breadcrumbs-collapsed"
      maxItems={4}
      items={[
        { label: "Home", href: "#home" },
        { label: "Workspace", href: "#workspace" },
        { label: "Team", href: "#team" },
        { label: "Projects", href: "#projects" },
        { label: "Acme", href: "#acme" },
        { label: "Releases", href: "#releases" },
        { label: "v2.0", current: true },
      ]}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Hidden crumbs (Workspace/Team/Projects) are NOT in the DOM while
    // collapsed.
    await expect(canvas.queryByText("Workspace")).toBeNull();
    await expect(canvas.queryByText("Team")).toBeNull();
    // The ellipsis button is present + collapsed.
    const ellipsis = canvas.getByTestId("breadcrumbs-ellipsis");
    await expect(ellipsis.tagName).toBe("BUTTON");
    await expect(ellipsis.getAttribute("aria-expanded")).toBe("false");
    await expect(ellipsis.getAttribute("aria-label")).toMatch(
      /show 3 hidden breadcrumbs/i,
    );
    // First + last group still visible.
    await expect(canvas.getByText("Home")).not.toBeNull();
    await expect(canvas.getByText("v2.0")).not.toBeNull();

    // Click the ellipsis → the full trail expands inline.
    await userEvent.click(ellipsis);
    await expect(canvas.getByText("Workspace")).not.toBeNull();
    await expect(canvas.getByText("Team")).not.toBeNull();
    await expect(canvas.getByText("Projects")).not.toBeNull();
    // The ellipsis button is gone once expanded.
    await expect(canvas.queryByTestId("breadcrumbs-ellipsis")).toBeNull();
  },
};

/* ─── 4b. Collapse that hides nothing renders NO ellipsis ─────────────────
 * Regression for the hiddenCount>0 guard: with maxItems=1 on a 2-crumb
 * trail, head(1) + tail(1) already cover both crumbs — hiddenCount is 0, so
 * no ellipsis must render. Pre-fix this rendered a bogus
 * `aria-label="Show 0 hidden breadcrumbs"` button.
 */
export const CollapseHidesNothing: Story = {
  name: "Collapse (hides nothing → no ellipsis)",
  parameters: {
    docs: {
      description: {
        story:
          "A `maxItems` that would hide no crumbs (head + tail already " +
          "cover the whole trail) must render the FULL trail with NO " +
          "ellipsis button — never an ellipsis that hides nothing.",
      },
    },
  },
  render: () => (
    <Breadcrumbs
      data-testid="breadcrumbs-noop-collapse"
      maxItems={1}
      items={[
        { label: "Home", href: "#home" },
        { label: "Settings", current: true },
      ]}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // No ellipsis button — nothing was hidden.
    await expect(canvas.queryByTestId("breadcrumbs-ellipsis")).toBeNull();
    // Both crumbs are present.
    await expect(canvas.getByText("Home")).not.toBeNull();
    const current = canvas.getByText("Settings");
    await expect(current.getAttribute("aria-current")).toBe("page");
  },
};

/* ─── 4c. Current crumb stays visible when it falls in the hidden span ─────
 * Regression for "current must stay visible when collapsed": the current
 * crumb is flagged in the middle of a long trail. Even while collapsed, the
 * crumb carrying aria-current="page" must be rendered (the tail is extended
 * back to it).
 */
export const CollapsedCurrentInMiddle: Story = {
  name: "Collapsed (current in hidden middle stays visible)",
  parameters: {
    docs: {
      description: {
        story:
          "When the flagged current crumb falls in the collapsed middle, " +
          "the trail extends the visible tail back to it so " +
          "`aria-current=\"page\"` is always rendered while collapsed.",
      },
    },
  },
  render: () => (
    <Breadcrumbs
      data-testid="breadcrumbs-collapsed-current"
      maxItems={3}
      items={[
        { label: "Home", href: "#home" },
        { label: "Workspace", href: "#workspace" },
        { label: "Team", current: true },
        { label: "Projects", href: "#projects" },
        { label: "Acme", href: "#acme" },
        { label: "Releases", href: "#releases" },
      ]}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // The trail is collapsed (an ellipsis is present).
    await expect(canvas.getByTestId("breadcrumbs-ellipsis")).not.toBeNull();
    // The flagged current crumb ("Team") is in the hidden middle but stays
    // visible and carries aria-current.
    const current = canvas.getByText("Team");
    await expect(current.getAttribute("aria-current")).toBe("page");
    await expect(current.closest("a")).toBeNull();
    // First crumb still visible.
    await expect(canvas.getByText("Home")).not.toBeNull();
  },
};

/* ─── 5. Router link (asChild) ────────────────────────────────────────── */

// A stand-in router <Link>: a custom anchor-like element. asChild routes
// Breadcrumbs.Link through Slot so the styling/semantics land on the
// consumer's element while the tag/href survive.
const RouterLink = forwardRef<
  HTMLAnchorElement,
  ComponentPropsWithoutRef<"a"> & { to: string }
>(function RouterLink({ to, children, ...rest }, ref) {
  return (
    <a {...rest} ref={ref} href={to} data-router-link="">
      {children}
    </a>
  );
});

export const RouterLinkStory: Story = {
  name: "Router link (asChild)",
  parameters: {
    docs: {
      description: {
        story:
          "`Breadcrumbs.Link asChild` renders the consumer's element (here " +
          "a router-style `<Link>`) through `Slot`, so client navigation " +
          "works while the breadcrumb styling/semantics are preserved. " +
          "play(): the custom tag + href survive the merge.",
      },
    },
  },
  render: () => (
    <Breadcrumbs data-testid="breadcrumbs-router">
      <Breadcrumbs.Item>
        <Breadcrumbs.Link asChild>
          <RouterLink to="/home">Home</RouterLink>
        </Breadcrumbs.Link>
      </Breadcrumbs.Item>
      <Breadcrumbs.Item>
        <Breadcrumbs.Link asChild>
          <RouterLink to="/reports">Reports</RouterLink>
        </Breadcrumbs.Link>
      </Breadcrumbs.Item>
      <Breadcrumbs.Item>
        <Breadcrumbs.Page>Q3</Breadcrumbs.Page>
      </Breadcrumbs.Item>
    </Breadcrumbs>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const home = canvas.getByRole("link", { name: "Home" });
    // The asChild element survives: it carries the router marker AND the
    // composed href AND the breadcrumb link class.
    await expect(home.getAttribute("data-router-link")).toBe("");
    await expect(home.getAttribute("href")).toBe("/home");
    await expect(home).toHaveClass("zs-breadcrumbs__link");
    await expect(home.getAttribute("data-slot")).toBe("breadcrumbs-link");
  },
};

/* ─── MultipleCurrent (dev-warn guard) ──────────────────────────────────
 * Regression for the dev-only warning when MORE THAN ONE crumb is flagged
 * `current`. `findIndex` honors only the FIRST flagged crumb; later flagged
 * crumbs silently render as links. The block emits a `console.warn` saying
 * only the first is honored. The dev-only console.warn that names this case
 * is compiled out of the production Storybook build, so we cannot assert it
 * via the test-runner gate; instead this story is a behavioral guard for the
 * observable "first current wins" contract. Tagged `!autodocs` so the
 * intentionally-ambiguous state stays out of docs. */
export const MultipleCurrent: Story = {
  tags: ["!autodocs"],
  parameters: {
    docs: {
      description: {
        story:
          "Regression: two crumbs flagged `current`. `findIndex` honors only " +
          "the first; the rest render as plain crumbs. Exactly one crumb " +
          "carries `aria-current=\"page\"` (the first flagged). A dev-only " +
          "`console.warn` (not observable in the prod build) names the case.",
      },
    },
  },
  render: () => (
    <Breadcrumbs
      data-testid="breadcrumbs-multiple-current"
      items={[
        { label: "Home", href: "#home" },
        { label: "Projects", href: "#projects", current: true },
        { label: "Acme", current: true },
      ]}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Behavioral guard for "first current wins": exactly one crumb carries
    // aria-current="page", and it is the FIRST flagged one ("Projects").
    const nav = canvas.getByRole("navigation", { name: /breadcrumb/i });
    const current = nav.querySelectorAll('[aria-current="page"]');
    await expect(current).toHaveLength(1);
    await expect(current[0]).toHaveTextContent("Projects");
    // The later flagged crumb still renders, but is not marked current.
    await expect(canvas.getByText("Acme")).toBeInTheDocument();
  },
};
