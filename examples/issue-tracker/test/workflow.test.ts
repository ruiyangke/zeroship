import { describe, expect, it } from "vitest";
import {
  ISSUE_STATUSES,
  STATUS_TRANSITIONS,
  InvalidIssueTransitionError,
  markDuplicateIssueState,
  reopenIssueState,
  resolveIssueState,
  transitionIssueState,
  type IssueState,
  type IssueStatus,
} from "../src/lib/workflow";

const stateFor = (status: IssueStatus): IssueState => ({
  status,
  resolution:
    status === "RESOLVED" || status === "VERIFIED" || status === "CLOSED"
      ? "FIXED"
      : null,
});

describe("issue status workflow", () => {
  it("accepts every edge in the transition table", () => {
    for (const from of ISSUE_STATUSES) {
      for (const to of STATUS_TRANSITIONS[from]) {
        const result = transitionIssueState(stateFor(from), {
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
    for (const from of ISSUE_STATUSES) {
      for (const to of ISSUE_STATUSES) {
        if (STATUS_TRANSITIONS[from].includes(to)) continue;
        expect(() =>
          transitionIssueState(stateFor(from), {
            status: to,
            ...(to === "RESOLVED" ? { resolution: "FIXED" as const } : {}),
          }),
        ).toThrow(InvalidIssueTransitionError);
      }
    }
  });

  it.each(["UNCONFIRMED", "CONFIRMED", "IN_PROGRESS"] as const)(
    "resolves an open %s issue",
    (status) => {
      expect(resolveIssueState(stateFor(status), "WONTFIX")).toEqual({
        status: "RESOLVED",
        resolution: "WONTFIX",
      });
    },
  );

  it.each(["RESOLVED", "VERIFIED", "CLOSED"] as const)(
    "rejects resolving an already non-open %s issue",
    (status) => {
      expect(() => resolveIssueState(stateFor(status), "FIXED")).toThrow(
        InvalidIssueTransitionError,
      );
    },
  );

  it("reserves DUPLICATE for markDuplicate", () => {
    expect(() =>
      resolveIssueState(stateFor("CONFIRMED"), "DUPLICATE" as never),
    ).toThrow(/issues\.markDuplicate/u);
    expect(() =>
      transitionIssueState(stateFor("CONFIRMED"), {
        status: "RESOLVED",
        resolution: "DUPLICATE",
      }),
    ).toThrow(/issues\.markDuplicate/u);
    expect(markDuplicateIssueState(stateFor("CONFIRMED"))).toEqual({
      status: "RESOLVED",
      resolution: "DUPLICATE",
    });
  });

  it.each(["RESOLVED", "VERIFIED", "CLOSED"] as const)(
    "reopens %s as CONFIRMED and clears its resolution",
    (status) => {
      expect(reopenIssueState(stateFor(status))).toEqual({
        status: "CONFIRMED",
        resolution: null,
      });
    },
  );

  it.each(["UNCONFIRMED", "CONFIRMED", "IN_PROGRESS"] as const)(
    "rejects reopening an open %s issue",
    (status) => {
      expect(() => reopenIssueState(stateFor(status))).toThrow(
        InvalidIssueTransitionError,
      );
    },
  );

  it("requires a resolution when entering RESOLVED", () => {
    expect(() =>
      transitionIssueState(stateFor("CONFIRMED"), { status: "RESOLVED" }),
    ).toThrow(/requires a valid resolution/u);
    expect(() =>
      transitionIssueState(stateFor("CONFIRMED"), {
        status: "RESOLVED",
        resolution: "NOT_A_RESOLUTION" as never,
      }),
    ).toThrow(/requires a valid resolution/u);
  });

  it("retains the resolution while verifying or closing", () => {
    const verified = transitionIssueState(
      { status: "RESOLVED", resolution: "INVALID" },
      { status: "VERIFIED" },
    );
    expect(verified).toEqual({ status: "VERIFIED", resolution: "INVALID" });
    expect(transitionIssueState(verified, { status: "CLOSED" })).toEqual({
      status: "CLOSED",
      resolution: "INVALID",
    });
  });

  it("does not permit a verify or close transition to rewrite resolution", () => {
    expect(() =>
      transitionIssueState(
        { status: "RESOLVED", resolution: "FIXED" },
        { status: "VERIFIED", resolution: "INVALID" },
      ),
    ).toThrow(/cannot change the resolution/u);
  });

  it("rejects internally inconsistent or unknown stored state", () => {
    expect(() =>
      transitionIssueState(
        { status: "CONFIRMED", resolution: "FIXED" },
        { status: "RESOLVED", resolution: "FIXED" },
      ),
    ).toThrow(/open status CONFIRMED/u);
    expect(() =>
      transitionIssueState(
        { status: "RESOLVED", resolution: null },
        { status: "VERIFIED" },
      ),
    ).toThrow(/requires a resolution/u);
    expect(() =>
      transitionIssueState(
        { status: "UNKNOWN" as never, resolution: null },
        { status: "CONFIRMED" },
      ),
    ).toThrow(/unknown issue status/u);
  });
});
