import { ISSUE_CREATION_FIELDS } from "../../lib/changes";
import type { Activity, Comment } from "../types";

/**
 * One timeline: comments and field changes, in the order they happened.
 *
 * GitHub and Linear both put activity INLINE with the conversation, and they
 * are right. This app had it behind a second tab, so "Alice raised this to P1"
 * -- often the most consequential thing on an issue -- was somewhere you had to
 * go and look, while a reply saying "on it" sat in plain view. A reader asking
 * "what happened here" wants both, in order.
 *
 * The History tab stays. This is the story; that is the log, and a log that
 * shows every field of the creation event is worth keeping separately.
 *
 * The ordering claim above is checked by `e2e/timeline-order.spec.ts`, which
 * was written after this comment had gone untested for some time -- it is the
 * argument for the two-tab split, so it should not rest on assertion.
 *
 * ONE CAVEAT, and it will waste your afternoon otherwise: `at` is a
 * millisecond stamp, so a comment and a field change issued back to back over
 * RPC can share it, and the tiebreak below then decides on KIND rather than on
 * time. Drive a page that way and the events appear to bunch at the end of the
 * thread, which looks exactly like a broken sort and is not one. A person
 * cannot type two things in the same millisecond; a script can.
 */

export type TimelineEvent = {
  kind: "event";
  at: number;
  actorId: string;
  changes: Activity[];
};

export type TimelineComment = { kind: "comment"; at: number; comment: Comment };

export type TimelineItem = TimelineComment | TimelineEvent;

/**
 * Fields whose changes are NOISE in a conversation.
 *
 * Comment and vote counts move whenever anyone types; the description is
 * already rendered as comment 0. Showing them inline would bury the changes
 * that matter under bookkeeping -- and unlike the History tab, this view is
 * allowed to be a summary, because the full log is one tab away and says so.
 */
const NOT_WORTH_SAYING = new Set([
  "commentCount",
  "voteCount",
  "description",
  "isConfirmed",
]);

const CREATION_FIELDS: ReadonlySet<string> = new Set(ISSUE_CREATION_FIELDS);

/**
 * Is this activity part of filing the issue, rather than something someone did?
 *
 * All three clauses matter. The field is one the server writes at creation;
 * it went from nothing; and it happened with the description. An assignee set
 * a week later is a real event even though creation also writes `assigneeId`.
 *
 * The rule used to be the last two clauses only, which describes creation but
 * does not ONLY describe creation: a file attached moments after filing has no
 * previous value and lands next to the description, so it matched, and the
 * upload silently vanished from the timeline. The missing clause was the one
 * the server could answer outright -- it declares the fields it writes.
 */
function isCreationField(activity: Activity, firstCommentAt: number): boolean {
  return (
    CREATION_FIELDS.has(activity.fieldName) &&
    !activity.oldValue &&
    Math.abs(activity.changedAt - firstCommentAt) < 1000
  );
}

/**
 * Merge comments and activity into one ordered list.
 *
 * Consecutive changes by the same person within a second are one edit -- the
 * log stores a row per field, so closing an issue writes three. Filing is dropped
 * entirely (see isCreationField): it writes nineteen fields at once, which as a
 * timeline entry reads as nineteen changes nobody made.
 */
export function buildTimeline(
  comments: readonly Comment[],
  activities: readonly Activity[],
): TimelineItem[] {
  const firstCommentAt = comments.length > 0 ? comments[0].created_at : 0;

  const events: TimelineEvent[] = [];
  for (const activity of activities) {
    if (NOT_WORTH_SAYING.has(activity.fieldName)) continue;
    if (isCreationField(activity, firstCommentAt)) continue;

    const last = events[events.length - 1];
    if (
      last &&
      last.actorId === activity.actorId &&
      Math.abs(last.at - activity.changedAt) < 1000
    ) {
      last.changes.push(activity);
      continue;
    }
    events.push({
      kind: "event",
      at: activity.changedAt,
      actorId: activity.actorId,
      changes: [activity],
    });
  }

  const items: TimelineItem[] = [
    ...comments.map((comment) => ({ kind: "comment" as const, at: comment.created_at, comment })),
    ...events,
  ];
  // Stable by time; on a tie a comment precedes an event, so a change made
  // while commenting reads as following the comment that explains it.
  //
  // The kind tiebreak applies ONLY when the kinds differ. Returning -1 for
  // every comment-vs-comment tie made the comparator inconsistent -- it said
  // both "a before b" and "b before a" for the same pair -- and the engine is
  // entitled to produce any order from that. It did: the description sorted
  // after the first reply, on a page whose whole argument is that the
  // description comes first.
  return items.sort((a, b) => {
    if (a.at !== b.at) return a.at - b.at;
    if (a.kind === b.kind) return 0;
    return a.kind === "comment" ? -1 : 1;
  });
}
