import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import { PageHeader } from "../layouts";
import { Button } from "../components";

const meta: Meta<typeof PageHeader> = {
  title: "Layouts/PageHeader",
  component: PageHeader,
  parameters: { layout: "fullscreen" },
};

export default meta;

type Story = StoryObj<typeof PageHeader>;

/* ─── 1. Full — breadcrumbs + title + description + actions ─────────────── */
export const Full: Story = {
  name: "Full (breadcrumbs + title + description + actions)",
  parameters: {
    docs: {
      description: {
        story:
          "The full page title band: a start-side text column " +
          "(`PageHeader.Text` → Breadcrumbs → Title → Description) and an " +
          "end-aligned `PageHeader.Actions` (a `Cluster`). The row wraps " +
          "on narrow widths so Actions drop below the text column. " +
          "Breadcrumbs render a `<nav aria-label=\"Breadcrumb\">` wrapping " +
          "an `<ol>`.",
      },
    },
  },
  render: () => (
    <PageHeader>
      <PageHeader.Text>
        <PageHeader.Breadcrumbs
          items={[
            { label: "Home", href: "#" },
            { label: "Projects", href: "#" },
          ]}
        />
        <PageHeader.Title>Acme dashboard</PageHeader.Title>
        <PageHeader.Description>
          Overview of your workspace and recent activity.
        </PageHeader.Description>
      </PageHeader.Text>
      <PageHeader.Actions>
        <Button variant="gray">Export</Button>
        <Button>New project</Button>
      </PageHeader.Actions>
    </PageHeader>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // The page heading is an <h1>.
    const heading = canvas.getByRole("heading", {
      level: 1,
      name: /acme dashboard/i,
    });
    await expect(heading.tagName).toBe("H1");
    // Breadcrumb landmark + ordered list.
    const crumbs = canvas.getByRole("navigation", { name: /breadcrumb/i });
    await expect(crumbs.tagName).toBe("NAV");
    await expect(crumbs.querySelector("ol")).not.toBeNull();
    // Actions composes Cluster and carries our band class; it is
    // justified to the end so actions hug the trailing edge. The composed
    // part now owns the semantic data-slot vocabulary (pre-fix it read
    // "cluster").
    const actions = canvasElement.querySelector('[data-slot~="page-header-actions"]');
    await expect(actions).not.toBeNull();
    await expect(actions!.getAttribute("data-slot")).toMatch(
      /(?:^|\s)page-header-actions(?:\s|$)/,
    );
    await expect(getComputedStyle(actions as Element).justifyContent).toBe(
      "flex-end",
    );
    // The text column stacks: PageHeader.Text retains both its semantic
    // token and Stack's token, and Title renders above Description.
    const text = canvasElement.querySelector(
      '[data-slot~="page-header-text"]',
    ) as HTMLElement;
    await expect(text).not.toBeNull();
    await expect(getComputedStyle(text).flexDirection).toBe("column");
    const titleRect = heading.getBoundingClientRect();
    const desc = canvasElement.querySelector(
      "[data-slot~=page-header-description]",
    ) as HTMLElement;
    await expect(desc).not.toBeNull();
    const descRect = desc.getBoundingClientRect();
    await expect(titleRect.bottom).toBeLessThanOrEqual(descRect.top);
  },
};

/* ─── 2. Minimal — title only ───────────────────────────────────────────── */
export const TitleOnly: Story = {
  name: "Minimal (title only)",
  parameters: {
    docs: {
      description: {
        story:
          "The minimal band: just a `PageHeader.Title` (an `<h1>`). No " +
          "breadcrumbs, description, or actions.",
      },
    },
  },
  render: () => (
    <PageHeader>
      <PageHeader.Title>Settings</PageHeader.Title>
    </PageHeader>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const heading = canvas.getByRole("heading", { level: 1, name: /settings/i });
    await expect(heading.tagName).toBe("H1");
  },
};

/* ─── 3. Relevelled title — asChild <h2> ────────────────────────────────── */
export const RelevelledTitle: Story = {
  name: "Relevelled title (asChild h2)",
  parameters: {
    docs: {
      description: {
        story:
          "`PageHeader.Title asChild` relevels the heading (here to `<h2>`) " +
          "so it matches the surrounding document outline when the band is " +
          "not the top of the page. The visual style is unchanged.",
      },
    },
  },
  render: () => (
    <PageHeader>
      <PageHeader.Text>
        <PageHeader.Title asChild>
          <h2>Section heading</h2>
        </PageHeader.Title>
        <PageHeader.Description>Relevelled via asChild.</PageHeader.Description>
      </PageHeader.Text>
      <PageHeader.Actions>
        <Button>Action</Button>
      </PageHeader.Actions>
    </PageHeader>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const heading = canvas.getByRole("heading", {
      level: 2,
      name: /section heading/i,
    });
    await expect(heading.tagName).toBe("H2");
    await expect(heading).toHaveAttribute(
      "data-slot",
      expect.stringMatching(/(?:^|\s)page-header-title(?:\s|$)/),
    );
  },
};

/* ─── 4. asChild root ───────────────────────────────────────────────────── */
export const AsChildRoot: Story = {
  name: "asChild root (render-as section)",
  parameters: {
    docs: {
      description: {
        story:
          "The PageHeader root accepts `asChild`: render the band as the " +
          "single child element (here a `<section>`) via `Slot`, with the " +
          "parts nested inside — no wrapper `<div>`. PageHeader is a plain " +
          "container, so the children keep their place.",
      },
    },
  },
  render: () => (
    <PageHeader asChild>
      <section aria-label="Page header">
        <PageHeader.Title>Reports</PageHeader.Title>
      </section>
    </PageHeader>
  ),
  play: async ({ canvasElement }) => {
    const root = canvasElement.querySelector(
      '[data-slot~="page-header"]',
    ) as HTMLElement;
    await expect(root.tagName).toBe("SECTION");
    await expect(root).toHaveAttribute(
      "data-slot",
      expect.stringMatching(/(?:^|\s)page-header(?:\s|$)/),
    );
  },
};
