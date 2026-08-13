// Renders bugs.get's `activities` array in order, with reference values
// resolved to names for display -- no client-side history re-derivation, per
// this app's fidelity requirement. (Other panels use the
// same array to reconstruct current keyword/flag state, which is a
// different thing: this tab is the raw log, unmodified.)
import { Avatar } from "@zeroship/ui";

import { EmptyState } from "../StateViews";
import type { Activity } from "../types";
import { personName, type PeopleMap } from "./people";

function name(value: string | null | undefined, labels: Record<string, string>) {
  if (value === null || value === undefined || value === "") {
    return <span className="dim">unset</span>;
  }
  return labels[value] ?? value;
}

function formatDate(ms: number): string {
  return new Date(ms).toLocaleString();
}

function timeAgo(ms: number): string {
  const seconds = Math.round((Date.now() - ms) / 1000);
  if (seconds < 60) return "just now";
  const units: [number, string][] = [
    [60, "minute"],
    [3600, "hour"],
    [86400, "day"],
    [604800, "week"],
    [2592000, "month"],
    [31536000, "year"],
  ];
  let unit = units[0];
  for (const candidate of units) if (seconds >= candidate[0]) unit = candidate;
  const value = Math.floor(seconds / unit[0]);
  return value + " " + unit[1] + (value === 1 ? "" : "s") + " ago";
}

function initials(who: string): string {
  const parts = who.trim().split(/\s+/).filter(Boolean);
  if (parts.length === 0) return "?";
  return (parts[0][0] + (parts[1]?.[0] ?? "")).toUpperCase();
}

/** Field names as the log stores them, in the words the UI uses elsewhere. */
const FIELD_LABELS: Record<string, string> = {
  bug_group: "Group restriction",
  assigneeId: "Assignee",
  qaContactId: "QA contact",
  // The one the generic splitter got visibly wrong: "reporterId" came out as
  // "Reporter Id", which is a property name wearing a label's clothes.
  reporterId: "Reporter",
  productId: "Product",
  componentId: "Component",
  versionId: "Version",
  milestoneId: "Milestone",
  duplicateOfId: "Duplicate of",
  opSys: "OS",
  whiteboard: "Whiteboard",
  isConfirmed: "Confirmed",
  voteCount: "Votes",
  commentCount: "Comments",
  workTimeMinutes: "Work time",
};

function fieldLabel(field: string): string {
  if (FIELD_LABELS[field]) return FIELD_LABELS[field];
  // camelCase and snake_case both become words: "workTimeMinutes" reads as
  // "Work time minutes" rather than being printed as a property name.
  const spaced = field.replace(/_/g, " ").replace(/([a-z])([A-Z])/g, "$1 $2");
  return spaced.charAt(0).toUpperCase() + spaced.slice(1);
}

/**
 * One edit, however many fields it touched.
 *
 * The log stores a row per FIELD, so closing a bug writes three of them --
 * status, resolution and whoever it was assigned to -- with the same actor and
 * the same timestamp. As four table columns that read as three unrelated
 * events. Grouping them is a presentation choice only: the rows, their order
 * and their values are exactly what the server recorded, which is the fidelity
 * rule this panel has always been held to.
 */
type Event = {
  actorId: string;
  changedAt: number;
  changes: Activity[];
};

function groupIntoEvents(activities: readonly Activity[]): Event[] {
  const events: Event[] = [];
  for (const activity of activities) {
    const last = events[events.length - 1];
    // Same person, same second. A second is coarse enough to catch one
    // transaction and fine enough not to merge two deliberate edits.
    const sameMoment =
      last &&
      last.actorId === activity.actorId &&
      Math.abs(last.changedAt - activity.changedAt) < 1000;
    if (sameMoment) last.changes.push(activity);
    else events.push({ actorId: activity.actorId, changedAt: activity.changedAt, changes: [activity] });
  }
  return events;
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
  people = {},
}: {
  activities: readonly Activity[];
  labels?: Record<string, string>;
  people?: PeopleMap;
}) {
  if (activities.length === 0) {
    return <EmptyState title="No activity yet." hint="Every field change will appear here." />;
  }

  const events = groupIntoEvents(activities);

  return (
    <ol className="history-timeline">
      {events.map((event, index) => {
        const who = personName(event.actorId, people, "Someone");
        return (
          <li key={index} className="history-event">
            <Avatar size="sm" fallback={initials(who)} aria-hidden="true" />
            <div className="history-event-body">
              <p className="history-event-head">
                <span className="history-actor">{who}</span>
                <span className="history-when" title={formatDate(event.changedAt)}>
                  {timeAgo(event.changedAt)}
                </span>
              </p>
              <ul className="history-changes">
                {event.changes.map((change, changeIndex) => (
                  <li key={changeIndex}>
                    <span className="history-field">{fieldLabel(change.fieldName)}</span>
                    <span className="history-from">{name(change.oldValue, labels)}</span>
                    <span className="history-arrow" aria-hidden="true">
                      to
                    </span>
                    <span className="history-to">{name(change.newValue, labels)}</span>
                  </li>
                ))}
              </ul>
            </div>
          </li>
        );
      })}
    </ol>
  );
}
