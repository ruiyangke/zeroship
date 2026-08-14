// Shared results table used by the issue list, advanced/quick search results,
// and the "my dashboard" sections. Presentation only -- callers own data
// fetching, filtering, and (for the issue list) which columns are visible.
import { DataTable, type DataTableColumn, type DataTableSort } from "@zeroship/ui";
import { Link } from "react-router-dom";

import { KindBadge, PriorityBadge, ResolutionBadge, SeverityBadge, StatusBadge } from "./Badges";
import type { Issue } from "./types";
import { useIssueLookups } from "./useIssueLookups";

export type IssueColumnKey =
  | "id"
  | "status"
  | "resolution"
  | "kind"
  | "severity"
  | "priority"
  | "product"
  | "summary"
  | "assignee"
  | "reporter"
  | "updated";

export const ALL_ISSUE_COLUMNS: { key: IssueColumnKey; label: string }[] = [
  { key: "id", label: "ID" },
  { key: "status", label: "Status" },
  { key: "resolution", label: "Resolution" },
  { key: "kind", label: "Kind" },
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
function issueLabel(issue: Issue, productKeysById: Record<string, string>): string {
  // The row's OWN key first. searchIssues carries productKey, so the label is
  // known the moment the row is, and the lookup map is only a fallback for
  // callers whose rows predate that field.
  //
  // The fallback used to be the full UUID, which meant every data change
  // rendered a column of long ids until a second request resolved the keys and
  // then snapped to the short form. A dash is a placeholder that holds its
  // place; a UUID is a different, much wider string pretending to be an answer.
  const key = issue.productKey ?? productKeysById[issue.productId];
  return key ? `${key}-${issue.number}` : "--";
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
 * always-live dropdown, so a screen of twelve issues was twelve form controls
 * and the eye had nowhere to rest -- the control competed with the data it
 * described. Changing status is a deliberate act and belongs on the issue page,
 * which is also the only place that can ask for the resolution a close
 * requires.
 */
/**
 * Which columns the SERVER can order by. A header is only clickable when the
 * server has a matching sort key -- offering to sort by a column it cannot
 * order would either silently do nothing or quietly sort one page in
 * isolation, which is worse than not offering it.
 */
const SERVER_SORTABLE: Partial<Record<IssueColumnKey, string>> = {
  id: "created_at",
  status: "status",
  severity: "severity",
  priority: "priority",
  updated: "updated_at",
};

export function IssueResultsTable({
  issues,
  columns,
  caption,
  loading = false,
  sort,
  onSortChange,
}: {
  issues: readonly Issue[];
  columns: readonly IssueColumnKey[];
  caption?: string;
  /** Marks the table busy IN PLACE during a refetch, rather than the caller
   *  unmounting it and leaving a hole where the rows were. */
  loading?: boolean;
  sort?: DataTableSort | null;
  onSortChange?: (sort: DataTableSort) => void;
}) {
  // Resolved HERE rather than passed in. These were props, and three of the
  // four call sites left them out, so those tables printed raw ids with
  // nothing failing. The table is the one place that always knows which ids
  // are on screen.
  const { productsById, productKeysById, usersById } = useIssueLookups(issues);

  const byKey: Record<IssueColumnKey, DataTableColumn<Issue>> = {
    id: {
      key: "id",
      header: "ID",
      cell: (issue) => (
        <Link to={`/issues/${issue.id}`} className="issue-link" title={issue.id}>
          {issueLabel(issue, productKeysById)}
        </Link>
      ),
    },
    status: { key: "status", header: "Status", cell: (issue) => <StatusBadge status={issue.status} /> },
    resolution: {
      key: "resolution",
      header: "Resolution",
      cell: (issue) => <ResolutionBadge resolution={issue.resolution ?? null} />,
    },
    kind: {
      key: "kind",
      header: "Kind",
      cell: (issue) => <KindBadge kind={issue.kind} />,
    },
    severity: {
      key: "severity",
      header: "Severity",
      cell: (issue) => <SeverityBadge severity={issue.severity} />,
    },
    priority: {
      key: "priority",
      header: "Priority",
      cell: (issue) => <PriorityBadge priority={issue.priority} />,
    },
    product: {
      key: "product",
      header: "Product",
      // A dash, not the id, while the lookup is in flight. The map arrives on
      // a second request, so falling back to the raw value meant every row
      // flashed prod_0346En6o4bMYRunhVm0mmX before the names landed -- brief
      // when idle, long enough to read on a loaded machine.
      cell: (issue) => productsById[issue.productId] ?? "--",
    },
    summary: {
      key: "summary",
      header: "Summary",
      cell: (issue) => (
        <Link to={`/issues/${issue.id}`} className="issue-summary-link">
          {issue.summary}
        </Link>
      ),
    },
    assignee: {
      key: "assignee",
      header: "Assignee",
      cell: (issue) => (issue.assigneeId ? usersById[issue.assigneeId] ?? "--" : "--"),
    },
    reporter: {
      key: "reporter",
      header: "Reporter",
      cell: (issue) => usersById[issue.reporterId] ?? "--",
    },
    updated: { key: "updated", header: "Updated", cell: (issue) => formatDate(issue.updated_at) },
  };

  // Two different states, deliberately not conflated.
  //
  // DataTable resolves state as error > loading > empty > data, so its
  // `loading` REPLACES the rows with skeletons. That is right when there is
  // nothing yet and wrong for a refetch, which is the common case here: every
  // filter change, sort and page turn reloads, and swapping 25 rows for 5
  // skeletons is the same hole the unmounting used to leave, just shorter.
  //
  // So skeletons only when there is nothing to keep. With rows on screen the
  // table stays exactly as it is and the wrapper is marked aria-busy, which
  // announces the update to a screen reader and dims it without moving
  // anything.
  const firstLoad = loading && issues.length === 0;
  const refetching = loading && issues.length > 0;

  return (
    <div aria-busy={refetching || undefined} className={refetching ? "is-refetching" : undefined}>
    <DataTable
      columns={columns.map((key) => ({
        ...byKey[key],
        sortable: onSortChange ? Boolean(SERVER_SORTABLE[key]) : false,
      }))}
      data={[...issues]}
      rowKey={(issue) => issue.id}
      // The SERVER filters, sorts and pages -- searchIssues takes text, sortBy
      // and limit/offset. Leaving the managed engine on gave the page two
      // search boxes and two paginators disagreeing with each other: the
      // built-in one showing "1-10 of 25" over a set the server had already
      // narrowed to 25 of hundreds.
      searchable={false}
      paginated={false}
      manualSorting
      manualFiltering
      manualPagination
      loading={firstLoad}
      sort={sort ?? null}
      onSortChange={onSortChange}
      // A caption or an aria-label is REQUIRED -- the component dev-warns and
      // the table is left unnamed for a screen reader without one.
      aria-label={caption ?? "Issues"}
    />
    </div>
  );
}
