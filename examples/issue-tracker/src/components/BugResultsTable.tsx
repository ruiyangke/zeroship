// Shared results table used by the bug list, advanced/quick search results,
// and the "my dashboard" sections. Presentation only -- callers own data
// fetching, filtering, and (for the bug list) which columns are visible.
import { DataTable, type DataTableColumn, type DataTableSort } from "@zeroship/ui";

import { PriorityBadge, ResolutionBadge, SeverityBadge, StatusBadge } from "./Badges";
import type { Bug } from "./types";
import { useBugLookups } from "./useBugLookups";

export type BugColumnKey =
  | "id"
  | "status"
  | "resolution"
  | "severity"
  | "priority"
  | "product"
  | "summary"
  | "assignee"
  | "reporter"
  | "updated";

export const ALL_BUG_COLUMNS: { key: BugColumnKey; label: string }[] = [
  { key: "id", label: "ID" },
  { key: "status", label: "Status" },
  { key: "resolution", label: "Resolution" },
  { key: "severity", label: "Severity" },
  { key: "priority", label: "Priority" },
  { key: "product", label: "Product" },
  { key: "summary", label: "Summary" },
  { key: "assignee", label: "Assignee" },
  { key: "reporter", label: "Reporter" },
  { key: "updated", label: "Updated" },
];

/**
 * The identifier a person reads: PARSER-12.
 *
 * Falls back to the UUID when the product key has not resolved yet, rather
 * than rendering a bare number: "12" on its own belongs to no product.
 */
function bugLabel(bug: Bug, productKeysById: Record<string, string>): string {
  const key = productKeysById[bug.productId];
  return key ? `${key}-${bug.number}` : bug.id;
}

function formatDate(ms: number): string {
  return new Date(ms).toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/**
 * Built on the design system's DataTable rather than a hand-rolled `<table>`.
 *
 * The status column is a BADGE, not a select. Every row used to carry an
 * always-live dropdown, so a screen of twelve bugs was twelve form controls
 * and the eye had nowhere to rest -- the control competed with the data it
 * described. Changing status is a deliberate act and belongs on the bug page,
 * which is also the only place that can ask for the resolution a close
 * requires.
 */
/**
 * Which columns the SERVER can order by. A header is only clickable when the
 * server has a matching sort key -- offering to sort by a column it cannot
 * order would either silently do nothing or quietly sort one page in
 * isolation, which is worse than not offering it.
 */
const SERVER_SORTABLE: Partial<Record<BugColumnKey, string>> = {
  id: "created_at",
  status: "status",
  severity: "severity",
  priority: "priority",
  updated: "updated_at",
};

export function BugResultsTable({
  bugs,
  columns,
  caption,
  sort,
  onSortChange,
}: {
  bugs: readonly Bug[];
  columns: readonly BugColumnKey[];
  caption?: string;
  sort?: DataTableSort | null;
  onSortChange?: (sort: DataTableSort) => void;
}) {
  // Resolved HERE rather than passed in. These were props, and three of the
  // four call sites left them out, so those tables printed raw ids with
  // nothing failing. The table is the one place that always knows which ids
  // are on screen.
  const { productsById, productKeysById, usersById } = useBugLookups(bugs);

  const byKey: Record<BugColumnKey, DataTableColumn<Bug>> = {
    id: {
      key: "id",
      header: "ID",
      cell: (bug) => (
        <a href={`#/bugs/${bug.id}`} className="bug-link" title={bug.id}>
          {bugLabel(bug, productKeysById)}
        </a>
      ),
    },
    status: { key: "status", header: "Status", cell: (bug) => <StatusBadge status={bug.status} /> },
    resolution: {
      key: "resolution",
      header: "Resolution",
      cell: (bug) => <ResolutionBadge resolution={bug.resolution ?? null} />,
    },
    severity: {
      key: "severity",
      header: "Severity",
      cell: (bug) => <SeverityBadge severity={bug.severity} />,
    },
    priority: {
      key: "priority",
      header: "Priority",
      cell: (bug) => <PriorityBadge priority={bug.priority} />,
    },
    product: {
      key: "product",
      header: "Product",
      cell: (bug) => productsById[bug.productId] ?? bug.productId,
    },
    summary: {
      key: "summary",
      header: "Summary",
      cell: (bug) => (
        <a href={`#/bugs/${bug.id}`} className="bug-summary-link">
          {bug.summary}
        </a>
      ),
    },
    assignee: {
      key: "assignee",
      header: "Assignee",
      cell: (bug) => (bug.assigneeId ? usersById[bug.assigneeId] ?? bug.assigneeId : "--"),
    },
    reporter: {
      key: "reporter",
      header: "Reporter",
      cell: (bug) => usersById[bug.reporterId] ?? bug.reporterId,
    },
    updated: { key: "updated", header: "Updated", cell: (bug) => formatDate(bug.updated_at) },
  };

  return (
    <DataTable
      columns={columns.map((key) => ({
        ...byKey[key],
        sortable: onSortChange ? Boolean(SERVER_SORTABLE[key]) : false,
      }))}
      data={[...bugs]}
      rowKey={(bug) => bug.id}
      // The SERVER filters, sorts and pages -- searchBugs takes text, sortBy
      // and limit/offset. Leaving the managed engine on gave the page two
      // search boxes and two paginators disagreeing with each other: the
      // built-in one showing "1-10 of 25" over a set the server had already
      // narrowed to 25 of hundreds.
      searchable={false}
      paginated={false}
      manualSorting
      manualFiltering
      manualPagination
      sort={sort ?? null}
      onSortChange={onSortChange}
      // A caption or an aria-label is REQUIRED -- the component dev-warns and
      // the table is left unnamed for a screen reader without one.
      aria-label={caption ?? "Bugs"}
    />
  );
}
