// Renders issues.get's `activities` array in order, with reference values
// resolved to names for display -- no client-side history re-derivation, per
// this app's fidelity requirement. (Other panels use the
// same array to reconstruct current keyword/flag state, which is a
// different thing: this tab is the raw log, unmodified.)
import { Avatar } from "@zeroship/ui";

import { Muted } from "../AppPrimitives";
import { EmptyState } from "../StateViews";
import type { Activity } from "../types";
import { personName, type PeopleMap } from "./people";
import { displayValue, fieldLabel } from "./activity";
import { TimelineBody, TimelineList, TimelineRow } from "./TimelinePrimitives";

function name(value: string | null | undefined, labels: Record<string, string>) {
  if (value === null || value === undefined || value === "") {
    return <Muted>unset</Muted>;
  }
  return labels[value] ?? displayValue(value);
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

/**
 * One edit, however many fields it touched.
 *
 * The log stores a row per FIELD, so closing an issue writes three of them --
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

function HistoryChangeList({
  changes,
  labels,
}: {
  changes: readonly Activity[];
  labels: Record<string, string>;
}) {
  return (
    <ul className="history-changes m-0 flex list-none flex-col gap-1 p-0">
      {changes.map((change, changeIndex) => (
        <li key={changeIndex} className="flex flex-wrap items-baseline gap-2 text-md">
          <span className="min-w-28 font-semibold text-ink-secondary">
            {fieldLabel(change.fieldName)}
          </span>
          <span className="text-ink-muted line-through">{name(change.oldValue, labels)}</span>
          <span className="text-sm text-ink-muted" aria-hidden="true">
            to
          </span>
          <span className="font-medium text-ink">{name(change.newValue, labels)}</span>
        </li>
      ))}
    </ul>
  );
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
    <TimelineList kind="history">
      {events.map((event, index) => {
        const who = personName(event.actorId, people, "Someone");
        /**
         * Filing an issue writes a row per field, so the first event is fourteen
         * lines of "unset to X" and every real change starts below them. It
         * is FOLDED, not dropped: the rows are all still here, one click away,
         * because a history that quietly omits things is worse than a long
         * one. Detected rather than assumed -- the first event whose changes
         * all came from nothing is the creation.
         */
        const isCreation = index === 0 && event.changes.every((c) => !c.oldValue);
        return (
          <TimelineRow key={index} kind="history">
            <Avatar size="sm" fallback={initials(who)} aria-hidden="true" />
            <TimelineBody>
              <p className="m-0 mb-1 flex items-baseline gap-2 text-sm text-ink-muted">
                <span className="history-actor text-md font-semibold text-ink">{who}</span>
                <span title={formatDate(event.changedAt)}>
                  {isCreation ? "filed this issue" : null} {timeAgo(event.changedAt)}
                </span>
              </p>
              {isCreation ? (
                <details className="history-creation [&>summary]:cursor-pointer [&>summary]:list-outside [&>summary]:text-base [&>summary]:text-ink-muted [&>summary:hover]:text-accent [&[open]>summary]:mb-2">
                  <summary>
                    {event.changes.length} fields set on creation
                  </summary>
                  <HistoryChangeList changes={event.changes} labels={labels} />
                </details>
              ) : (
                <HistoryChangeList changes={event.changes} labels={labels} />
              )}
            </TimelineBody>
          </TimelineRow>
        );
      })}
    </TimelineList>
  );
}
