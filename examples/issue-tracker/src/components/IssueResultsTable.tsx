// Shared results table used by the issue list, advanced/quick search results,
// and the "my dashboard" sections. Presentation only -- callers own data
// fetching, filtering, and (for the issue list) which columns are visible.
import type { ReactNode } from "react";
import { Link } from "react-router-dom";

import { EmptyState } from "../ui/EmptyState";
import { Skeleton } from "../ui/Skeleton";
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
 * The status column is a BADGE, not a select. Every row used to carry an
 * always-live dropdown, so a screen of twelve issues was twelve form controls
 * and the eye had nowhere to rest -- the control competed with the data it
 * described. Changing status is a deliberate act and belongs on the issue page,
 * which is also the only place that can ask for the resolution a close
 * requires.
 *
 * The table itself is deliberately plain HTML. Filtering, sorting and paging
 * all happen on the server, so a client table engine would own no row math at
 * this only call site; the useful contract is the header button beside the
 * column it orders and the rows exactly as the server returned them.
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

export interface IssueTableSort {
  key: string;
  direction: "asc" | "desc";
}

interface IssueTableColumn {
  key: IssueColumnKey;
  header: string;
  cell: (issue: Issue) => ReactNode;
}

function SortGlyph({ state }: { state: "ascending" | "descending" | "none" }) {
  const path =
    state === "ascending"
      ? ["m18 15-6-6-6 6"]
      : state === "descending"
        ? ["m6 9 6 6 6-6"]
        : ["m7 15 5 5 5-5", "m7 9 5-5 5 5"];

  return (
    <span
      aria-hidden="true"
      className={`inline-flex size-3 flex-none items-center justify-center ${
        state === "none" ? "text-ink-disabled" : "text-accent-strong"
      }`}
    >
      <svg
        aria-hidden="true"
        className="size-full"
        fill="none"
        focusable="false"
        stroke="currentColor"
        strokeLinecap="round"
        strokeLinejoin="round"
        strokeWidth="2"
        viewBox="0 0 24 24"
      >
        {path.map((d) => (
          <path key={d} d={d} />
        ))}
      </svg>
    </span>
  );
}

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
  sort?: IssueTableSort | null;
  onSortChange?: (sort: IssueTableSort) => void;
}) {
  // Resolved HERE rather than passed in. These were props, and three of the
  // four call sites left them out, so those tables printed raw ids with
  // nothing failing. The table is the one place that always knows which ids
  // are on screen.
  const { productsById, productKeysById, usersById } = useIssueLookups(issues);

  const byKey: Record<IssueColumnKey, IssueTableColumn> = {
    id: {
      key: "id",
      header: "ID",
      cell: (issue) => (
        <Link
          to={`/issues/${issue.id}`}
          className="issue-link inline-flex whitespace-nowrap rounded-sm font-mono text-base focus-visible:focus-ring-tight"
          title={issue.id}
        >
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
      cell: (issue) => (
        <span className="block max-w-52 truncate">
          {productsById[issue.productId] ?? "--"}
        </span>
      ),
    },
    summary: {
      key: "summary",
      header: "Summary",
      cell: (issue) => (
        <Link
          to={`/issues/${issue.id}`}
          className="block max-w-80 truncate rounded-sm text-ink! hover:text-accent-strong! focus-visible:focus-ring-tight"
        >
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
  // Skeleton rows are right when there is nothing yet and wrong for a refetch,
  // which is the common case here: every
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
    <div
      aria-busy={refetching || undefined}
      className={
        refetching
          ? "opacity-55 transition-opacity duration-fast ease-out"
          : undefined
      }
    >
      <div className="flex w-full min-w-0 flex-col overflow-hidden rounded border border-line bg-surface text-ink">
        <div className="min-h-0 w-full min-w-0 overflow-auto bg-surface">
          <table
            aria-label={caption ?? "Issues"}
            className="w-full border-separate border-spacing-0 bg-surface text-base leading-snug text-ink"
          >
            <thead>
              <tr>
                {columns.map((key) => {
                  const column = byKey[key];
                  const sortable = onSortChange != null && Boolean(SERVER_SORTABLE[key]);
                  const sortState = sortable
                    ? sort?.key === key
                      ? sort.direction === "asc"
                        ? "ascending"
                        : "descending"
                      : "none"
                    : undefined;

                  return (
                    <th
                      key={key}
                      scope="col"
                      data-column={key}
                      aria-sort={sortState}
                      className="h-8 whitespace-nowrap border-b border-line-strong bg-surface-sunken px-3 py-0 text-left text-xs font-semibold tracking-wide text-ink-muted"
                    >
                      {sortable ? (
                        <button
                          type="button"
                          className="inline-flex min-h-6 w-full cursor-pointer items-center justify-start gap-1 rounded border-0 bg-transparent p-0 text-inherit hover:text-ink"
                          onClick={() =>
                            onSortChange({
                              key,
                              direction:
                                sort?.key === key && sort.direction === "asc" ? "desc" : "asc",
                            })
                          }
                        >
                          <span className="min-w-0 truncate">{column.header}</span>
                          <SortGlyph state={sortState ?? "none"} />
                        </button>
                      ) : (
                        <span className="min-w-0 truncate">{column.header}</span>
                      )}
                    </th>
                  );
                })}
              </tr>
            </thead>
            <tbody className="bg-surface">
              {firstLoad
                ? Array.from({ length: 5 }, (_, rowIndex) => (
                    <tr key={`skeleton-${rowIndex}`} className="bg-surface">
                      {columns.map((key) => (
                        <td
                          key={key}
                          className={`h-8 border-b border-line px-3 py-0 align-middle${
                            rowIndex === 4 ? " border-b-0!" : ""
                          }`}
                        >
                          <Skeleton />
                        </td>
                      ))}
                    </tr>
                  ))
                : issues.length === 0
                  ? (
                      <tr className="bg-surface">
                        <td colSpan={columns.length} className="px-3 py-12">
                          <EmptyState
                            title="No data"
                            description="There's nothing to show here yet."
                          />
                        </td>
                      </tr>
                    )
                  : issues.map((issue, rowIndex) => (
                      <tr
                        key={issue.id}
                        className="bg-surface transition-colors duration-fast ease-out hover:bg-surface-hover focus-within:bg-surface-selected motion-reduce:transition-none"
                      >
                        {columns.map((key) => (
                          <td
                            key={key}
                            data-column={key}
                            className={`h-8 whitespace-nowrap border-b border-line px-3 py-0 align-middle tabular-nums${
                              rowIndex === issues.length - 1 ? " border-b-0!" : ""
                            }`}
                          >
                            {byKey[key].cell(issue)}
                          </td>
                        ))}
                      </tr>
                    ))}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
