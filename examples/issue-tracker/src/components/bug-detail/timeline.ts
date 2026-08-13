import type { Activity, Comment } from "../types";

/**
 * One timeline: comments and field changes, in the order they happened.
 *
 * GitHub and Linear both put activity INLINE with the conversation, and they
 * are right. This app had it behind a second tab, so "Alice raised this to P1"
 * -- often the most consequential thing on a bug -- was somewhere you had to
 * go and look, while a reply saying "on it" sat in plain view. A reader asking
 * "what happened here" wants both, in order.
 *
 * The History tab stays. This is the story; that is the log, and a log that
 * shows every field of the creation event is worth keeping separately.
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

/**
 * Merge comments and activity into one ordered list.
 *
 * Consecutive changes by the same person within a second are one edit -- the
 * log stores a row per field, so closing a bug writes three. The creation
 * event is dropped entirely: every field goes from unset at once, which as a
 * timeline entry reads as fourteen changes nobody made.
 */
export function buildTimeline(
  comments: readonly Comment[],
  activities: readonly Activity[],
): TimelineItem[] {
  const firstCommentAt = comments.length > 0 ? comments[0].created_at : 0;

  const events: TimelineEvent[] = [];
  for (const activity of activities) {
    if (NOT_WORTH_SAYING.has(activity.fieldName)) continue;
    // Creation: everything arrives from nothing, at the moment of the first
    // comment. Identified by having no previous value AND landing with the
    // description, so a later field set from empty still shows.
    if (!activity.oldValue && Math.abs(activity.changedAt - firstCommentAt) < 1000) continue;

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
