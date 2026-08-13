// Renders bugs.get's `activities` array in order, with reference values
// resolved to names for display -- no client-side history re-derivation, per
// this app's fidelity requirement. (Other panels use the
// same array to reconstruct current keyword/flag state, which is a
// different thing: this tab is the raw log, unmodified.)
import { EmptyState } from "../StateViews";
import type { Activity } from "../types";

function name(value: string | null | undefined, labels: Record<string, string>) {
  if (value === null || value === undefined || value === "") {
    return <span className="dim">--</span>;
  }
  return labels[value] ?? value;
}

function formatDate(ms: number): string {
  return new Date(ms).toLocaleString();
}

/**
 * `labels` maps an id to the name it stands for.
 *
 * Several fields record a REFERENCE -- product, component, version,
 * milestone, assignee -- so the log stored and printed
 * `prod_0346Dt31y2ejoQKqPytS6X` where a product name belongs. Naming the
 * value is not re-deriving the history: the row, its field and its order are
 * still exactly what the server recorded, and an id with no mapping is still
 * printed as itself rather than hidden.
 */
export function HistoryPanel({
  activities,
  labels = {},
}: {
  activities: readonly Activity[];
  labels?: Record<string, string>;
}) {
  if (activities.length === 0) {
    return <EmptyState title="No activity yet." hint="Every field change will appear here." />;
  }
  return (
    <div className="table-wrap">
      <table className="history-table">
        <thead>
          <tr>
            <th>When</th>
            <th>Field</th>
            <th>Old value</th>
            <th>New value</th>
          </tr>
        </thead>
        <tbody>
          {activities.map((activity, index) => (
            <tr key={index}>
              <td>{formatDate(activity.changedAt)}</td>
              <td>{activity.fieldName}</td>
              <td>{name(activity.oldValue, labels)}</td>
              <td>{name(activity.newValue, labels)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
