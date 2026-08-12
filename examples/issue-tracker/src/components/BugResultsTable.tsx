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

function shortId(id: string): string {
  const parts = id.split("_");
  return parts.length > 1 ? `${parts[0]}_${parts[1].slice(0, 6)}` : id;
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
  productsById?: Record<string, string>;
  usersById?: Record<string, string>;
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
