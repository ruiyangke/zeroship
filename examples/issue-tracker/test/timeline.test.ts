import { describe, expect, it } from "vitest";

import { buildTimeline } from "../src/components/issue-detail/timeline";
import { displayValue } from "../src/components/issue-detail/activity";

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
        activity(1000, "productId", "u1", ""),
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

describe("logged values", () => {
  it("shows the filename, not the typed id it is stored with", () => {
    // The server stores `id:filename` on purpose so the log still points at
    // the file after a rename. The id is the durable half; it is not the
    // readable half.
    expect(displayValue("atta_0346OnsDKnCnUWHpzUByI3:upload-trace.log")).toBe("upload-trace.log");
  });

  it("leaves a value that merely contains a colon alone", () => {
    // The stripping is anchored to a LEADING typed id. A URL or a whiteboard
    // note has colons too, and losing everything before the first one would
    // be a worse bug than the one this fixes.
    expect(displayValue("https://example.com/spec")).toBe("https://example.com/spec");
    expect(displayValue("blocked: waiting on infra")).toBe("blocked: waiting on infra");
  });

  it("treats an absent value as empty rather than printing null", () => {
    expect(displayValue(null)).toBe("");
    expect(displayValue(undefined)).toBe("");
  });
});

describe("creation filtering", () => {
  it("keeps a real first-value change that merely happens early", () => {
    // An attachment uploaded seconds after filing has no previous value and
    // lands near the description, which is exactly the shape the creation
    // filter looks for. Dropping it loses a real event from the story.
    const items = buildTimeline(
      [comment("c0", 1000, 0)],
      [
        activity(1000, "summary", "u1", ""),
        activity(1000, "status", "u1", ""),
        activity(1000, "severity", "u1", ""),
        activity(1000, "priority", "u1", ""),
        activity(1000, "productId", "u1", ""),
        activity(1400, "attachment", "u1", ""),
      ],
    );
    const events = items.filter((i) => i.kind === "event");
    expect(
      events.flatMap((e) => (e.kind === "event" ? e.changes.map((c) => c.fieldName) : [])),
      "the attachment survives, the creation fields do not",
    ).toEqual(["attachment"]);
  });
});
