import { describe, expect, it } from "vitest";
import {
  BUG_STATUSES,
  STATUS_TRANSITIONS,
  InvalidBugTransitionError,
  markDuplicateBugState,
  reopenBugState,
  resolveBugState,
  transitionBugState,
  type BugState,
  type BugStatus,
} from "../src/lib/workflow";

const stateFor = (status: BugStatus): BugState => ({
  status,
  resolution:
    status === "RESOLVED" || status === "VERIFIED" || status === "CLOSED"
      ? "FIXED"
      : null,
});

describe("bug status workflow", () => {
  it("accepts every edge in the transition table", () => {
    for (const from of BUG_STATUSES) {
      for (const to of STATUS_TRANSITIONS[from]) {
        const result = transitionBugState(stateFor(from), {
          status: to,
          ...(to === "RESOLVED" ? { resolution: "FIXED" as const } : {}),
        });

        expect(result.status).toBe(to);
        expect(result.resolution).toBe(
          to === "UNCONFIRMED" || to === "CONFIRMED" || to === "IN_PROGRESS"
            ? null
            : "FIXED",
        );
      }
    }
  });

  it("rejects every edge absent from the transition table, including no-ops", () => {
    for (const from of BUG_STATUSES) {
      for (const to of BUG_STATUSES) {
        if (STATUS_TRANSITIONS[from].includes(to)) continue;
        expect(() =>
          transitionBugState(stateFor(from), {
            status: to,
            ...(to === "RESOLVED" ? { resolution: "FIXED" as const } : {}),
          }),
        ).toThrow(InvalidBugTransitionError);
      }
    }
  });

  it.each(["UNCONFIRMED", "CONFIRMED", "IN_PROGRESS"] as const)(
    "resolves an open %s bug",
    (status) => {
      expect(resolveBugState(stateFor(status), "WONTFIX")).toEqual({
        status: "RESOLVED",
        resolution: "WONTFIX",
      });
    },
  );

  it.each(["RESOLVED", "VERIFIED", "CLOSED"] as const)(
    "rejects resolving an already non-open %s bug",
    (status) => {
      expect(() => resolveBugState(stateFor(status), "FIXED")).toThrow(
        InvalidBugTransitionError,
      );
    },
  );

  it("reserves DUPLICATE for markDuplicate", () => {
    expect(() =>
      resolveBugState(stateFor("CONFIRMED"), "DUPLICATE" as never),
    ).toThrow(/bugs\.markDuplicate/u);
    expect(() =>
      transitionBugState(stateFor("CONFIRMED"), {
        status: "RESOLVED",
        resolution: "DUPLICATE",
      }),
    ).toThrow(/bugs\.markDuplicate/u);
    expect(markDuplicateBugState(stateFor("CONFIRMED"))).toEqual({
      status: "RESOLVED",
      resolution: "DUPLICATE",
    });
  });

  it.each(["RESOLVED", "VERIFIED", "CLOSED"] as const)(
    "reopens %s as CONFIRMED and clears its resolution",
    (status) => {
      expect(reopenBugState(stateFor(status))).toEqual({
        status: "CONFIRMED",
        resolution: null,
      });
    },
  );

  it.each(["UNCONFIRMED", "CONFIRMED", "IN_PROGRESS"] as const)(
    "rejects reopening an open %s bug",
    (status) => {
      expect(() => reopenBugState(stateFor(status))).toThrow(
        InvalidBugTransitionError,
      );
    },
  );

  it("requires a resolution when entering RESOLVED", () => {
    expect(() =>
      transitionBugState(stateFor("CONFIRMED"), { status: "RESOLVED" }),
    ).toThrow(/requires a valid resolution/u);
    expect(() =>
      transitionBugState(stateFor("CONFIRMED"), {
        status: "RESOLVED",
        resolution: "NOT_A_RESOLUTION" as never,
      }),
    ).toThrow(/requires a valid resolution/u);
  });

  it("retains the resolution while verifying or closing", () => {
    const verified = transitionBugState(
      { status: "RESOLVED", resolution: "INVALID" },
      { status: "VERIFIED" },
    );
    expect(verified).toEqual({ status: "VERIFIED", resolution: "INVALID" });
    expect(transitionBugState(verified, { status: "CLOSED" })).toEqual({
      status: "CLOSED",
      resolution: "INVALID",
    });
  });

  it("does not permit a verify or close transition to rewrite resolution", () => {
    expect(() =>
      transitionBugState(
        { status: "RESOLVED", resolution: "FIXED" },
        { status: "VERIFIED", resolution: "INVALID" },
      ),
    ).toThrow(/cannot change the resolution/u);
  });

  it("rejects internally inconsistent or unknown stored state", () => {
    expect(() =>
      transitionBugState(
        { status: "CONFIRMED", resolution: "FIXED" },
        { status: "RESOLVED", resolution: "FIXED" },
      ),
    ).toThrow(/open status CONFIRMED/u);
    expect(() =>
      transitionBugState(
        { status: "RESOLVED", resolution: null },
        { status: "VERIFIED" },
      ),
    ).toThrow(/requires a resolution/u);
    expect(() =>
      transitionBugState(
        { status: "UNKNOWN" as never, resolution: null },
        { status: "CONFIRMED" },
      ),
    ).toThrow(/unknown bug status/u);
  });
});
