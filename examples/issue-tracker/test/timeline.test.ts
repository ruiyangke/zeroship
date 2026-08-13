import { describe, expect, it } from "vitest";

import { buildTimeline } from "../src/components/bug-detail/timeline";

/**
 * Unit-level because the ordering rule is arithmetic, not layout.
 *
 * The bug this exists for: the comparator applied its kind tiebreak to EVERY
 * tie, including comment-vs-comment, so it claimed both "a before b" and "b
 * before a" for the same pair. An engine handed an inconsistent comparator may
 * produce any order, and it did -- the description sorted after the first
 * reply, on a page whose entire argument is that the description comes first.
 * A browser test would have caught the symptom; this catches the rule.
 */

type C = Parameters<typeof buildTimeline>[0][number];
type A = Parameters<typeof buildTimeline>[1][number];

const comment = (id: string, at: number, commentNumber: number): C =>
  ({ id, created_at: at, commentNumber, body: id, authorId: "u1" }) as unknown as C;

const activity = (at: number, fieldName: string, actorId = "u1", oldValue = "x"): A =>
  ({ changedAt: at, fieldName, actorId, oldValue, newValue: "y" }) as unknown as A;

describe("bug timeline", () => {
  it("keeps comments in their own order when timestamps tie", () => {
    // Identical timestamps are the norm, not an edge case: a bug's
    // description and its creation events are written in one transaction.
    const items = buildTimeline(
      [comment("c0", 1000, 0), comment("c1", 1000, 1), comment("c2", 1000, 2)],
      [],
    );
    expect(items.map((i) => (i.kind === "comment" ? i.comment.id : "event"))).toEqual([
      "c0",
      "c1",
      "c2",
    ]);
  });

  it("orders by time, with a comment ahead of an event that ties with it", () => {
    const items = buildTimeline(
      [comment("c0", 1000, 0), comment("c1", 3000, 1)],
      [activity(3000, "priority"), activity(5000, "status")],
    );
    expect(items.map((i) => (i.kind === "comment" ? i.comment.id : "event:" + i.changes[0].fieldName))).toEqual([
      "c0",
      "c1",
      "event:priority",
      "event:status",
    ]);
  });

  it("groups one edit's fields together and drops creation noise", () => {
    const items = buildTimeline(
      [comment("c0", 1000, 0)],
      [
        // Creation: from nothing, at the moment of the description.
        activity(1000, "summary", "u1", ""),
        activity(1000, "status", "u1", ""),
        // A real edit, two fields, same second.
        activity(9000, "priority"),
        activity(9200, "severity"),
      ],
    );
    const events = items.filter((i) => i.kind === "event");
    expect(events, "creation is not narrated as a change").toHaveLength(1);
    expect(events[0].kind === "event" && events[0].changes.map((c) => c.fieldName)).toEqual([
      "priority",
      "severity",
    ]);
  });

  it("leaves bookkeeping fields out of the conversation", () => {
    const items = buildTimeline([comment("c0", 1000, 0)], [activity(9000, "commentCount")]);
    expect(items.filter((i) => i.kind === "event")).toHaveLength(0);
  });
});
