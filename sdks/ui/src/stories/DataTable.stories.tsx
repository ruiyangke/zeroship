import type { Meta, StoryObj } from "@storybook/react";
import { expect, fn, userEvent, within } from "@storybook/test";
import { useState } from "react";
import {
  DataTable,
  type DataTableColumn,
  type DataTableSort,
} from "../blocks";

/* ─── fixture ─────────────────────────────────────────────────────── */
interface Person {
  id: string;
  name: string;
  role: string;
  commits: number;
}

const PEOPLE: Person[] = [
  { id: "u_ada", name: "Ada Lovelace", role: "Engineer", commits: 128 },
  { id: "u_alan", name: "Alan Turing", role: "Researcher", commits: 342 },
  { id: "u_grace", name: "Grace Hopper", role: "Engineer", commits: 87 },
  { id: "u_kat", name: "Katherine Johnson", role: "Analyst", commits: 53 },
];

const COLUMNS: DataTableColumn<Person>[] = [
  { key: "name", header: "Name", cell: (p) => p.name, sortable: true },
  { key: "role", header: "Role", cell: (p) => p.role },
  {
    key: "commits",
    header: "Commits",
    cell: (p) => p.commits,
    sortable: true,
    align: "end",
    width: "8rem",
  },
];

const meta: Meta<typeof DataTable> = {
  title: "Blocks/DataTable",
  component: DataTable,
  parameters: { layout: "fullscreen" },
};

export default meta;

type Story = StoryObj<typeof DataTable<Person>>;

/* ─── 1. Basic ──────────────────────────────────────────────────────── */
export const Basic: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Presentational table with a real `<caption>` (its accessible " +
          "name), three columns, and four rows. No selection, no sort " +
          "applied. DataTable renders `data` as-is — the consumer owns " +
          "the ordering.",
      },
    },
  },
  render: () => (
    <div className="zs-story-cell" style={{ padding: "1rem", maxInlineSize: "48rem" }}>
      <DataTable<Person>
        data-testid="dt-basic"
        caption="Team members"
        columns={COLUMNS}
        data={PEOPLE}
        rowKey={(p) => p.id}
      />
    </div>
  ),
};

/* ─── 2. Sortable ───────────────────────────────────────────────────── */
export const Sortable: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Controlled `sort` driven by a useState wrapper. Clicking a " +
          "sortable header emits the NEXT sort (a fresh column starts " +
          "`asc`; re-clicking toggles `asc`↔`desc`). The play() clicks " +
          "the Commits header twice and asserts `onSortChange` fired with " +
          "the toggled direction AND the header's `aria-sort` flips.",
      },
    },
  },
  args: { onSortChange: fn() },
  render: function SortableRender(args) {
    const [sort, setSort] = useState<DataTableSort | null>(null);
    return (
      <div className="zs-story-cell" style={{ padding: "1rem", maxInlineSize: "48rem" }}>
        <DataTable<Person>
          data-testid="dt-sortable"
          caption="Sort by clicking a header"
          columns={COLUMNS}
          data={PEOPLE}
          rowKey={(p) => p.id}
          sort={sort}
          onSortChange={(next) => {
            setSort(next);
            args.onSortChange?.(next);
          }}
        />
      </div>
    );
  },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    const commitsHeaderButton = canvas.getByTestId("data-table-sort-commits");
    const commitsTh = commitsHeaderButton.closest("th") as HTMLElement;

    // Initially no sort → aria-sort="none" on sortable columns.
    expect(commitsTh).toHaveAttribute("aria-sort", "none");

    // First click → ascending.
    await userEvent.click(commitsHeaderButton);
    expect(args.onSortChange).toHaveBeenLastCalledWith({
      key: "commits",
      direction: "asc",
    });
    expect(commitsTh).toHaveAttribute("aria-sort", "ascending");

    // Second click on the active column → toggles to descending.
    await userEvent.click(commitsHeaderButton);
    expect(args.onSortChange).toHaveBeenLastCalledWith({
      key: "commits",
      direction: "desc",
    });
    expect(commitsTh).toHaveAttribute("aria-sort", "descending");
  },
};

/* ─── 3. SelectableMultiple ─────────────────────────────────────────── */
export const SelectableMultiple: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "`selection='multiple'` adds a leading checkbox column plus a " +
          "header select-all checkbox with an `indeterminate` partial " +
          "state. Selection is controlled. The play() starts with a " +
          "PARTIAL selection (one visible row + one OFF-DATA key from " +
          "another page), clicks select-all then clear-all, and asserts " +
          "the off-data key survives both (select-all/clear-all only " +
          "touch visible rows), then toggles one row checkbox.",
      },
    },
  },
  args: { onSelectionChange: fn() },
  render: function SelectableRender(args) {
    // Seed a PARTIAL selection so the header checkbox starts indeterminate.
    // `u_offpage` is an OFF-DATA key (a row on another page/filter that is
    // not in `data`) — select-all/clear-all must never drop it.
    const [selected, setSelected] = useState<string[]>(["u_offpage", "u_ada"]);
    return (
      <div className="zs-story-cell" style={{ padding: "1rem", maxInlineSize: "48rem" }}>
        <DataTable<Person>
          data-testid="dt-selectable"
          caption="Select rows"
          columns={COLUMNS}
          data={PEOPLE}
          rowKey={(p) => p.id}
          selection="multiple"
          selectedKeys={selected}
          onSelectionChange={(keys) => {
            setSelected(keys);
            args.onSelectionChange?.(keys);
          }}
        />
      </div>
    );
  },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    const lastKeys = () =>
      (args.onSelectionChange as ReturnType<typeof fn>).mock.calls.at(-1)
        ?.[0] as string[];

    // Partial selection (1 visible of 4, + 1 off-data) → indeterminate.
    const selectAll = canvas.getByTestId("data-table-select-all");
    expect(selectAll).toHaveAttribute("data-indeterminate");

    // Select-all → appends only the MISSING visible keys; the existing
    // selection (off-data key + already-selected u_ada) is preserved in
    // order, not wiped.
    await userEvent.click(selectAll);
    expect(args.onSelectionChange).toHaveBeenLastCalledWith([
      "u_offpage",
      "u_ada",
      "u_alan",
      "u_grace",
      "u_kat",
    ]);
    // All VISIBLE rows selected → header checkbox checked (off-data key
    // doesn't affect the visible-only allSelected derivation).
    expect(selectAll).toHaveAttribute("data-checked");
    expect(selectAll).not.toHaveAttribute("data-indeterminate");

    // Clear-all → removes only the VISIBLE keys; the off-data key SURVIVES
    // (regression: pre-fix clear emitted [] and dropped it).
    await userEvent.click(selectAll);
    const afterClear = lastKeys();
    expect(afterClear).toEqual(["u_offpage"]);
    expect(afterClear).toContain("u_offpage");
    expect(afterClear).not.toContain("u_ada");

    // Re-select-all, then toggle one row off → that visible key drops, the
    // off-data key still rides along.
    await userEvent.click(selectAll);
    const adaRow = canvas.getByTestId("data-table-row-select-u_ada");
    await userEvent.click(adaRow);
    const lastCall = lastKeys();
    expect(lastCall).not.toContain("u_ada");
    expect(lastCall).toContain("u_alan");
    expect(lastCall).toContain("u_offpage");
  },
};

/* ─── 4. Empty ──────────────────────────────────────────────────────── */
export const Empty: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Empty `data` (and not loading) renders the default " +
          "`<EmptyState>` in a single full-span row. The table keeps its " +
          "accessible name and header so its shape is stable.",
      },
    },
  },
  render: () => (
    <div className="zs-story-cell" style={{ padding: "1rem", maxInlineSize: "48rem" }}>
      <DataTable<Person>
        data-testid="dt-empty"
        caption="No members yet"
        columns={COLUMNS}
        data={[]}
        rowKey={(p) => p.id}
      />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // The default EmptyState heading renders inside the full-span row.
    expect(canvas.getByText("No data")).toBeInTheDocument();
  },
};

/* ─── 5. Loading ────────────────────────────────────────────────────── */
export const Loading: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "`loading` renders `loadingRowCount` Skeleton rows (the header " +
          "still renders). Skeletons are `aria-hidden`, so the table " +
          "still needs its accessible name (the caption supplies it).",
      },
    },
  },
  render: () => (
    <div className="zs-story-cell" style={{ padding: "1rem", maxInlineSize: "48rem" }}>
      <DataTable<Person>
        data-testid="dt-loading"
        caption="Loading members"
        columns={COLUMNS}
        data={[]}
        rowKey={(p) => p.id}
        loading
        loadingRowCount={4}
      />
    </div>
  ),
  play: async ({ canvasElement }) => {
    // Four loading rows render; each carries the loading-row slot.
    const rows = canvasElement.querySelectorAll(
      '[data-slot="data-table-loading-row"]',
    );
    expect(rows.length).toBe(4);
  },
};

/* ─── 6. StickyHeader ───────────────────────────────────────────────── */
export const StickyHeader: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "`stickyHeader` pins the header so rows scroll under it. The " +
          "table sits in a CONSUMER-sized scroll container (a wrapper with " +
          "`max-block-size` + `overflow: auto`). The header paints an " +
          "opaque background so rows don't bleed through.",
      },
    },
  },
  render: () => {
    // A taller dataset so there's something to scroll.
    const many: Person[] = Array.from({ length: 24 }, (_, i) => ({
      id: `u_${i}`,
      name: `Person ${i + 1}`,
      role: i % 2 === 0 ? "Engineer" : "Analyst",
      commits: (i * 37) % 500,
    }));
    return (
      <div
        className="zs-story-cell"
        style={{
          padding: "1rem",
          maxInlineSize: "48rem",
          maxBlockSize: "16rem",
          overflow: "auto",
        }}
      >
        <DataTable<Person>
          data-testid="dt-sticky"
          caption="Scroll — the header stays pinned"
          columns={COLUMNS}
          data={many}
          rowKey={(p) => p.id}
          stickyHeader
        />
      </div>
    );
  },
};

/* ─── 7. Density (compact) ──────────────────────────────────────────── */
export const DensityCompact: Story = {
  name: "Density — compact",
  parameters: {
    docs: {
      description: {
        story:
          "`density='compact'` tightens the cell padding only — the type " +
          "scale and separators are unchanged. Use it for dense, " +
          "scan-heavy tables.",
      },
    },
  },
  render: () => (
    <div className="zs-story-cell" style={{ padding: "1rem", maxInlineSize: "48rem" }}>
      <DataTable<Person>
        data-testid="dt-compact"
        caption="Compact density"
        columns={COLUMNS}
        data={PEOPLE}
        rowKey={(p) => p.id}
        selection="multiple"
        selectedKeys={["u_alan"]}
        onSelectionChange={() => {}}
        density="compact"
      />
    </div>
  ),
};
