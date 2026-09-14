import type { Meta, StoryObj } from "@storybook/react";
import { expect, fn, userEvent, within } from "@storybook/test";
import { ChevronRight, MoreHorizontal } from "lucide-react";
import { ListView, type ListViewItem } from "../blocks";
import { EmptyState } from "../blocks";
import { Avatar } from "../components/Avatar";
import { Badge } from "../components/Badge";
import { Button } from "../components/Button";
import { Icon } from "../components/Icon";

const meta: Meta<typeof ListView> = {
  title: "Blocks/ListView",
  component: ListView,
  parameters: { layout: "fullscreen" },
};

export default meta;

type Story = StoryObj<typeof ListView>;

/* A constrained shell so the fullscreen stories read as a real list
   panel rather than a full-bleed band. */
function Shell({ children }: { children: React.ReactNode }) {
  return (
    <div style={{ maxInlineSize: "32rem", margin: "var(--zs-space-6) auto" }}>
      {children}
    </div>
  );
}

const people: Array<{ id: string; name: string; role: string; ago: string }> = [
  { id: "ada", name: "Ada Lovelace", role: "Analytical engine", ago: "2h" },
  { id: "alan", name: "Alan Turing", role: "Computation", ago: "5h" },
  { id: "grace", name: "Grace Hopper", role: "Compilers", ago: "1d" },
];

/* ─── 1. Basic — avatars + title + description + meta + chevron ────────── */
export const Basic: Story = {
  name: "Basic (avatar · title/description · meta · chevron)",
  parameters: {
    docs: {
      description: {
        story:
          "The ergonomic `items` surface: each row is an Avatar leading, a " +
          "title + description content column, a `meta` timestamp, and a " +
          "decorative chevron Icon in `trailing`. No row is interactive, so " +
          "the main area renders as a plain `<div>`.",
      },
    },
  },
  render: () => (
    <Shell>
      <ListView
        data-testid="lv-basic"
        items={people.map((p) => ({
          id: p.id,
          leading: (
            <Avatar size="sm" fallback={p.name.slice(0, 2).toUpperCase()} />
          ),
          title: p.name,
          description: p.role,
          meta: p.ago,
          trailing: <Icon as={ChevronRight} size="sm" />,
        }))}
      />
    </Shell>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const list = canvas.getByTestId("lv-basic");
    // List semantics: <ul> of <li>.
    await expect(list.tagName).toBe("UL");
    const items = within(list).getAllByRole("listitem");
    await expect(items).toHaveLength(3);
    // No interactive rows here — no links, no buttons.
    await expect(within(list).queryByRole("link")).toBeNull();
    await expect(within(list).queryByRole("button")).toBeNull();
  },
};

/* ─── 2. Interactive — href rows (link wraps content only) ─────────────── */
export const Interactive: Story = {
  name: "Interactive (href rows — trailing outside the link)",
  parameters: {
    docs: {
      description: {
        story:
          "Whole-row links via `href`. The row link (`<a>`) wraps ONLY the " +
          "main area (leading + title + description); the trailing chevron " +
          "lives in a sibling `aside`, OUTSIDE the link — so no interactive " +
          "is nested inside another. The play() asserts the row's content " +
          "is an `<a href>` and the trailing control is not a descendant of " +
          "that link.",
      },
    },
  },
  render: () => (
    <Shell>
      <ListView
        data-testid="lv-interactive"
        items={people.map((p) => ({
          id: p.id,
          leading: (
            <Avatar size="sm" fallback={p.name.slice(0, 2).toUpperCase()} />
          ),
          title: p.name,
          description: p.role,
          meta: p.ago,
          trailing: <Icon as={ChevronRight} size="sm" />,
          href: `#/people/${p.id}`,
        }))}
      />
    </Shell>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const list = canvas.getByTestId("lv-interactive");
    await expect(list.tagName).toBe("UL");

    // The row link is a real <a href> wrapping the title.
    const links = within(list).getAllByRole("link");
    await expect(links).toHaveLength(3);
    const firstLink = links[0];
    await expect(firstLink).toHaveAttribute("href", "#/people/ada");
    await expect(firstLink).toHaveTextContent("Ada Lovelace");

    // The trailing control must be OUTSIDE the link (not a descendant).
    const li = firstLink.closest('[data-slot="list-view-item"]');
    await expect(li).not.toBeNull();
    const trailing = li!.querySelector('[data-slot="list-view-trailing"]');
    await expect(trailing).not.toBeNull();
    await expect(firstLink.contains(trailing)).toBe(false);
    // And it is a sibling under the same <li>, not under the link.
    await expect(trailing!.closest("a")).toBeNull();
  },
};

/* ─── 3. WithActions — trailing Button + kebab (fires without row) ─────── */
export const WithActions: Story = {
  name: "WithActions (trailing controls fire without triggering the row)",
  parameters: {
    docs: {
      description: {
        story:
          "Rows are interactive via `onClick` (the main area is a " +
          "`<button>`); the trailing slot carries its OWN controls — a " +
          "primary Button and an icon-only kebab. Because trailing sits " +
          "outside the row button, clicking a trailing control fires that " +
          "control's handler WITHOUT triggering the row's `onClick`.",
      },
    },
  },
  args: {
    // Per-row handlers are wired in render via closures over these fns so
    // play() can assert which fired.
  },
  render: () => {
    const onRow = fn();
    const onAction = fn();
    const onMenu = fn();
    // Stash on window so play() can read the same fn instances.
    (window as unknown as Record<string, unknown>).__lvHandlers = {
      onRow,
      onAction,
      onMenu,
    };
    const items: ListViewItem[] = people.map((p) => ({
      id: p.id,
      leading: <Avatar size="sm" fallback={p.name.slice(0, 2).toUpperCase()} />,
      title: p.name,
      description: p.role,
      onClick: () => onRow(p.id),
      trailing: (
        <>
          <Button variant="tinted" size="small" onClick={() => onAction(p.id)}>
            Invite
          </Button>
          <Button
            variant="plain"
            size="small"
            aria-label={`More actions for ${p.name}`}
            onClick={() => onMenu(p.id)}
          >
            <Icon as={MoreHorizontal} size="sm" />
          </Button>
        </>
      ),
    }));
    return (
      <Shell>
        <ListView data-testid="lv-actions" items={items} />
      </Shell>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const list = canvas.getByTestId("lv-actions");
    const handlers = (window as unknown as Record<string, unknown>)
      .__lvHandlers as {
      onRow: ReturnType<typeof fn>;
      onAction: ReturnType<typeof fn>;
      onMenu: ReturnType<typeof fn>;
    };

    // Click the trailing "Invite" action on the first row.
    const invite = within(list).getAllByRole("button", { name: "Invite" })[0];
    // The action button must NOT be inside the row's main button.
    const rowButton = invite
      .closest('[data-slot="list-view-item"]')!
      .querySelector('[data-slot="list-view-main"]');
    await expect(rowButton!.contains(invite)).toBe(false);

    await userEvent.click(invite);
    await expect(handlers.onAction).toHaveBeenCalledWith("ada");
    // Crucially the row handler did NOT fire from the trailing click.
    await expect(handlers.onRow).not.toHaveBeenCalled();

    // Clicking the row's main button fires the row handler.
    await userEvent.click(rowButton as HTMLElement);
    await expect(handlers.onRow).toHaveBeenCalledWith("ada");
  },
};

/* ─── 4. Compound — the parts form ─────────────────────────────────────── */
export const Compound: Story = {
  name: "Compound (ListView.Item / .Leading / .Content / .Trailing)",
  parameters: {
    docs: {
      description: {
        story:
          "The compound surface for full control over composition. The " +
          "consumer assembles `ListView.Item` rows from `.Leading`, " +
          "`.Content` (with `.Title` / `.Description`), `.Meta`, and " +
          "`.Trailing`. ListView does not auto-wrap interactives here — " +
          "compose your own `<a>`/`<button>` inside `.Content` if needed.",
      },
    },
  },
  render: () => (
    <Shell>
      <ListView data-testid="lv-compound">
        <ListView.Item>
          <ListView.Leading>
            <Avatar size="sm" fallback="AL" />
          </ListView.Leading>
          <ListView.Content>
            <ListView.Title>Ada Lovelace</ListView.Title>
            <ListView.Description>Analytical engine</ListView.Description>
          </ListView.Content>
          <ListView.Trailing>
            <Badge intent="success" variant="soft">
              Active
            </Badge>
          </ListView.Trailing>
        </ListView.Item>
        <ListView.Item>
          <ListView.Leading>
            <Avatar size="sm" fallback="AT" />
          </ListView.Leading>
          <ListView.Content>
            <ListView.Title>Alan Turing</ListView.Title>
            <ListView.Description>Computation</ListView.Description>
          </ListView.Content>
          <ListView.Trailing>
            <Badge intent="neutral" variant="soft">
              Away
            </Badge>
          </ListView.Trailing>
        </ListView.Item>
      </ListView>
    </Shell>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const list = canvas.getByTestId("lv-compound");
    await expect(list.tagName).toBe("UL");
    const items = within(list).getAllByRole("listitem");
    await expect(items).toHaveLength(2);
    await expect(
      list.querySelector('[data-slot="list-view-title"]'),
    ).toHaveTextContent("Ada Lovelace");
    await expect(
      list.querySelector('[data-slot="list-view-trailing"]'),
    ).not.toBeNull();
  },
};

/* ─── 5. Plain — divided=false ─────────────────────────────────────────── */
export const Plain: Story = {
  name: "Plain (divided=false, compact)",
  parameters: {
    docs: {
      description: {
        story:
          "A flush, borderless list (`divided={false}`) at `compact` " +
          "density — tighter block padding, no hairlines between rows.",
      },
    },
  },
  render: () => (
    <Shell>
      <ListView
        data-testid="lv-plain"
        divided={false}
        density="compact"
        items={people.map((p) => ({
          id: p.id,
          title: p.name,
          description: p.role,
          meta: p.ago,
        }))}
      />
    </Shell>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const list = canvas.getByTestId("lv-plain");
    await expect(list).not.toHaveAttribute("data-divided");
    await expect(list).toHaveAttribute("data-density", "compact");
  },
};

/* ─── 6. Empty — compose EmptyState ────────────────────────────────────── */
export const Empty: Story = {
  name: "Empty (compose EmptyState)",
  parameters: {
    docs: {
      description: {
        story:
          "When there is nothing to list, render an `EmptyState` instead of " +
          "an empty `<ul>`. ListView itself is presentational; the " +
          "empty-collection decision belongs to the consumer.",
      },
    },
  },
  render: () => (
    <Shell>
      <EmptyState
        data-testid="lv-empty"
        title="No members yet"
        description="Invite teammates and they'll show up here."
        action={<Button>Invite people</Button>}
      />
    </Shell>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByTestId("lv-empty")).toBeInTheDocument();
    await expect(canvas.getByText("No members yet")).toBeInTheDocument();
  },
};

/* ─── DuplicateIds (dev-warn guard) ─────────────────────────────────────
 * Regression for the dev-only warning when two `items` share an `id`.
 * `item.id` is the documented stable React key; duplicates silently break
 * row reconciliation. The block emits a dev-only `console.warn` naming the
 * offending id — that warn is compiled out of the production Storybook build,
 * so it cannot be asserted via the test-runner gate; this story is a smoke
 * guard that duplicate ids still render without crashing and stay axe-clean.
 * Tagged `!autodocs` so the intentionally-broken state stays out of docs. */
export const DuplicateIds: Story = {
  tags: ["!autodocs"],
  parameters: {
    docs: {
      description: {
        story:
          "Two `items` share `id=\"dup\"`. Ids are the stable React keys and " +
          "must be unique; the block emits a dev-only `console.warn` (not " +
          "observable in the prod build) naming the offending id. The list " +
          "still renders both rows.",
      },
    },
  },
  render: () => (
    <div className="zs-story-cell" style={{ padding: "1rem", maxInlineSize: "32rem" }}>
      <ListView
        items={[
          { id: "dup", title: "Ada Lovelace" },
          { id: "dup", title: "Alan Turing" },
        ]}
      />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Both rows still render despite the duplicate id (the warn is advisory,
    // not fatal). Smoke + axe coverage of the degenerate input.
    await expect(canvas.getByText("Ada Lovelace")).toBeInTheDocument();
    await expect(canvas.getByText("Alan Turing")).toBeInTheDocument();
  },
};
