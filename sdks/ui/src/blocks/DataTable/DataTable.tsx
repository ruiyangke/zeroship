/*
 * DataTable v3 — a presentational data grid with an opt-in MANAGED engine,
 * now driven by the headless @tanstack/react-table engine.
 *
 * The block still renders a REAL semantic `<table>` — `<caption?>`,
 * `<thead><tr><th scope="col">`, `<tbody><tr><td>` — so assistive tech
 * gets the native table model for free (row/column navigation, header
 * association). Around that table sit a toolbar (title + actions +
 * global search), an optional per-column text-filter row, and a footer
 * that composes the real `Pagination` block.
 *
 * ─── Engine: @tanstack/react-table (headless) ─────────────────────────
 *
 * The sort / filter / paginate / selection pipeline is now a TanStack
 * `useReactTable` instance — the same architecture we use elsewhere
 * (Base UI = headless engine for interactive primitives; TanStack =
 * headless engine for the table). The VISUAL layer, the `--zs-*` token
 * system, the cell-type renderers, RowActions, the Pagination/Badge/Menu/
 * Input/Checkbox/Skeleton/EmptyState composition, the `data-slot`
 * vocabulary, and every a11y wire stay OURS. TanStack only owns the
 * row-model math.
 *
 * Each `DataTableColumn<T>` maps to a TanStack `ColumnDef<T>`:
 *   - `accessorFn` = `column.accessor ?? (row) => row[key]`
 *   - `id` = `column.key`
 *   - `enableSorting` = `!!column.sortable` (actions never sortable)
 *   - `enableColumnFilter` = `column.filterable !== false`
 *   - `align` / `width` / `minWidth` / `truncate` ride in `column.meta`
 * The selection checkbox and the actions kebab are NOT TanStack display
 * columns — they live outside the column model (our `<th>`/`<td>` chrome),
 * because their a11y + the visible-only/off-page selection semantics are
 * hand-tuned (see Selection below).
 *
 * ─── The managed / manual matrix (unchanged DX) ──────────────────────
 *
 * DataTable SORTS, FILTERS, and PAGINATES `data` itself by default —
 * `<DataTable columns data rowKey />` is a working grid out of the box.
 * Each axis (sort / globalFilter / columnFilters / page+pageSize) is:
 *   - MANAGED (default): TanStack's `getSorted/Filtered/PaginationRowModel`
 *     transform `data`; seeded from our `default*`; `on…Change` still fires.
 *   - CONTROLLED: pass the value prop + `on…Change`; we feed it into the
 *     TanStack `state`, TanStack still transforms.
 *   - MANUAL: pass the `manual…` flag → set the matching TanStack
 *     `manual…: true` so TanStack does NOT transform that axis; we render
 *     the consumer-provided rows/`total` and only emit intent.
 *
 *   controlled?  manual?   behavior
 *   ──────────── ──────── ─────────────────────────────────────────────
 *   no           no       MANAGED (default). Internal TanStack state;
 *                         `data` is transformed; on…Change still fires.
 *   yes          no       Controlled value, TanStack still transforms.
 *   no           yes      We own the value, TanStack does NOT transform.
 *   yes          yes      Fully presentational — the consumer owns the
 *                         value AND the transform; server-side grids.
 *
 * `manualPagination` needs `total` (the unpaginated row count) for the
 * page math; without it `total` falls back to `data.length`. In managed
 * pagination `total` is the post-filter row count (TanStack's filtered
 * model length) and is computed for you.
 *
 * Single-column sort toggle (asc→desc) and "reset to page 1 on
 * filter/sort/pageSize change in managed pagination" are preserved.
 *
 * ─── Cell-type system (unchanged) ─────────────────────────────────────
 *
 * A column declares a `type` preset that drives a default renderer; an
 * explicit `column.cell` ALWAYS overrides it. The default renderer reads
 * the raw value via `column.accessor?.(row)` (else `row[column.key]`) and
 * formats by type, using only `Intl` (no deps):
 *   text · number · currency · date|datetime · boolean · badge · link · actions
 * (see DefaultCell below).
 *
 * ─── Selection ────────────────────────────────────────────────────────
 *
 * `selection` (`none`/`single`/`multiple`) maps to TanStack
 * `enableRowSelection` / `enableMultiRowSelection` with `getRowId =
 * rowKey`, and our controlled `selectedKeys[]` feeds the TanStack
 * `rowSelection` record. We DO NOT delegate select-all / per-row toggle
 * to TanStack's handlers, because our contract is stricter than
 * TanStack's row-model default:
 *   - `selectedKeys` may include keys NOT present in the current `data`
 *     (an off-page selection); those have no TanStack row and must SURVIVE
 *     a select-all / clear-all.
 *   - select-all / clear-all touch ONLY the currently-VISIBLE rows.
 *   - `onSelectionChange` emits the next full `string[]` of keys.
 * So selection emission stays in our handlers (computed over the visible
 * row keys), while TanStack still tracks `rowSelection` for `getIsSelected`
 * parity. The header select-all checkbox shows `indeterminate` for a
 * partial visible selection.
 *
 * ─── Virtualization — STILL DEFERRED ──────────────────────────────────
 *
 * There is no row windowing. The scale strategy is PAGINATION: managed
 * mode renders only the current page, which keeps the DOM bounded. The
 * documented future windowing path is TanStack + `@tanstack/react-virtual`
 * (the row virtualizer composes cleanly with `getRowModel().rows`); we
 * will not pull a windowing dependency in before it is designed. For
 * server-scale data, use `manualPagination` and feed one page at a time.
 *
 * ─── State-slot precedence (highest wins): error > loading > empty > data
 *   - error:   `renderError` returns a truthy node → fills a full-span row.
 *   - loading: `loading` → `loadingRowCount` Skeleton rows (header stays).
 *   - empty:   no rows to show → `renderEmpty?.()` ?? `<EmptyState/>`. When
 *              a filter/search produced the emptiness, a "no results"
 *              EmptyState variant renders instead of the generic one.
 *   - data:    the (current page of) rows.
 *
 * ─── Glass rule ──────────────────────────────────────────────────────
 * The sticky header paints an OPAQUE background; the selected-row tint is
 * an opaque `color-mix(... var(--zs-surface))`, never translucent.
 *
 * ─── Accessible name ─────────────────────────────────────────────────
 * Provide either a `caption` (a real `<caption>`) or an `aria-label`. A
 * dev-mode warn fires when NEITHER is supplied (DCE'd out of prod).
 *
 * ─── Interactive rows ────────────────────────────────────────────────
 * `onRowClick` makes a row clickable (cursor + activation) but the row is
 * NOT a button — that would nest interactive controls and break the table
 * semantics.
 */
import {
  forwardRef,
  useCallback,
  useMemo,
  useState,
  type ComponentPropsWithoutRef,
  type CSSProperties,
  type ReactNode,
} from "react";
import {
  useReactTable,
  getCoreRowModel,
  getSortedRowModel,
  getFilteredRowModel,
  getPaginationRowModel,
  type ColumnDef,
  type SortingState,
  type ColumnFiltersState,
  type PaginationState,
  type RowSelectionState,
  type Row,
  type FilterFn,
} from "@tanstack/react-table";
import {
  Check,
  ChevronDown,
  ChevronUp,
  ChevronsUpDown,
  Minus,
  MoreVertical,
} from "lucide-react";
import { classnames } from "../../components/_classnames";
import { Badge, type BadgeIntent } from "../../components/Badge";
import { Button } from "../../components/Button";
import { Checkbox } from "../../components/Checkbox";
import { Icon } from "../../components/Icon/Icon";
import { Input } from "../../components/Input";
import { Menu } from "../../components/Menu";
import { Skeleton } from "../../components/Skeleton";
import { Cluster } from "../../layouts/Cluster";
import { EmptyState } from "../EmptyState";
import { Pagination } from "../Pagination";

/**
 * Horizontal alignment of a column's header + cells. Intentionally
 * narrows the shared layout `Align` — a table cell can't `"stretch"`.
 */
export type DataTableAlign = "start" | "center" | "end";

/** Row density — changes cell padding only. */
export type DataTableDensity = "comfortable" | "compact";

/**
 * Built-in cell-type presets. Each drives a default renderer; an explicit
 * `column.cell` always wins. `number`/`currency` end-align by default.
 */
export type DataTableColumnType =
  | "text"
  | "number"
  | "currency"
  | "date"
  | "datetime"
  | "boolean"
  | "badge"
  | "link"
  | "actions";

/** A single row action surfaced in an `actions`-type column's kebab Menu. */
export interface DataTableRowAction {
  /** Visible item label. */
  label: ReactNode;
  /** Fired when the item is activated. */
  onSelect: () => void;
  /** Render the item disabled (non-interactive). */
  disabled?: boolean;
  /** Style the item as a destructive action (danger ink). */
  danger?: boolean;
  /** Optional `data-testid` on the item for tests. */
  "data-testid"?: string;
}

export interface DataTableColumn<T> {
  /** Stable column identity. Used as the React key, the `sort.key`
   *  emitted on header click, the per-column `data-column` attr, the
   *  `columnFilters` key, and the TanStack `ColumnDef.id`. */
  key: string;
  /** Header content — rendered in `<th scope="col">` (inside a
   *  `<button>` when `sortable`). For an `actions` column, pass a
   *  visually-hidden label or omit. */
  header: ReactNode;
  /**
   * Cell renderer for a row. When provided it ALWAYS wins over `type`.
   * When omitted, the default renderer formats `accessor?.(row)` (else
   * `row[key]`) per `type`.
   */
  cell?: (row: T) => ReactNode;

  /**
   * Cell-type preset driving the default renderer. Ignored when `cell`
   * is provided. Default `"text"`.
   */
  type?: DataTableColumnType;
  /** Reads the raw value from a row for the default renderer + the
   *  managed sort/filter engine. Default `(row) => row[key]`. */
  accessor?: (row: T) => unknown;

  /** When true, the header becomes a real `<button>` that drives sort
   *  and carries `aria-sort`. Default false. An `actions` column is
   *  never sortable. */
  sortable?: boolean;
  /**
   * Comparator for the managed sort, ascending. Falls back to a default
   * (numbers numeric, dates by time, otherwise localeCompare; nullish
   * last). Ignored under `manualSorting`.
   */
  sortFn?: (a: T, b: T) => number;

  /**
   * Opt this column OUT of global search AND per-column filtering when
   * `false`. Default (undefined/true) keeps it searchable. An `actions`
   * column is never filterable.
   */
  filterable?: boolean;
  /**
   * Derives the searchable/filterable text for managed filtering. Falls
   * back to the string form of `accessor?.(row)` (else `row[key]`).
   */
  getFilterValue?: (row: T) => string;

  /** Horizontal alignment of this column's header and cells. Default
   *  `"start"`. `number`/`currency` default to `"end"`. */
  align?: DataTableAlign;
  /** A CSS length width hint applied via `<col>` (e.g. `"12rem"`). */
  width?: string;
  /** A CSS min-inline-size for the cell content box; the cell truncates
   *  with an ellipsis + native `title` past it. */
  minWidth?: string;
  /** Truncate the cell to a single line with an ellipsis + a `title`
   *  tooltip carrying the full text. Default false. */
  truncate?: boolean;

  /* type-specific options (all optional) */
  /** `number` formatting options for `Intl.NumberFormat`. */
  numberOptions?: Intl.NumberFormatOptions;
  /** ISO 4217 currency code for `currency`. Default `"USD"`. */
  currency?: string;
  /** Locale(s) for `number`/`currency`/`date` formatting. */
  locale?: string | string[];
  /** `date`/`datetime` options for `Intl.DateTimeFormat`. */
  dateOptions?: Intl.DateTimeFormatOptions;
  /** `boolean` text labels. Default `{ true: "Yes", false: "No" }`. */
  booleanLabels?: { true: string; false: string };
  /** `badge` value→intent map. Default `() => "neutral"`. */
  badgeIntent?: (value: unknown) => BadgeIntent;
  /** `link` href builder. When it returns null/empty, falls back to text. */
  href?: (row: T) => string | null | undefined;
  /** `link` target (e.g. `"_blank"`). */
  linkTarget?: string;
  /** `link` rel. Defaults to `"noreferrer"` when `linkTarget` is set. */
  linkRel?: string;
  /** `actions` item builder for the row's kebab Menu. */
  actions?: (row: T) => DataTableRowAction[];
}

/** The active sort: which column, which direction. `null` = unsorted. */
export interface DataTableSort {
  key: string;
  direction: "asc" | "desc";
}

/** A per-column text filter: `{ [columnKey]: query }`. */
export type DataTableColumnFilters = Record<string, string>;

/** Selection model. `"none"` (default) renders no selection column. */
export type DataTableSelectionMode = "none" | "single" | "multiple";

export interface DataTableProps<T>
  extends Omit<
    ComponentPropsWithoutRef<"div">,
    "children" | "onChange" | "title"
  > {
  /** Column definitions, left-to-right. */
  columns: DataTableColumn<T>[];
  /** Row data. In managed mode DataTable sorts/filters/paginates these;
   *  under the matching `manual…` flags it renders them as-given. */
  data: T[];
  /** Derives a stable string key for a row — the React key, the TanStack
   *  `getRowId`, AND the selection identity emitted in
   *  `onSelectionChange`. */
  rowKey: (row: T) => string;

  /** A real `<caption>` — the preferred accessible name. When omitted,
   *  `aria-label` is REQUIRED (dev-warn if neither). */
  caption?: ReactNode;

  /* ─── toolbar ─────────────────────────────────────────────────────── */
  /** Optional heading shown at the start of the toolbar. */
  title?: ReactNode;
  /** Extra toolbar content (buttons, filters) rendered at the end. */
  toolbar?: ReactNode;
  /** Show the global-search Input. Default: true when any column is
   *  searchable. Set false to hide it. */
  searchable?: boolean;
  /** Placeholder for the global-search Input. Default `"Search…"`. */
  searchPlaceholder?: string;

  /* ─── sort ────────────────────────────────────────────────────────── */
  /** Active sort. Controlled when provided; else internal (seeded by
   *  `defaultSort`). */
  sort?: DataTableSort | null;
  /** Initial sort for managed/uncontrolled mode. */
  defaultSort?: DataTableSort | null;
  /** Emitted with the NEXT sort when a sortable header is activated. */
  onSortChange?: (sort: DataTableSort) => void;
  /** Opt sorting INTO server-side: DataTable emits intent but does not
   *  reorder `data`. Default false. */
  manualSorting?: boolean;

  /* ─── global filter ──────────────────────────────────────────────── */
  /** Global search query. Controlled when provided; else internal. */
  globalFilter?: string;
  /** Initial global query for managed/uncontrolled mode. */
  defaultGlobalFilter?: string;
  /** Emitted when the global-search query changes. */
  onGlobalFilterChange?: (query: string) => void;

  /* ─── column filters ─────────────────────────────────────────────── */
  /** Show a per-column text-filter row under the header. Default: true
   *  when any filterable column declares it via this flag — pass
   *  explicitly to enable. */
  filterable?: boolean;
  /** Per-column filter queries. Controlled when provided; else internal. */
  columnFilters?: DataTableColumnFilters;
  /** Initial per-column filters for managed/uncontrolled mode. */
  defaultColumnFilters?: DataTableColumnFilters;
  /** Emitted with the NEXT full filter map when a column filter changes. */
  onColumnFiltersChange?: (filters: DataTableColumnFilters) => void;
  /** Opt filtering INTO server-side: DataTable emits intent but does not
   *  filter `data`. Default false. */
  manualFiltering?: boolean;

  /* ─── pagination ─────────────────────────────────────────────────── */
  /** Force-show or force-hide the pagination footer. Default: shown when
   *  the (filtered) row count exceeds the smallest page size. */
  paginated?: boolean;
  /** Current 1-based page. Controlled when provided; else internal. */
  page?: number;
  /** Initial page for managed/uncontrolled mode. Default 1. */
  defaultPage?: number;
  /** Page size. Controlled when provided; else internal. */
  pageSize?: number;
  /** Initial page size for managed/uncontrolled mode. Default 10. */
  defaultPageSize?: number;
  /** Sizes offered in the footer's "Rows per page" Select. Omit to hide
   *  the size control. */
  pageSizeOptions?: number[];
  /** Total (unpaginated) row count. REQUIRED under `manualPagination`;
   *  otherwise computed (managed) or `data.length`. */
  total?: number;
  /** Emitted with the requested 1-based page. */
  onPageChange?: (page: number) => void;
  /** Emitted with the chosen page size. */
  onPageSizeChange?: (size: number) => void;
  /** Opt pagination INTO server-side: DataTable renders `data` as the
   *  current page and does NOT slice. Needs `total`. Default false. */
  manualPagination?: boolean;

  /* ─── selection ──────────────────────────────────────────────────── */
  /** Selection model. `"none"` (default) renders no checkbox column. */
  selection?: DataTableSelectionMode;
  /** Selected row keys, CONTROLLED by the consumer. */
  selectedKeys?: string[];
  /** Emitted with the NEXT full set of selected keys after a toggle. */
  onSelectionChange?: (keys: string[]) => void;
  /** Builds the accessible label for a per-row selection checkbox.
   *  Default `Select row {n}` (1-based). */
  rowSelectionLabel?: (row: T, index: number) => string;

  /* ─── interaction + chrome ───────────────────────────────────────── */
  /** Click anywhere on a row (outside an interactive cell control).
   *  The row gets a pointer cursor but is NOT a button — see file header. */
  onRowClick?: (row: T) => void;

  /** Pin the header so rows scroll under it. The table must sit in a
   *  consumer-sized scroll container. */
  stickyHeader?: boolean;
  /** Row density. `"comfortable"` (default) or `"compact"`. */
  density?: DataTableDensity;

  /** When true, the body renders `loadingRowCount` Skeleton rows. */
  loading?: boolean;
  /** Number of Skeleton rows shown while `loading`. Default `5`. */
  loadingRowCount?: number;

  /** Empty slot for genuinely-no-data. Defaults to a generic EmptyState. */
  renderEmpty?: () => ReactNode;
  /** Empty slot when a filter/search produced zero rows. Defaults to a
   *  "no results match" EmptyState. */
  renderNoResults?: () => ReactNode;
  /** Error slot. A truthy return replaces the data rows (highest
   *  precedence). */
  renderError?: () => ReactNode;

  /** Root `data-slot`. Defaults to `"data-table"`. */
  "data-slot"?: string;
}

/* ─── column meta carried on the TanStack ColumnDef ───────────────────── */
interface DataTableColumnMeta<T> {
  column: DataTableColumn<T>;
  align: DataTableAlign;
}

/* ─── small generic helpers ──────────────────────────────────────────── */

/** Maps the column `align` to the resolved `data-align` value, applying
 *  the numeric end-align default. */
function resolveAlign(column: DataTableColumn<unknown>): DataTableAlign {
  if (column.align) return column.align;
  if (column.type === "number" || column.type === "currency") return "end";
  return "start";
}

/** Reads a column's raw value from a row. */
function readValue<T>(column: DataTableColumn<T>, row: T): unknown {
  if (column.accessor) return column.accessor(row);
  return (row as Record<string, unknown>)[column.key];
}

/** Derives the searchable text for managed filtering. */
function filterText<T>(column: DataTableColumn<T>, row: T): string {
  if (column.getFilterValue) return column.getFilterValue(row);
  const raw = readValue(column, row);
  if (raw == null) return "";
  if (raw instanceof Date) return raw.toISOString();
  return String(raw);
}

/** Coerces a `date`/`datetime` cell input into a `Date` (or null). */
function toDate(value: unknown): Date | null {
  if (value == null) return null;
  if (value instanceof Date)
    return Number.isNaN(value.getTime()) ? null : value;
  if (typeof value === "number") {
    const d = new Date(value);
    return Number.isNaN(d.getTime()) ? null : d;
  }
  if (typeof value === "string") {
    const d = new Date(value);
    return Number.isNaN(d.getTime()) ? null : d;
  }
  return null;
}

/** Default ascending comparator for a column's raw values: numbers
 *  numeric, dates by time, otherwise localeCompare; nullish sorts LAST. */
function defaultCompare(a: unknown, b: unknown): number {
  const aNull = a == null || a === "";
  const bNull = b == null || b === "";
  if (aNull && bNull) return 0;
  if (aNull) return 1; // nullish last
  if (bNull) return -1;
  if (typeof a === "number" && typeof b === "number") return a - b;
  if (a instanceof Date && b instanceof Date) return a.getTime() - b.getTime();
  if (typeof a === "boolean" && typeof b === "boolean")
    return a === b ? 0 : a ? 1 : -1;
  return String(a).localeCompare(String(b));
}

/* ─── sort affordance ─────────────────────────────────────────────────
 * Distinct glyph per state (shape ≠ color): the active direction is a
 * single accent chevron (ChevronUp / ChevronDown); an inactive sortable
 * column shows the muted up/down chevron pair (ChevronsUpDown). The glyph
 * is decorative — `aria-hidden` on the wrapper and a label-less `Icon`,
 * `aria-sort` on the <th> carries the meaning. The wrapper owns the size
 * (tracks the header text) and the color via `data-state`. */
function SortGlyph({
  state,
}: {
  state: "ascending" | "descending" | "none";
}) {
  const Glyph =
    state === "ascending"
      ? ChevronUp
      : state === "descending"
        ? ChevronDown
        : ChevronsUpDown;
  return (
    <span className="zs-data-table__sort-glyph" aria-hidden="true" data-state={state}>
      <Icon as={Glyph} className="zs-data-table__sort-glyph-icon" />
    </span>
  );
}

/* ─── boolean + kebab glyphs ──────────────────────────────────────────── */
function BoolGlyph({ value }: { value: boolean }) {
  return (
    <Icon
      as={value ? Check : Minus}
      size="sm"
      className="zs-data-table__bool-glyph"
      data-value={value ? "true" : "false"}
    />
  );
}

function KebabGlyph() {
  return <Icon as={MoreVertical} size="sm" />;
}

/* ─── RowActions kebab ─────────────────────────────────────────────────
 * A real `Menu` opened by an icon-only `Button`. The button carries
 * `aria-label="Row actions"`; Base UI's Menu owns focus + roving.
 * Each item is a `Menu.Item` whose `onClick` fires the action's
 * `onSelect` (Base UI closes the menu on activation). */
function RowActions({ actions }: { actions: DataTableRowAction[] }) {
  if (actions.length === 0) return null;
  return (
    <Menu>
      <Menu.Trigger
        render={
          <Button
            type="button"
            variant="plain"
            size="small"
            aria-label="Row actions"
            data-slot="data-table-row-actions-trigger"
            className="zs-data-table__row-actions-trigger"
          >
            <KebabGlyph />
          </Button>
        }
      />
      <Menu.Portal>
        <Menu.Popup align="end" data-slot="data-table-row-actions-menu">
          {actions.map((action, i) => (
            <Menu.Item
              key={i}
              disabled={action.disabled}
              data-danger={action.danger ? "" : undefined}
              data-testid={action["data-testid"]}
              className={action.danger ? "zs-data-table__row-action--danger" : undefined}
              onClick={() => action.onSelect()}
            >
              {action.label}
            </Menu.Item>
          ))}
        </Menu.Popup>
      </Menu.Portal>
    </Menu>
  );
}

/* ─── default cell renderer ───────────────────────────────────────────── */
function DefaultCell<T>({
  column,
  row,
}: {
  column: DataTableColumn<T>;
  row: T;
}) {
  const type = column.type ?? "text";

  if (type === "actions") {
    return <RowActions actions={column.actions?.(row) ?? []} />;
  }

  const value = readValue(column, row);

  switch (type) {
    case "number": {
      if (value == null || value === "") return null;
      const n = typeof value === "number" ? value : Number(value);
      if (!Number.isFinite(n)) return String(value);
      return new Intl.NumberFormat(column.locale, column.numberOptions).format(n);
    }
    case "currency": {
      if (value == null || value === "") return null;
      const n = typeof value === "number" ? value : Number(value);
      if (!Number.isFinite(n)) return String(value);
      return new Intl.NumberFormat(column.locale, {
        style: "currency",
        currency: column.currency ?? "USD",
        ...column.numberOptions,
      }).format(n);
    }
    case "date":
    case "datetime": {
      const d = toDate(value);
      if (d == null) return null;
      const opts: Intl.DateTimeFormatOptions =
        column.dateOptions ??
        (type === "datetime"
          ? { dateStyle: "medium", timeStyle: "short" }
          : { dateStyle: "medium" });
      return new Intl.DateTimeFormat(column.locale, opts).format(d);
    }
    case "boolean": {
      if (value == null) return null;
      const b = Boolean(value);
      const labels = column.booleanLabels ?? { true: "Yes", false: "No" };
      const label = b ? labels.true : labels.false;
      // Glyph + a visible OR SR-only text label — never color/glyph alone.
      return (
        <span className="zs-data-table__bool" data-value={b ? "true" : "false"}>
          <BoolGlyph value={b} />
          <span className="zs-data-table__bool-label">{label}</span>
        </span>
      );
    }
    case "badge": {
      if (value == null || value === "") return null;
      const intent = column.badgeIntent?.(value) ?? "neutral";
      return (
        <Badge intent={intent} size="sm">
          {String(value)}
        </Badge>
      );
    }
    case "link": {
      const href = column.href?.(row);
      const text = value == null ? "" : String(value);
      if (!href) return text || null;
      const target = column.linkTarget;
      const rel = column.linkRel ?? (target === "_blank" ? "noreferrer" : undefined);
      return (
        <a
          className="zs-data-table__link"
          href={href}
          target={target}
          rel={rel}
        >
          {text || href}
        </a>
      );
    }
    case "text":
    default:
      return value == null ? null : String(value);
  }
}

/* ─── controlled-or-internal state hook ───────────────────────────────── */
function useControllableState<V>(
  controlled: V | undefined,
  defaultValue: V,
  onChange: ((value: V) => void) | undefined,
): [V, (value: V) => void] {
  const [internal, setInternal] = useState<V>(defaultValue);
  const isControlled = controlled !== undefined;
  const value = isControlled ? (controlled as V) : internal;
  const set = useCallback(
    (next: V) => {
      if (!isControlled) setInternal(next);
      onChange?.(next);
    },
    [isControlled, onChange],
  );
  return [value, set];
}

function DataTableInner<T>(
  props: DataTableProps<T>,
  ref: React.Ref<HTMLTableElement>,
) {
  const {
    columns,
    data,
    rowKey,
    caption,

    title,
    toolbar,
    searchable,
    searchPlaceholder = "Search…",

    sort: sortProp,
    defaultSort = null,
    onSortChange,
    manualSorting = false,

    globalFilter: globalFilterProp,
    defaultGlobalFilter = "",
    onGlobalFilterChange,

    filterable = false,
    columnFilters: columnFiltersProp,
    defaultColumnFilters,
    onColumnFiltersChange,
    manualFiltering = false,

    paginated,
    page: pageProp,
    defaultPage = 1,
    pageSize: pageSizeProp,
    defaultPageSize = 10,
    pageSizeOptions,
    total: totalProp,
    onPageChange,
    onPageSizeChange,
    manualPagination = false,

    selection = "none",
    selectedKeys,
    onSelectionChange,
    rowSelectionLabel,

    onRowClick,
    stickyHeader = false,
    density = "comfortable",
    loading = false,
    loadingRowCount = 5,
    renderEmpty,
    renderNoResults,
    renderError,

    className,
    "data-slot": dataSlot = "data-table",
    "aria-label": ariaLabel,
    ...rest
  } = props;

  // Dev-mode validation: a table with no accessible name is a serious
  // a11y defect. Gated to non-production so bundlers DCE the branch.
  if (process.env.NODE_ENV !== "production") {
    if (caption == null && (ariaLabel == null || ariaLabel === "")) {
      // eslint-disable-next-line no-console
      console.warn(
        "DataTable has no accessible name: pass `caption` (preferred — a " +
          "visible <caption>) or `aria-label`. Without one, assistive tech " +
          "announces the table with no context.",
      );
    }
    if (manualPagination && totalProp == null) {
      // eslint-disable-next-line no-console
      console.warn(
        "DataTable: `manualPagination` needs a `total` (the unpaginated row " +
          "count) for the page math; falling back to `data.length`, which is " +
          "the size of the current page and will under-count the pages.",
      );
    }
  }

  /* ─── axis state (controlled-or-internal, OUR public contract) ─────
   * We keep our own controllable state for each axis so the public
   * value/`on…Change`/`default…` props behave exactly as documented;
   * the values are then PROJECTED into the TanStack `state` shape below.
   * TanStack is the transform engine, not the source of truth for our
   * public surface. */
  const [sort, setSort] = useControllableState<DataTableSort | null>(
    sortProp,
    defaultSort,
    onSortChange as ((v: DataTableSort | null) => void) | undefined,
  );
  const [globalFilter, setGlobalFilter] = useControllableState<string>(
    globalFilterProp,
    defaultGlobalFilter,
    onGlobalFilterChange,
  );
  const [columnFilters, setColumnFilters] =
    useControllableState<DataTableColumnFilters>(
      columnFiltersProp,
      defaultColumnFilters ?? {},
      onColumnFiltersChange,
    );
  const [page, setPage] = useControllableState<number>(
    pageProp,
    defaultPage,
    onPageChange,
  );
  const [pageSize, setPageSize] = useControllableState<number>(
    pageSizeProp,
    defaultPageSize,
    onPageSizeChange,
  );

  const hasSelection = selection !== "none";
  const isMultiple = selection === "multiple";

  /* ─── column lookups + a quick key→DataTableColumn map ────────────── */
  const columnByKey = useMemo(() => {
    const m = new Map<string, DataTableColumn<T>>();
    for (const c of columns) m.set(c.key, c);
    return m;
  }, [columns]);

  /* ─── DataTableColumn<T> → TanStack ColumnDef<T> ──────────────────── */
  const tableColumns = useMemo<ColumnDef<T, unknown>[]>(
    () =>
      columns.map((column) => {
        const align = resolveAlign(column as DataTableColumn<unknown>);
        const isActions = column.type === "actions";
        return {
          id: column.key,
          // accessorFn drives TanStack's getValue → our sort/filter engine.
          accessorFn: (row: T) => readValue(column, row),
          enableSorting: !!column.sortable && !isActions,
          enableColumnFilter: column.filterable !== false && !isActions,
          // Opt EVERY searchable column into global filtering explicitly.
          // TanStack's default `getColumnCanGlobalFilter` otherwise gates on
          // the FIRST row's value being string/number, so a table whose
          // searchable columns are all non-string-typed (date/boolean/object,
          // or a null first row) would silently skip global search. We pair
          // this with a table-level `getColumnCanGlobalFilter: () => true`
          // (below) so `enableGlobalFilter` is the SOLE gate and our custom
          // type-agnostic `globalFilterFn` always runs.
          enableGlobalFilter: column.filterable !== false && !isActions,
          // Our default-compare wins over TanStack's built-in alphanumeric.
          sortingFn: column.sortFn
            ? (rowA: Row<T>, rowB: Row<T>) =>
                column.sortFn!(rowA.original, rowB.original)
            : (rowA: Row<T>, rowB: Row<T>) =>
                defaultCompare(
                  readValue(column, rowA.original),
                  readValue(column, rowB.original),
                ),
          // Per-column filter: case-insensitive "contains" over filter text.
          filterFn: ((row: Row<T>, _id, value) => {
            const q = String(value ?? "").trim().toLowerCase();
            if (q === "") return true;
            return filterText(column, row.original).toLowerCase().includes(q);
          }) as FilterFn<T>,
          meta: { column, align } satisfies DataTableColumnMeta<T>,
        } satisfies ColumnDef<T, unknown>;
      }),
    [columns],
  );

  /* ─── project OUR state → TanStack state shapes ───────────────────── */
  const sortingState = useMemo<SortingState>(
    () => (sort ? [{ id: sort.key, desc: sort.direction === "desc" }] : []),
    [sort],
  );
  const columnFiltersState = useMemo<ColumnFiltersState>(
    () =>
      Object.entries(columnFilters)
        .filter(([id, q]) => {
          if (q == null || q.trim() === "") return false;
          // Only project filters for columns that EXIST and are eligible.
          // TanStack's `enableColumnFilter: false` does NOT stop the filtered
          // row model from applying a column's `filterFn` once a
          // columnFilters entry is present, so a stray filter on a
          // `filterable: false` / `actions` column (or an unknown key) would
          // still narrow the rows. The bespoke baseline ignored those — match it.
          const col = columnByKey.get(id);
          return col != null && col.filterable !== false && col.type !== "actions";
        })
        .map(([id, value]) => ({ id, value })),
    [columnFilters, columnByKey],
  );
  const safePageSize = Math.max(1, pageSize);
  const paginationState = useMemo<PaginationState>(
    () => ({ pageIndex: Math.max(0, page - 1), pageSize: safePageSize }),
    [page, safePageSize],
  );
  const rowSelectionState = useMemo<RowSelectionState>(() => {
    const sel: RowSelectionState = {};
    for (const k of selectedKeys ?? []) sel[k] = true;
    return sel;
  }, [selectedKeys]);

  /* ─── global-filter fn: ANY searchable column's text contains query ─ */
  const globalFilterFn = useCallback<FilterFn<T>>(
    (row, _columnId, value) => {
      const q = String(value ?? "").trim().toLowerCase();
      if (q === "") return true;
      return columns.some((column) => {
        if (column.filterable === false || column.type === "actions")
          return false;
        return filterText(column, row.original).toLowerCase().includes(q);
      });
    },
    [columns],
  );

  /* ─── the headless TanStack engine ────────────────────────────────── */
  const table = useReactTable<T>({
    data,
    columns: tableColumns,
    getRowId: (row) => rowKey(row),
    state: {
      sorting: sortingState,
      columnFilters: columnFiltersState,
      globalFilter,
      pagination: paginationState,
      rowSelection: rowSelectionState,
    },
    manualSorting,
    manualFiltering,
    manualPagination,
    // Under manual pagination we report the consumer `total` so
    // getPageCount() reflects the SERVER total, not the local page.
    ...(manualPagination
      ? { rowCount: totalProp ?? data.length }
      : manualFiltering
        ? { rowCount: totalProp ?? data.length }
        : {}),
    enableRowSelection: hasSelection,
    enableMultiRowSelection: isMultiple,
    globalFilterFn,
    // Let the per-column `enableGlobalFilter` be the SOLE gate on which
    // columns are globally filterable. TanStack's default sniffs the first
    // row's value type (string/number) here, which would skip global search
    // for a table whose searchable columns are all non-string-typed; our
    // `globalFilterFn` is type-agnostic, so we override the type sniff to true.
    getColumnCanGlobalFilter: () => true,
    // We drive state ourselves; these no-op so TanStack never tries to own
    // it (our handlers below call setSort/setPage/etc. + fire on…Change).
    onSortingChange: () => {},
    onColumnFiltersChange: () => {},
    onGlobalFilterChange: () => {},
    onPaginationChange: () => {},
    onRowSelectionChange: () => {},
    getCoreRowModel: getCoreRowModel(),
    // Managed axes get their row model; manual axes set manual* above so
    // TanStack passes the rows through that model untransformed.
    getSortedRowModel: getSortedRowModel(),
    getFilteredRowModel: getFilteredRowModel(),
    getPaginationRowModel: getPaginationRowModel(),
    autoResetPageIndex: false,
  });

  // The unpaginated count the footer reports. Managed: post-filter size
  // (TanStack's filtered model — independent of the pagination slice).
  // Manual pagination/filtering: consumer `total` (or data.length fallback).
  const total = manualPagination
    ? totalProp ?? data.length
    : manualFiltering
      ? totalProp ?? data.length
      : table.getFilteredRowModel().rows.length;

  const pageCount = Math.max(1, Math.ceil(total / safePageSize));
  // The CLAMPED current page — the single source of truth for BOTH the
  // body slice and the <Pagination> footer, so they can never disagree.
  const currentPage = Math.min(Math.max(page, 1), pageCount);

  // Decide whether to show the footer. Default: any time the row count
  // exceeds the smallest available page size. Explicit `paginated` wins.
  const smallestSize = pageSizeOptions?.length
    ? Math.min(...pageSizeOptions)
    : safePageSize;
  const autoPaginate = total > smallestSize;
  const showPagination = paginated ?? autoPaginate;

  /* ─── the rows actually rendered ──────────────────────────────────────
   * We slice the body ourselves from the PRE-pagination row model rather
   * than reading `table.getRowModel().rows`, for two correctness reasons:
   *   (1) paginated={false} (managed): TanStack's getRowModel ALWAYS runs
   *       through getPaginationRowModel, so it would render only the first
   *       `pageSize` rows even with the footer hidden. The baseline showed
   *       every sorted/filtered row. We render the full pre-pagination model.
   *   (2) page agreement: when the footer IS shown we slice by the CLAMPED
   *       `currentPage`, so an out-of-range/stale controlled `page` (or a
   *       page count that shrank under a filter) can't leave the body empty
   *       while the footer shows the clamped last page.
   * Manual pagination/filtering bypass the managed slice: TanStack passes
   * `data` through untransformed (manual* flags), so we render it as-given. */
  const visibleRowObjs: Row<T>[] =
    manualPagination || !showPagination
      ? table.getPrePaginationRowModel().rows
      : table
          .getPrePaginationRowModel()
          .rows.slice(
            (currentPage - 1) * safePageSize,
            (currentPage - 1) * safePageSize + safePageSize,
          );
  const visibleRows = visibleRowObjs.map((r) => r.original);
  const visibleRowKeys = visibleRowObjs.map((r) => r.id);

  /* ─── selection (derived over the VISIBLE rows) ───────────────────── */
  const selectedSet = useMemo(() => new Set(selectedKeys ?? []), [selectedKeys]);
  const selectedVisibleCount = visibleRowKeys.filter((k) =>
    selectedSet.has(k),
  ).length;
  const allSelected =
    visibleRowKeys.length > 0 && selectedVisibleCount === visibleRowKeys.length;
  const someSelected = selectedVisibleCount > 0 && !allSelected;

  const totalColumns = columns.length + (hasSelection ? 1 : 0);

  /* ─── handlers ────────────────────────────────────────────────────── */
  // Single-column sort toggle: a fresh column starts asc; re-click toggles.
  const handleHeaderSort = (column: DataTableColumn<T>) => {
    if (!column.sortable || column.type === "actions") return;
    const next: DataTableSort =
      sort && sort.key === column.key
        ? {
            key: column.key,
            direction: sort.direction === "asc" ? "desc" : "asc",
          }
        : { key: column.key, direction: "asc" };
    setSort(next);
    // Sorting changes the row order; reset to page 1 in managed pagination.
    if (!manualPagination) setPage(1);
  };

  const handleGlobalFilter = (value: string) => {
    setGlobalFilter(value);
    if (!manualPagination) setPage(1);
  };

  const handleColumnFilter = (key: string, value: string) => {
    setColumnFilters({ ...columnFilters, [key]: value });
    if (!manualPagination) setPage(1);
  };

  // Select-all / clear-all touch ONLY the visible rows; off-page keys
  // (keys in `selectedKeys` with no current row) survive untouched.
  const handleSelectAll = () => {
    if (!onSelectionChange) return;
    const current = selectedKeys ?? [];
    if (allSelected) {
      const visible = new Set(visibleRowKeys);
      onSelectionChange(current.filter((k) => !visible.has(k)));
    } else {
      const missing = visibleRowKeys.filter((k) => !selectedSet.has(k));
      onSelectionChange([...current, ...missing]);
    }
  };

  const handleRowSelect = (key: string) => {
    if (!onSelectionChange) return;
    if (selection === "single") {
      onSelectionChange(selectedSet.has(key) ? [] : [key]);
      return;
    }
    if (selectedSet.has(key)) {
      onSelectionChange((selectedKeys ?? []).filter((k) => k !== key));
    } else {
      onSelectionChange([...(selectedKeys ?? []), key]);
    }
  };

  /* ─── state precedence: error > loading > empty > data ────────────── */
  const errorNode = renderError?.();
  const showError = errorNode != null && errorNode !== false;
  const showLoading = !showError && loading;
  const showEmpty = !showError && !showLoading && visibleRows.length === 0;
  // Distinguish "no data at all" from "filtered/searched to nothing" so
  // the empty slot can show the right message.
  const hasActiveFilter =
    !manualFiltering &&
    (globalFilter.trim() !== "" ||
      Object.values(columnFilters).some((q) => q != null && q.trim() !== ""));
  const emptyDueToFilter =
    showEmpty && (hasActiveFilter || (manualFiltering && data.length === 0));

  /* ─── toolbar visibility ──────────────────────────────────────────── */
  const anySearchable = columns.some(
    (c) => c.filterable !== false && c.type !== "actions",
  );
  const showSearch = searchable ?? anySearchable;
  const showToolbar = title != null || toolbar != null || showSearch;

  // Per-column filter row: shown when `filterable` is on AND at least one
  // column is eligible.
  const filterableColumns = columns.filter(
    (c) => c.filterable !== false && c.type !== "actions",
  );
  const showColumnFilters = filterable && filterableColumns.length > 0;

  const tableClassName = classnames(
    "zs-data-table",
    `zs-data-table--${density}`,
    stickyHeader ? "zs-data-table--sticky" : null,
  );

  const fullSpanRow = (content: ReactNode, slot: string) => (
    <tr data-slot={slot}>
      <td className="zs-data-table__state-cell" colSpan={totalColumns}>
        {content}
      </td>
    </tr>
  );

  const cellStyle = (column: DataTableColumn<T>): CSSProperties | undefined =>
    column.minWidth != null
      ? ({ "--zs-data-table-cell-min": column.minWidth } as CSSProperties)
      : undefined;

  // aria-sort for a header column: absent for non-sortable/actions; else
  // "ascending"/"descending"/"none" from the TanStack sort direction.
  const ariaSortFor = (
    column: DataTableColumn<T>,
  ): "ascending" | "descending" | "none" | undefined => {
    if (!column.sortable || column.type === "actions") return undefined;
    if (sort && sort.key === column.key) {
      return sort.direction === "asc" ? "ascending" : "descending";
    }
    return "none";
  };

  return (
    <div
      {...rest}
      data-slot={dataSlot}
      data-density={density}
      className={classnames("zs-data-table-shell", className)}
    >
      {showToolbar ? (
        <Cluster
          gap={3}
          align="center"
          justify="between"
          className="zs-data-table__toolbar"
          data-slot="data-table-toolbar"
        >
          <Cluster gap={3} align="center" className="zs-data-table__toolbar-start">
            {title != null ? (
              <div className="zs-data-table__title" data-slot="data-table-title">
                {title}
              </div>
            ) : null}
          </Cluster>
          <Cluster gap={2} align="center" className="zs-data-table__toolbar-end">
            {showSearch ? (
              <Input
                type="search"
                size="sm"
                aria-label="Search table"
                placeholder={searchPlaceholder}
                value={globalFilter}
                onChange={(e) => handleGlobalFilter(e.currentTarget.value)}
                className="zs-data-table__search"
                data-slot="data-table-search"
                data-testid="data-table-search"
              />
            ) : null}
            {toolbar}
          </Cluster>
        </Cluster>
      ) : null}

      <div
        className="zs-data-table__scroll"
        data-slot="data-table-scroll"
        data-sticky={stickyHeader ? "" : undefined}
      >
        <table
          ref={ref}
          aria-label={ariaLabel}
          className={tableClassName}
          data-slot="data-table-table"
        >
          {caption != null ? (
            <caption className="zs-data-table__caption" data-slot="data-table-caption">
              {caption}
            </caption>
          ) : null}

          {columns.some((c) => c.width != null) || hasSelection ? (
            <colgroup>
              {hasSelection ? <col className="zs-data-table__select-col" /> : null}
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
                      disabled={visibleRowKeys.length === 0}
                      data-testid="data-table-select-all"
                    />
                  ) : (
                    <span className="zs-visually-hidden">Select</span>
                  )}
                </th>
              ) : null}
              {columns.map((column) => {
                const sortState = ariaSortFor(column);
                const align = resolveAlign(column as DataTableColumn<unknown>);
                const isActions = column.type === "actions";
                return (
                  <th
                    key={column.key}
                    scope="col"
                    data-slot="data-table-column-header"
                    data-column={column.key}
                    data-align={align}
                    data-type={column.type}
                    aria-sort={sortState}
                    className={classnames(
                      "zs-data-table__th",
                      isActions ? "zs-data-table__th--actions" : null,
                    )}
                  >
                    {column.sortable && !isActions ? (
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
                    ) : isActions ? (
                      <span className="zs-visually-hidden">{column.header ?? "Actions"}</span>
                    ) : (
                      <span className="zs-data-table__header-label">
                        {column.header}
                      </span>
                    )}
                  </th>
                );
              })}
            </tr>

            {showColumnFilters ? (
              <tr data-slot="data-table-filter-row" className="zs-data-table__filter-row">
                {hasSelection ? (
                  <th className="zs-data-table__select-cell" aria-hidden="true" />
                ) : null}
                {columns.map((column) => {
                  const eligible =
                    column.filterable !== false && column.type !== "actions";
                  const label =
                    typeof column.header === "string"
                      ? `Filter ${column.header}`
                      : `Filter ${column.key}`;
                  return (
                    <th
                      key={column.key}
                      className="zs-data-table__filter-cell"
                      data-column={column.key}
                      // An ineligible column renders no filter input → the
                      // <th> would be an empty header (axe `empty-table-header`).
                      // Hide it from AT, matching the select-cell filter <th>.
                      aria-hidden={eligible ? undefined : "true"}
                    >
                      {eligible ? (
                        <Input
                          type="search"
                          size="sm"
                          variant="filled"
                          aria-label={label}
                          placeholder="Filter…"
                          value={columnFilters[column.key] ?? ""}
                          onChange={(e) =>
                            handleColumnFilter(column.key, e.currentTarget.value)
                          }
                          className="zs-data-table__filter-input"
                          data-testid={`data-table-filter-${column.key}`}
                        />
                      ) : null}
                    </th>
                  );
                })}
              </tr>
            ) : null}
          </thead>

          <tbody data-slot="data-table-body">
            {showError
              ? fullSpanRow(errorNode, "data-table-error")
              : showLoading
                ? Array.from({ length: Math.max(0, loadingRowCount) }, (_, i) => (
                    <tr key={`skeleton-${i}`} data-slot="data-table-loading-row">
                      {hasSelection ? (
                        <td className="zs-data-table__select-cell">
                          <Skeleton variant="text" width="1rem" />
                        </td>
                      ) : null}
                      {columns.map((column) => (
                        <td
                          key={column.key}
                          data-align={resolveAlign(column as DataTableColumn<unknown>)}
                          className="zs-data-table__td"
                        >
                          <Skeleton variant="text" />
                        </td>
                      ))}
                    </tr>
                  ))
                : showEmpty
                  ? fullSpanRow(
                      emptyDueToFilter
                        ? renderNoResults?.() ?? (
                            <EmptyState
                              title="No results"
                              description="No rows match your search or filters."
                            />
                          )
                        : renderEmpty?.() ?? (
                            <EmptyState
                              title="No data"
                              description="There's nothing to show here yet."
                            />
                          ),
                      "data-table-empty",
                    )
                  : visibleRowObjs.map((rowObj, index) => {
                      const row = rowObj.original;
                      const key = rowObj.id;
                      const selected = selectedSet.has(key);
                      return (
                        <tr
                          key={key}
                          data-slot="data-table-row"
                          data-selected={selected ? "" : undefined}
                          data-clickable={onRowClick ? "" : undefined}
                          aria-selected={hasSelection ? selected : undefined}
                          onClick={
                            onRowClick
                              ? (event) => {
                                  // Don't hijack clicks that landed on an
                                  // interactive control inside the row
                                  // (checkbox, link, the actions kebab).
                                  const target = event.target as HTMLElement;
                                  if (
                                    target.closest(
                                      "button, a, input, [role='menuitem'], label",
                                    )
                                  )
                                    return;
                                  onRowClick(row);
                                }
                              : undefined
                          }
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
                          ) : (
                            null
                          )}
                          {columns.map((column) => {
                            const align = resolveAlign(
                              column as DataTableColumn<unknown>,
                            );
                            const content = column.cell ? (
                              column.cell(row)
                            ) : (
                              <DefaultCell column={column} row={row} />
                            );
                            const isActions = column.type === "actions";
                            return (
                              <td
                                key={column.key}
                                data-slot="data-table-cell"
                                data-column={column.key}
                                data-align={align}
                                data-type={column.type}
                                style={cellStyle(column)}
                                className={classnames(
                                  "zs-data-table__td",
                                  column.truncate ? "zs-data-table__td--truncate" : null,
                                  isActions ? "zs-data-table__td--actions" : null,
                                )}
                              >
                                {column.truncate ? (
                                  <span
                                    className="zs-data-table__truncate"
                                    title={
                                      typeof content === "string" ? content : undefined
                                    }
                                  >
                                    {content}
                                  </span>
                                ) : (
                                  content
                                )}
                              </td>
                            );
                          })}
                        </tr>
                      );
                    })}
          </tbody>
        </table>
      </div>

      {showPagination ? (
        <div className="zs-data-table__footer" data-slot="data-table-footer">
          <Pagination
            size="sm"
            page={currentPage}
            pageSize={safePageSize}
            total={total}
            onPageChange={(next) => {
              // OUR page state is the source of truth; TanStack reads it
              // back through `state.pagination` (pageIndex = page-1).
              setPage(next);
            }}
            pageSizeOptions={pageSizeOptions}
            onPageSizeChange={(size) => {
              setPageSize(size);
              if (!manualPagination) setPage(1);
            }}
            data-slot="data-table-pagination"
            data-testid="data-table-pagination"
          />
        </div>
      ) : null}
    </div>
  );
}

/**
 * DataTable — a presentational data grid with an opt-in managed engine
 * (driven by @tanstack/react-table), generic over the row type `T`.
 *
 * @example Managed (default — sorts/filters/paginates `data` itself)
 * ```tsx
 * <DataTable
 *   caption="Users"
 *   columns={[
 *     { key: "name", header: "Name", sortable: true },
 *     { key: "spend", header: "Spend", type: "currency", sortable: true },
 *     { key: "active", header: "Active", type: "boolean" },
 *   ]}
 *   data={users}
 *   rowKey={(u) => u.id}
 * />
 * ```
 *
 * @example Server-side (fully manual — the v1 presentational contract)
 * ```tsx
 * <DataTable
 *   aria-label="Users"
 *   columns={columns}
 *   data={pageRows}
 *   rowKey={(u) => u.id}
 *   manualSorting manualFiltering manualPagination
 *   sort={sort} onSortChange={refetchSorted}
 *   globalFilter={q} onGlobalFilterChange={refetchFiltered}
 *   page={page} pageSize={size} total={totalCount}
 *   onPageChange={setPage}
 * />
 * ```
 *
 * `forwardRef` loses generic inference, so we re-assert the generic
 * signature on the exported binding — calling `<DataTable<User> …/>`
 * keeps `cell`/`rowKey`/`accessor`/`sortFn` typed against `User`. The
 * forwarded `ref` lands on the inner `<table>` element.
 */
export const DataTable = forwardRef(DataTableInner) as <T>(
  props: DataTableProps<T> & { ref?: React.Ref<HTMLTableElement> },
) => ReturnType<typeof DataTableInner>;
