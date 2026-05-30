/*
 * DataTable — a PRESENTATIONAL data table.
 *
 * The long-promised `Table`, built as the most complex Wave-3 block. It
 * composes the existing primitives (Checkbox for selection, EmptyState
 * for the empty slot, Skeleton for loading rows) and renders a REAL
 * semantic `<table>` — `<caption?>`, `<thead><tr><th scope="col">`,
 * `<tbody><tr><td>` — so assistive tech gets the native table model for
 * free (row/column navigation, header association).
 *
 * Presentational contract — the BOUNDARY that keeps this block reusable:
 *
 *   THE CONSUMER owns the data and ALL sort/selection LOGIC. DataTable
 *   renders the current state and emits INTENT:
 *     - `sort` + `onSortChange`   (controlled active sort)
 *     - `selectedKeys` + `onSelectionChange` (controlled selection)
 *
 *   DataTable never sorts the `data` array, never mutates
 *   `selectedKeys`, never holds its own copy of either. Clicking a
 *   sortable header computes the *next* sort and hands it back; the
 *   consumer decides what to do (re-query, re-sort in memory, ignore).
 *   This mirrors the headless-table convention (TanStack Table, Base UI
 *   patterns) where the data layer and the presentation layer stay
 *   decoupled.
 *
 * Virtualization — DEFERRED. DataTable renders ALL rows in `data`; there
 * is no windowing. For very large datasets, paginate or virtualize
 * upstream (slice the `data` array, or wrap in a windowing container).
 * A first-class windowing mode is a future slice; we will not pull a
 * windowing dependency into the block library before it is designed.
 *
 * State-slot precedence (highest wins): error > loading > empty > data.
 *   - error:   `renderError` provided AND returns a truthy node → that
 *              node fills a single full-span row; data rows are skipped.
 *   - loading: `loading` → `loadingRowCount` Skeleton rows in the tbody
 *              (the header still renders so the table's shape is stable).
 *   - empty:   `data` is empty (and not loading/error) → `renderEmpty?.()
 *              ?? <EmptyState/>` in a single full-span row.
 *   - data:    the rows.
 *
 * Glass rule — the sticky header paints an OPAQUE background so rows
 * scroll *under* it without bleeding through. Selected-row tint is an
 * opaque `color-mix(... var(--zs-surface))`, never a translucent overlay
 * (axe's contrast walk can't see through translucency).
 *
 * Accessible name — a table MUST have an accessible name. Provide either
 * a `caption` (rendered as a real `<caption>`) or an `aria-label`. A
 * dev-mode warn fires when NEITHER is supplied (DCE'd out of production).
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type CSSProperties,
  type ReactNode,
} from "react";
import { classnames } from "../../components/_classnames";
import { Checkbox } from "../../components/Checkbox";
import { Skeleton } from "../Skeleton";
import { EmptyState } from "../EmptyState";

/**
 * Horizontal alignment of a column's header + cells. Intentionally
 * narrows the shared layout `Align` — a table cell can't `"stretch"`.
 */
export type DataTableAlign = "start" | "center" | "end";

/** Row density — changes cell padding only. */
export type DataTableDensity = "comfortable" | "compact";

export interface DataTableColumn<T> {
  /** Stable column identity. Used as the React key, the `sort.key`
   *  emitted on header click, and the per-column `data-column` attr. */
  key: string;
  /** Header content — rendered in `<th scope="col">` (inside a
   *  `<button>` when `sortable`). */
  header: ReactNode;
  /** Cell renderer for a given row. Returns the `<td>` contents. */
  cell: (row: T) => ReactNode;
  /** When true, the header becomes a real `<button>` that emits
   *  `onSortChange` and carries `aria-sort`. Default false. */
  sortable?: boolean;
  /** Horizontal alignment of this column's header and cells. Default
   *  `"start"`. `"end"` is the convention for numeric columns. */
  align?: DataTableAlign;
  /** A CSS length width hint applied via `<col>` (e.g. `"12rem"`,
   *  `"20%"`). Optional — columns size to content by default. */
  width?: string;
}

/** The active sort: which column, which direction. `null` = unsorted. */
export interface DataTableSort {
  key: string;
  direction: "asc" | "desc";
}

/** Selection model. `"none"` (default) renders no selection column. */
export type DataTableSelectionMode = "none" | "single" | "multiple";

export interface DataTableProps<T>
  extends Omit<ComponentPropsWithoutRef<"table">, "children"> {
  /** Column definitions, left-to-right. */
  columns: DataTableColumn<T>[];
  /** Row data. DataTable renders these as-is — it does NOT sort or
   *  filter. Renders ALL rows (no virtualization — see file header). */
  data: T[];
  /** Derives a stable string key for a row — the React key AND the
   *  selection identity emitted in `onSelectionChange`. */
  rowKey: (row: T) => string;

  /**
   * A real `<caption>` for the table — the preferred accessible name
   * (visible AND announced). When omitted, `aria-label` is REQUIRED;
   * a dev-mode warn fires if NEITHER is supplied.
   */
  caption?: ReactNode;

  /**
   * The active sort, CONTROLLED by the consumer. `null`/absent = no
   * column sorted. DataTable reflects this in `aria-sort` and the sort
   * affordance but never changes `data` ordering itself.
   */
  sort?: DataTableSort | null;
  /**
   * Emitted when a sortable header is activated. Receives the NEXT
   * sort: a fresh column starts `"asc"`; re-activating the active
   * column toggles `asc`→`desc`→`asc`. The consumer applies it.
   */
  onSortChange?: (sort: DataTableSort) => void;

  /**
   * Selection model. `"none"` (default) renders no checkbox column.
   * `"single"` adds a per-row checkbox (no header select-all).
   * `"multiple"` adds per-row checkboxes AND a header select-all
   * checkbox with an `indeterminate` partial state.
   */
  selection?: DataTableSelectionMode;
  /** Selected row keys, CONTROLLED by the consumer. */
  selectedKeys?: string[];
  /** Emitted with the NEXT full set of selected keys after a toggle. */
  onSelectionChange?: (keys: string[]) => void;
  /**
   * Builds the accessible label for a per-row selection checkbox. The
   * checkbox has no visible label, so this names it for AT. Defaults to
   * `Select row {n}` (1-based). Override to label by a row field, e.g.
   * `(row) => \`Select \${row.name}\``.
   */
  rowSelectionLabel?: (row: T, index: number) => string;

  /**
   * Pin the header so rows scroll under it. `<thead>` cells become
   * `position: sticky; inset-block-start: 0` with an opaque header bg.
   * The table must sit in a scroll container the CONSUMER sizes (set a
   * `max-block-size` + `overflow: auto` on the wrapper) for this to
   * have an effect.
   */
  stickyHeader?: boolean;

  /** Row density. `"comfortable"` (default) or `"compact"` — changes
   *  cell padding only. */
  density?: DataTableDensity;

  /**
   * Loading state. When true, the tbody renders `loadingRowCount`
   * Skeleton rows instead of data (the header still renders). Ranks
   * below `error` and above `empty` in the state precedence.
   */
  loading?: boolean;
  /** Number of Skeleton rows shown while `loading`. Default `5`. */
  loadingRowCount?: number;

  /**
   * Renders the empty slot when `data` is empty (and not loading/in
   * error). Defaults to a generic `<EmptyState title="No data" …/>`.
   * Filled into a single full-span row.
   */
  renderEmpty?: () => ReactNode;
  /**
   * Renders an error slot. When provided AND it returns a truthy node,
   * that node replaces the data rows (highest state precedence). Filled
   * into a single full-span row.
   */
  renderError?: () => ReactNode;

  /** Root `data-slot`. Defaults to `"data-table"`. */
  "data-slot"?: string;
}

/** Maps the column `align` to the `<td>`/`<th>` `data-align` attr. */
function alignAttr(align: DataTableAlign | undefined): DataTableAlign {
  return align ?? "start";
}

/** Maps the active sort to a `<th>` `aria-sort` value for a column. */
function ariaSortFor(
  column: DataTableColumn<unknown>,
  sort: DataTableSort | null | undefined,
): "ascending" | "descending" | "none" | undefined {
  // Non-sortable headers carry NO aria-sort (absent), per the WAI-ARIA
  // pattern — aria-sort="none" is reserved for sortable-but-inactive
  // columns so AT can distinguish "you can sort this" from "you can't".
  if (!column.sortable) return undefined;
  if (sort && sort.key === column.key) {
    return sort.direction === "asc" ? "ascending" : "descending";
  }
  return "none";
}

/** Computes the next sort when a sortable header is activated. A fresh
 *  column starts ascending; the active column toggles asc↔desc. */
function nextSort(
  columnKey: string,
  current: DataTableSort | null | undefined,
): DataTableSort {
  if (current && current.key === columnKey) {
    return {
      key: columnKey,
      direction: current.direction === "asc" ? "desc" : "asc",
    };
  }
  return { key: columnKey, direction: "asc" };
}

/* ─── sort affordance — two distinct glyphs (shape ≠ color) ──────────
 * The active direction shows a filled caret (▲/▼); an inactive sortable
 * column shows a muted up/down pair so the affordance reads as "sortable"
 * before any sort is applied. aria-hidden — the `aria-sort` attribute on
 * the <th> carries the meaning for AT. */
function SortGlyph({
  state,
}: {
  state: "ascending" | "descending" | "none";
}) {
  return (
    <span className="zs-data-table__sort-glyph" aria-hidden="true" data-state={state}>
      <svg viewBox="0 0 12 12" focusable="false" role="presentation">
        <path className="zs-data-table__sort-up" d="M6 2.5L9 6H3z" />
        <path className="zs-data-table__sort-down" d="M6 9.5L3 6h6z" />
      </svg>
    </span>
  );
}

function DataTableInner<T>(
  {
    columns,
    data,
    rowKey,
    caption,
    sort,
    onSortChange,
    selection = "none",
    selectedKeys,
    onSelectionChange,
    rowSelectionLabel,
    stickyHeader = false,
    density = "comfortable",
    loading = false,
    loadingRowCount = 5,
    renderEmpty,
    renderError,
    className,
    "data-slot": dataSlot = "data-table",
    "aria-label": ariaLabel,
    ...rest
  }: DataTableProps<T>,
  ref: React.Ref<HTMLTableElement>,
) {
  // Dev-mode validation: a table with no accessible name is a serious
  // a11y defect (AT announces "table" with no context). Gated to
  // non-production so bundlers DCE the whole branch out of prod builds.
  if (process.env.NODE_ENV !== "production") {
    if (caption == null && (ariaLabel == null || ariaLabel === "")) {
      // eslint-disable-next-line no-console
      console.warn(
        "DataTable has no accessible name: pass `caption` (preferred — a " +
          "visible <caption>) or `aria-label`. Without one, assistive tech " +
          "announces the table with no context.",
      );
    }
  }

  const hasSelection = selection !== "none";
  const isMultiple = selection === "multiple";
  const selectedSet = new Set(selectedKeys ?? []);
  // Total leading columns = the selection checkbox column (if any).
  const totalColumns = columns.length + (hasSelection ? 1 : 0);

  // Select-all derives from the CURRENT data + selectedKeys (controlled).
  // allSelected only when there is at least one row AND every row key is
  // in the set; `some` drives the indeterminate partial state.
  const rowKeys = data.map((row) => rowKey(row));
  const selectedVisibleCount = rowKeys.filter((k) => selectedSet.has(k)).length;
  const allSelected = rowKeys.length > 0 && selectedVisibleCount === rowKeys.length;
  const someSelected = selectedVisibleCount > 0 && !allSelected;

  const handleHeaderSort = (column: DataTableColumn<T>) => {
    if (!column.sortable) return;
    onSortChange?.(nextSort(column.key, sort));
  };

  const handleSelectAll = () => {
    if (!onSelectionChange) return;
    // Toggle the *visible* rows WITHOUT touching off-data keys (rows on
    // other pages / behind filters that the consumer still tracks):
    //   - clear-all: drop only the visible keys, keep every off-data key.
    //   - select-all: append only the MISSING visible keys to the end of
    //     the existing selection (preserving its order + off-data keys).
    const current = selectedKeys ?? [];
    if (allSelected) {
      const visible = new Set(rowKeys);
      onSelectionChange(current.filter((k) => !visible.has(k)));
    } else {
      const missing = rowKeys.filter((k) => !selectedSet.has(k));
      onSelectionChange([...current, ...missing]);
    }
  };

  const handleRowSelect = (key: string) => {
    if (!onSelectionChange) return;
    if (selection === "single") {
      // Single mode: selecting a row replaces the selection; clicking
      // the selected row clears it.
      onSelectionChange(selectedSet.has(key) ? [] : [key]);
      return;
    }
    // Multiple: toggle the key within the existing set, preserving the
    // order the consumer gave us plus any newly-added key at the end.
    if (selectedSet.has(key)) {
      onSelectionChange((selectedKeys ?? []).filter((k) => k !== key));
    } else {
      onSelectionChange([...(selectedKeys ?? []), key]);
    }
  };

  // State precedence: error > loading > empty > data. Compute the error
  // node ONCE and gate on a real truthy check so "", 0, or false don't
  // render a blank full-span error row.
  const errorNode = renderError?.();
  const showError = errorNode != null && errorNode !== false;
  const showLoading = !showError && loading;
  const showEmpty = !showError && !showLoading && data.length === 0;

  const tableClassName = classnames(
    "zs-data-table",
    `zs-data-table--${density}`,
    stickyHeader ? "zs-data-table--sticky" : null,
    className,
  );

  const fullSpanRow = (content: ReactNode, slot: string) => (
    <tr data-slot={slot}>
      <td className="zs-data-table__state-cell" colSpan={totalColumns}>
        {content}
      </td>
    </tr>
  );

  return (
    <table
      {...rest}
      ref={ref}
      data-slot={dataSlot}
      data-density={density}
      aria-label={ariaLabel}
      className={tableClassName}
    >
      {caption != null ? (
        <caption className="zs-data-table__caption" data-slot="data-table-caption">
          {caption}
        </caption>
      ) : null}

      {columns.some((c) => c.width != null) || hasSelection ? (
        <colgroup>
          {hasSelection ? (
            <col className="zs-data-table__select-col" />
          ) : null}
          {columns.map((column) => (
            <col
              key={column.key}
              style={
                column.width != null
                  ? ({ inlineSize: column.width } as CSSProperties)
                  : undefined
              }
            />
          ))}
        </colgroup>
      ) : null}

      <thead data-slot="data-table-head">
        <tr>
          {hasSelection ? (
            <th
              scope="col"
              data-slot="data-table-select-header"
              className="zs-data-table__select-cell"
            >
              {isMultiple ? (
                <Checkbox
                  aria-label="Select all rows"
                  checked={allSelected}
                  indeterminate={someSelected}
                  onCheckedChange={handleSelectAll}
                  disabled={rowKeys.length === 0}
                  data-testid="data-table-select-all"
                />
              ) : (
                // Single-select has no header checkbox — render a
                // visually-hidden label so the column still has a
                // programmatic name for the header cell.
                <span className="zs-visually-hidden">Select</span>
              )}
            </th>
          ) : null}
          {columns.map((column) => {
            const sortState = ariaSortFor(
              column as DataTableColumn<unknown>,
              sort,
            );
            return (
              <th
                key={column.key}
                scope="col"
                data-slot="data-table-column-header"
                data-column={column.key}
                data-align={alignAttr(column.align)}
                aria-sort={sortState}
                className="zs-data-table__th"
              >
                {column.sortable ? (
                  <button
                    type="button"
                    className="zs-data-table__sort-button"
                    onClick={() => handleHeaderSort(column)}
                    data-testid={`data-table-sort-${column.key}`}
                  >
                    <span className="zs-data-table__header-label">
                      {column.header}
                    </span>
                    <SortGlyph state={sortState ?? "none"} />
                  </button>
                ) : (
                  <span className="zs-data-table__header-label">
                    {column.header}
                  </span>
                )}
              </th>
            );
          })}
        </tr>
      </thead>

      <tbody data-slot="data-table-body">
        {showError
          ? fullSpanRow(errorNode, "data-table-error")
          : showLoading
            ? // Per-cell skeletons (one bar per column, incl. the selection
              // column) over a single full-width colSpan bar: the loading
              // state mirrors the populated table's structure (the premium
              // pattern; Linear/Vercel do this), and a per-column bar keeps
              // valid table structure (cells match the rendered columns).
              Array.from({ length: Math.max(0, loadingRowCount) }, (_, i) => (
                <tr key={`skeleton-${i}`} data-slot="data-table-loading-row">
                  {hasSelection ? (
                    <td className="zs-data-table__select-cell">
                      <Skeleton variant="text" width="1rem" />
                    </td>
                  ) : null}
                  {columns.map((column) => (
                    <td
                      key={column.key}
                      data-align={alignAttr(column.align)}
                      className="zs-data-table__td"
                    >
                      <Skeleton variant="text" />
                    </td>
                  ))}
                </tr>
              ))
            : showEmpty
              ? fullSpanRow(
                  renderEmpty?.() ?? (
                    <EmptyState
                      title="No data"
                      description="There's nothing to show here yet."
                    />
                  ),
                  "data-table-empty",
                )
              : data.map((row, index) => {
                  const key = rowKeys[index];
                  const selected = selectedSet.has(key);
                  return (
                    <tr
                      key={key}
                      data-slot="data-table-row"
                      data-selected={selected ? "" : undefined}
                      aria-selected={hasSelection ? selected : undefined}
                    >
                      {hasSelection ? (
                        <td className="zs-data-table__select-cell">
                          <Checkbox
                            aria-label={
                              rowSelectionLabel?.(row, index) ??
                              `Select row ${index + 1}`
                            }
                            checked={selected}
                            onCheckedChange={() => handleRowSelect(key)}
                            data-testid={`data-table-row-select-${key}`}
                          />
                        </td>
                      ) : null}
                      {columns.map((column) => (
                        <td
                          key={column.key}
                          data-slot="data-table-cell"
                          data-column={column.key}
                          data-align={alignAttr(column.align)}
                          className="zs-data-table__td"
                        >
                          {column.cell(row)}
                        </td>
                      ))}
                    </tr>
                  );
                })}
      </tbody>
    </table>
  );
}

/**
 * DataTable — presentational, generic over the row type `T`.
 *
 * @example
 * ```tsx
 * <DataTable
 *   caption="Users"
 *   columns={[
 *     { key: "name", header: "Name", cell: (u) => u.name, sortable: true },
 *     { key: "email", header: "Email", cell: (u) => u.email },
 *   ]}
 *   data={users}
 *   rowKey={(u) => u.id}
 *   sort={sort}
 *   onSortChange={setSort}
 *   selection="multiple"
 *   selectedKeys={selected}
 *   onSelectionChange={setSelected}
 * />
 * ```
 *
 * `forwardRef` loses generic inference, so we re-assert the generic
 * signature on the exported binding — calling `<DataTable<User> …/>`
 * keeps `cell`/`rowKey`/`rowSelectionLabel` typed against `User`.
 */
export const DataTable = forwardRef(DataTableInner) as <T>(
  props: DataTableProps<T> & { ref?: React.Ref<HTMLTableElement> },
) => ReturnType<typeof DataTableInner>;
