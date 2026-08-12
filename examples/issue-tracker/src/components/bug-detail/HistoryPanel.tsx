// Renders bugs.get's `activities` array verbatim -- no client-side history
// re-derivation, per this app's fidelity requirement. (Other panels use the
// same array to reconstruct current keyword/flag state, which is a
// different thing: this tab is the raw log, unmodified.)
import { EmptyState } from "../StateViews";
import type { Activity } from "../types";

function formatDate(ms: number): string {
  return new Date(ms).toLocaleString();
}

export function HistoryPanel({ activities }: { activities: readonly Activity[] }) {
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
              <td>{activity.oldValue ?? <span className="dim">--</span>}</td>
              <td>{activity.newValue ?? <span className="dim">--</span>}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
