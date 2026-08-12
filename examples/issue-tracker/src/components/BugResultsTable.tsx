// Shared results table used by the bug list, advanced/quick search results,
// and the "my dashboard" sections. Presentation only -- callers own data
// fetching, filtering, and (for the bug list) which columns are visible.
import { PriorityBadge, ResolutionBadge, SeverityBadge, StatusBadge } from "./Badges";
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
 * A bug id short enough for a table cell and still unique.
 *
 * Takes the END of the body, not the start. A typed_id is a UUIDv7 in base62,
 * and UUIDv7 is time-ordered: the leading characters encode the timestamp, so
 * every id minted in the same window shares them. Keeping the first six kept
 * exactly the half that is identical across rows and threw away the random
 * half -- measured against the dev database, 61 bugs rendered as 11 distinct
 * ids, so 50 of them shared a displayed id with another bug. Bugs filed
 * seconds apart, which is what a test run or an import produces, collided
 * every time.
 *
 * That makes the id column worse than absent in the one job it has: you cannot
 * tell two rows apart, quote one to a colleague, or match a row against a
 * screenshot. `title` carries the full id, but hover does not survive reading,
 * copying or printing.
 *
 * The leading ellipsis is deliberate. The detail page shows the id in full, so
 * a silently shortened form that looks whole would not match it.
 *
 * Six base62 characters is 62^6, or 35.7 bits. That is not a uniqueness
 * guarantee, and the number worth knowing is the onset rather than the median:
 * by the birthday bound some pair collides with 1% probability at ~34,000 bugs
 * and 50% at ~281,000. Thirty-four thousand is an ordinary size for a tracker,
 * so treat this as "short enough to read, unique enough to scan" and not as an
 * identifier. The anchor carries the full id for that reason.
 */
export function shortId(id: string): string {
  const parts = id.split("_");
  if (parts.length < 2 || parts[1].length <= 6) return id;
  return `${parts[0]}_...${parts[1].slice(-6)}`;
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
  productsById,
  usersById,
  allowInlineStatus = false,
  onStatusChanged,
}: {
  bugs: readonly Bug[];
  columns: readonly BugColumnKey[];
  // Required, not optional. These were optional, and three of the four call
  // sites simply left them out -- the table then rendered `prod_034607nk...`
  // and `user_0345pl8p...` in the product and assignee columns with nothing
  // failing. An omitted lookup is indistinguishable from an unresolved one at
  // runtime, so the compiler is the only thing that can tell them apart.
  // `useBugLookups()` supplies both.
  productsById: Record<string, string>;
  usersById: Record<string, string>;
  allowInlineStatus?: boolean;
  onStatusChanged?: (bug: Bug) => void;
}) {
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
                      {shortId(bug.id)}
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
                  {key === "product" && (productsById?.[bug.productId] ?? bug.productId)}
                  {key === "summary" && (
                    <a href={`#/bugs/${bug.id}`} className="bug-summary-link">
                      {bug.summary}
                    </a>
                  )}
                  {key === "assignee" &&
                    (bug.assigneeId ? usersById?.[bug.assigneeId] ?? bug.assigneeId : "--")}
                  {key === "reporter" && (usersById?.[bug.reporterId] ?? bug.reporterId)}
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
