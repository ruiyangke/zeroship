import type { Meta, StoryObj } from "@storybook/react";
import { expect, fn, userEvent, waitFor, within } from "@storybook/test";
import { useState } from "react";
import {
  DataTable,
  type DataTableColumn,
  type DataTableColumnFilters,
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
          "name), three columns, and four rows. No selection. With only " +
          "four rows the pagination footer auto-hides (rows ≤ smallest page " +
          "size). The default managed engine is dormant until you sort or " +
          "search.",
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
          "Controlled `sort` lifted into a useState wrapper while the " +
          "managed engine still reorders the rows. Clicking a sortable " +
          "header emits the NEXT sort (a fresh column starts `asc`; " +
          "re-clicking toggles `asc`↔`desc`). The play() clicks the " +
          "Commits header twice and asserts `onSortChange` fired with the " +
          "toggled direction AND the header's `aria-sort` flips.",
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

    const selectAll = canvas.getByTestId("data-table-select-all");
    expect(selectAll).toHaveAttribute("data-indeterminate");

    await userEvent.click(selectAll);
    expect(args.onSelectionChange).toHaveBeenLastCalledWith([
      "u_offpage",
      "u_ada",
      "u_alan",
      "u_grace",
      "u_kat",
    ]);
    expect(selectAll).toHaveAttribute("data-checked");
    expect(selectAll).not.toHaveAttribute("data-indeterminate");

    await userEvent.click(selectAll);
    const afterClear = lastKeys();
    expect(afterClear).toEqual(["u_offpage"]);
    expect(afterClear).toContain("u_offpage");
    expect(afterClear).not.toContain("u_ada");

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
          "DataTable owns the scroll region (`.zs-data-table__scroll`); " +
          "the consumer sizes its block-size. The header paints an opaque " +
          "background so rows don't bleed through. Pagination is forced " +
          "off here so the full dataset scrolls.",
      },
    },
  },
  render: () => {
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
          overflow: "hidden",
        }}
      >
        <DataTable<Person>
          data-testid="dt-sticky"
          caption="Scroll — the header stays pinned"
          columns={COLUMNS}
          data={many}
          rowKey={(p) => p.id}
          stickyHeader
          paginated={false}
          style={{ maxBlockSize: "14rem" }}
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

/* ─── 8. Managed (client-side sort/filter/paginate) ─────────────────── */
const MANY: Person[] = Array.from({ length: 25 }, (_, i) => ({
  id: `m_${i}`,
  name: `Member ${String(i + 1).padStart(2, "0")}`,
  role: ["Engineer", "Analyst", "Researcher"][i % 3] as string,
  commits: (i * 53) % 400,
}));

export const Managed: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "The default MANAGED mode — no `manual*` flags. DataTable sorts, " +
          "filters, and paginates `data` itself. The play() types a global " +
          "search (rows shrink), sorts by a header (order changes), then " +
          "pages to page 2 (the rendered rows change). All three transforms " +
          "run inside the block; `on*Change` still fire so a consumer can " +
          "observe.",
      },
    },
  },
  render: () => (
    <div className="zs-story-cell" style={{ padding: "1rem", maxInlineSize: "48rem" }}>
      <DataTable<Person>
        data-testid="dt-managed"
        caption="25 members — searchable, sortable, paginated"
        columns={COLUMNS}
        data={MANY}
        rowKey={(p) => p.id}
        defaultPageSize={10}
        pageSizeOptions={[10, 25]}
      />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    const rowCount = () =>
      canvasElement.querySelectorAll('[data-slot="data-table-row"]').length;
    const names = () =>
      Array.from(
        canvasElement.querySelectorAll(
          '[data-slot="data-table-row"] [data-column="name"]',
        ),
      ).map((el) => el.textContent ?? "");

    // Page 1 of a 25-row / 10-per-page set renders 10 rows.
    expect(rowCount()).toBe(10);

    // The pagination footer is composed (re-query it fresh each time —
    // it UNMOUNTS when a filter drops the row count below the page size,
    // so any cached reference would go stale).
    const paginationNow = () => canvas.getByTestId("data-table-pagination");
    expect(paginationNow()).toBeInTheDocument();

    // Global search filters: "Researcher" matches roughly a third of rows.
    const search = canvas.getByTestId("data-table-search");
    await userEvent.type(search, "Researcher");
    await waitFor(() => expect(rowCount()).toBeLessThan(10));
    const filteredCount = rowCount();
    expect(filteredCount).toBeGreaterThan(0);

    // Clear the search → back to a full page of 10.
    await userEvent.clear(search);
    await waitFor(() => expect(rowCount()).toBe(10));

    // Sort by name DESCENDING (click twice) → the page's row order changes
    // vs. the unsorted page-1 order.
    const beforeOrder = names();
    const nameHeader = canvas.getByTestId("data-table-sort-name");
    await userEvent.click(nameHeader); // asc
    await userEvent.click(nameHeader); // desc
    await waitFor(() => expect(names()).not.toEqual(beforeOrder));
    const sortedOrder = names();

    // Go to page 2 → the rendered rows differ from page 1. Resolve the
    // button from the LIVE footer.
    await userEvent.click(
      within(paginationNow()).getByRole("button", { name: /go to page 2/i }),
    );
    await waitFor(() =>
      expect(
        within(paginationNow()).getByRole("button", { name: /go to page 2/i }),
      ).toHaveAttribute("aria-current", "page"),
    );
    await waitFor(() => expect(names()).not.toEqual(sortedOrder));
    expect(rowCount()).toBeGreaterThan(0);
  },
};

/* ─── 9. RichColumns (cell-type presets + actions kebab) ────────────── */
interface Invoice {
  id: string;
  customer: string;
  amount: number;
  issued: string;
  paid: boolean;
  status: "open" | "paid" | "overdue";
}

const INVOICES: Invoice[] = [
  { id: "in_1", customer: "Acme Co", amount: 1299.5, issued: "2026-01-12", paid: true, status: "paid" },
  { id: "in_2", customer: "Globex", amount: 420, issued: "2026-02-03", paid: false, status: "open" },
  { id: "in_3", customer: "Initech", amount: 87.25, issued: "2026-02-20", paid: false, status: "overdue" },
];

export const RichColumns: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Cell-type presets: `currency` (Intl), `date` (Intl), `boolean` " +
          "(glyph + Yes/No text label), `badge` (real Badge with a " +
          "value→intent map), `link` (an `<a>` from `href`), and an " +
          "`actions` column rendering a RowActions kebab — a real `Menu` " +
          "opened by an icon-only `Button`. The play() opens the kebab and " +
          "clicks an item, asserting its `onSelect` fires.",
      },
    },
  },
  render: function RichRender() {
    const [lastAction, setLastAction] = useState<string>("none");
    const columns: DataTableColumn<Invoice>[] = [
      {
        key: "customer",
        header: "Customer",
        type: "link",
        href: (r) => `/customers/${r.id}`,
      },
      { key: "amount", header: "Amount", type: "currency", sortable: true },
      { key: "issued", header: "Issued", type: "date", sortable: true },
      { key: "paid", header: "Paid", type: "boolean", align: "center" },
      {
        key: "status",
        header: "Status",
        type: "badge",
        badgeIntent: (v) =>
          v === "paid" ? "success" : v === "overdue" ? "danger" : "info",
      },
      {
        key: "actions",
        header: "Actions",
        type: "actions",
        actions: (r) => [
          {
            label: "Edit",
            onSelect: () => setLastAction(`edit:${r.id}`),
            "data-testid": `dt-action-edit-${r.id}`,
          },
          {
            label: "Delete",
            danger: true,
            onSelect: () => setLastAction(`delete:${r.id}`),
            "data-testid": `dt-action-delete-${r.id}`,
          },
        ],
      },
    ];
    return (
      <div className="zs-story-cell" style={{ padding: "1rem", maxInlineSize: "52rem" }}>
        <DataTable<Invoice>
          data-testid="dt-rich"
          caption="Invoices"
          columns={columns}
          data={INVOICES}
          rowKey={(r) => r.id}
        />
        <p data-testid="dt-rich-last-action">Last action: {lastAction}</p>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    // Currency cell renders an Intl-formatted value with a $ sign.
    expect(canvas.getByText("$1,299.50")).toBeInTheDocument();
    // Boolean cell carries a text label (not glyph/color alone).
    expect(canvas.getAllByText("Yes").length).toBeGreaterThan(0);
    // Badge text renders.
    expect(canvas.getByText("paid")).toBeInTheDocument();
    // Link cell is a real anchor.
    const link = canvas.getByText("Acme Co").closest("a") as HTMLElement;
    expect(link).toHaveAttribute("href", "/customers/in_1");

    // Open the first row's actions kebab.
    const triggers = canvas.getAllByRole("button", { name: "Row actions" });
    expect(triggers.length).toBe(3);
    await userEvent.click(triggers[0]);

    // The Menu portals to the body — query the document, not the canvas.
    const body = within(document.body);
    const editItem = await body.findByTestId("dt-action-edit-in_1");
    await userEvent.click(editItem);

    // The action's onSelect fired.
    await waitFor(() =>
      expect(canvas.getByTestId("dt-rich-last-action")).toHaveTextContent(
        "Last action: edit:in_1",
      ),
    );
  },
};

/* ─── 10. ManualServerSide ──────────────────────────────────────────── */
export const ManualServerSide: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Fully server-side: `manualSorting` + `manualFiltering` + " +
          "`manualPagination` with controlled state. DataTable does NOT " +
          "transform `data` — it renders the given page as-is and only " +
          "emits intent. The play() asserts the `on*Change` handlers fire " +
          "AND that the rendered rows are exactly the `data` passed for the " +
          "current page (no client re-sort/filter).",
      },
    },
  },
  args: {
    onSortChange: fn(),
    onPageChange: fn(),
    onGlobalFilterChange: fn(),
  },
  render: function ManualRender(args) {
    const [sort, setSort] = useState<DataTableSort | null>(null);
    const [page, setPage] = useState(2);
    const [query, setQuery] = useState("");
    // The "server" hands back exactly these two rows for page 2. DataTable
    // must render them as-is regardless of sort/filter state.
    const pageRows: Person[] = [
      { id: "s_1", name: "Server Row A", role: "Engineer", commits: 11 },
      { id: "s_2", name: "Server Row B", role: "Analyst", commits: 22 },
    ];
    return (
      <div className="zs-story-cell" style={{ padding: "1rem", maxInlineSize: "48rem" }}>
        <DataTable<Person>
          data-testid="dt-manual"
          caption="Server-side paged data"
          columns={COLUMNS}
          data={pageRows}
          rowKey={(p) => p.id}
          manualSorting
          manualFiltering
          manualPagination
          total={42}
          sort={sort}
          onSortChange={(next) => {
            setSort(next);
            args.onSortChange?.(next);
          }}
          globalFilter={query}
          onGlobalFilterChange={(q) => {
            setQuery(q);
            args.onGlobalFilterChange?.(q);
          }}
          page={page}
          pageSize={2}
          onPageChange={(p) => {
            setPage(p);
            args.onPageChange?.(p);
          }}
        />
      </div>
    );
  },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);

    const names = () =>
      Array.from(
        canvasElement.querySelectorAll(
          '[data-slot="data-table-row"] [data-column="name"]',
        ),
      ).map((el) => el.textContent);

    // DataTable renders EXACTLY the given page rows, in the given order.
    expect(names()).toEqual(["Server Row A", "Server Row B"]);

    // The footer reports the SERVER total (42), not the 2 local rows:
    // 42 / pageSize 2 = 21 pages → a "Go to page 21" button exists.
    const pagination = canvas.getByTestId("data-table-pagination");
    expect(
      within(pagination).getByRole("button", { name: /page 21/i }),
    ).toBeInTheDocument();

    // Sorting emits intent but does NOT reorder the rendered rows.
    const commitsHeader = canvas.getByTestId("data-table-sort-commits");
    await userEvent.click(commitsHeader);
    expect(args.onSortChange).toHaveBeenLastCalledWith({
      key: "commits",
      direction: "asc",
    });
    expect(names()).toEqual(["Server Row A", "Server Row B"]);

    // Typing in the search emits onGlobalFilterChange but does NOT filter
    // the rendered rows (manualFiltering).
    const search = canvas.getByTestId("data-table-search");
    await userEvent.type(search, "zzz");
    expect(args.onGlobalFilterChange).toHaveBeenCalled();
    expect(names()).toEqual(["Server Row A", "Server Row B"]);

    // Prev fires onPageChange with page 1; the rows stay (the consumer
    // would refetch). manualPagination does NOT reset the page itself.
    const prev = within(pagination).getByRole("button", { name: /previous/i });
    await userEvent.click(prev);
    expect(args.onPageChange).toHaveBeenLastCalledWith(1);
  },
};

/* ─── 11. GlobalFilterEmpty ─────────────────────────────────────────── */
export const GlobalFilterEmpty: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "When the global search matches no rows, the empty slot renders " +
          "a 'No results' EmptyState (distinct from the no-data variant). " +
          "The play() types a query with no matches and asserts the " +
          "no-results message appears.",
      },
    },
  },
  render: () => (
    <div className="zs-story-cell" style={{ padding: "1rem", maxInlineSize: "48rem" }}>
      <DataTable<Person>
        data-testid="dt-filter-empty"
        caption="Searchable team"
        columns={COLUMNS}
        data={PEOPLE}
        rowKey={(p) => p.id}
        searchable
      />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const search = canvas.getByTestId("data-table-search");
    await userEvent.type(search, "qqqzzz-no-match");
    await waitFor(() =>
      expect(canvas.getByText("No results")).toBeInTheDocument(),
    );
    expect(
      canvas.getByText("No rows match your search or filters."),
    ).toBeInTheDocument();
  },
};

/* ─── 12. ColumnFilters (per-column text filter row) ────────────────── */
export const ColumnFilters: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "`filterable` enables a per-column text-filter row under the " +
          "header. Each filterable column gets a labelled `Input` whose " +
          "value is a case-insensitive contains match. The play() filters " +
          "the Role column and asserts the rows shrink.",
      },
    },
  },
  render: function ColumnFiltersRender() {
    const [filters, setFilters] = useState<DataTableColumnFilters>({});
    return (
      <div className="zs-story-cell" style={{ padding: "1rem", maxInlineSize: "48rem" }}>
        <DataTable<Person>
          data-testid="dt-colfilter"
          caption="Filter by column"
          columns={COLUMNS}
          data={PEOPLE}
          rowKey={(p) => p.id}
          filterable
          columnFilters={filters}
          onColumnFiltersChange={setFilters}
          searchable={false}
        />
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const rowCount = () =>
      canvasElement.querySelectorAll('[data-slot="data-table-row"]').length;
    const canvas = within(canvasElement);

    expect(rowCount()).toBe(4);
    const roleFilter = canvas.getByTestId("data-table-filter-role");
    await userEvent.type(roleFilter, "Engineer");
    await waitFor(() => expect(rowCount()).toBe(2));
  },
};
