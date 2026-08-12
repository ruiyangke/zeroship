// Shared results table used by the bug list, advanced/quick search results,
// and the "my dashboard" sections. Presentation only -- callers own data
// fetching, filtering, and (for the bug list) which columns are visible.
import { PriorityBadge, ResolutionBadge, SeverityBadge, StatusBadge } from "./Badges";
import { useBugLookups } from "./useBugLookups";
import { InlineStatusEdit } from "./InlineStatusEdit";
import type { Bug } from "./types";

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
 * This replaced a truncated UUID. That form took the FIRST six characters of
 * the id body, and a typed_id is a time-ordered UUIDv7 in base62, so the
 * leading characters encode the timestamp: measured against the dev database,
 * 61 bugs rendered as 11 distinct strings. Taking the tail instead made them
 * unique but not meaningful -- `bug_...4ewAeC` is no more quotable than the
 * whole thing.
 *
 * A per-product sequence is both. It is short, ordered, sayable out loud, and
 * survives being copied into a commit message, which is the actual job of the
 * column.
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

export function BugResultsTable({
  bugs,
  columns,
  allowInlineStatus = false,
  onStatusChanged,
}: {
  bugs: readonly Bug[];
  columns: readonly BugColumnKey[];
  allowInlineStatus?: boolean;
  onStatusChanged?: (bug: Bug) => void;
}) {
  // Resolved HERE rather than passed in. These were props, and three of the
  // four call sites left them out, so those tables printed raw ids with
  // nothing failing. Making them required only moved the problem: the pages
  // call the hook where the rows are not loaded yet. The table is the one
  // place that always knows which ids are on screen.
  const { productsById, productKeysById, usersById } = useBugLookups(bugs);
  return (
    <div className="table-wrap">
      <table className="bug-table">
        <thead>
          <tr>
            {columns.map((key) => (
              <th key={key}>{ALL_BUG_COLUMNS.find((c) => c.key === key)?.label ?? key}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {bugs.map((bug) => (
            <tr key={bug.id}>
              {columns.map((key) => (
                <td key={key} data-col={key}>
                  {key === "id" && (
                    <a href={`#/bugs/${bug.id}`} className="bug-link" title={bug.id}>
                      {bugLabel(bug, productKeysById)}
                    </a>
                  )}
                  {key === "status" &&
                    (allowInlineStatus ? (
                      <InlineStatusEdit bug={bug} onChanged={onStatusChanged} />
                    ) : (
                      <StatusBadge status={bug.status} />
                    ))}
                  {key === "resolution" && <ResolutionBadge resolution={bug.resolution ?? null} />}
                  {key === "severity" && <SeverityBadge severity={bug.severity} />}
                  {key === "priority" && <PriorityBadge priority={bug.priority} />}
                  {key === "product" && (productsById[bug.productId] ?? bug.productId)}
                  {key === "summary" && (
                    <a href={`#/bugs/${bug.id}`} className="bug-summary-link">
                      {bug.summary}
                    </a>
                  )}
                  {key === "assignee" &&
                    (bug.assigneeId ? usersById[bug.assigneeId] ?? bug.assigneeId : "--")}
                  {key === "reporter" && (usersById[bug.reporterId] ?? bug.reporterId)}
                  {key === "updated" && formatDate(bug.updated_at)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
