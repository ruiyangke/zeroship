import { describe, expect, it } from "vitest";
import { activityValue, diffTrackedFields } from "../src/lib/changes";

describe("tracked field diffs", () => {
  it("serializes activity values into the text-column representation", () => {
    expect(activityValue("P1")).toBe("P1");
    expect(activityValue(42)).toBe("42");
    expect(activityValue(true)).toBe("true");
    expect(activityValue(false)).toBe("false");
    expect(activityValue(null)).toBeNull();
    expect(activityValue(undefined)).toBeNull();
  });

  it("returns one row for each changed field in caller order", () => {
    expect(
      diffTrackedFields(
        { status: "CONFIRMED", priority: "P3", voteCount: 0 },
        { status: "RESOLVED", priority: "P1", voteCount: 0 },
        ["status", "priority", "voteCount"],
      ),
    ).toEqual([
      {
        fieldName: "status",
        oldValue: "CONFIRMED",
        newValue: "RESOLVED",
      },
      { fieldName: "priority", oldValue: "P3", newValue: "P1" },
    ]);
  });

  it("represents setting and clearing nullable fields", () => {
    expect(
      diffTrackedFields(
        { resolution: null, assigneeId: "usr_old" },
        { resolution: "FIXED", assigneeId: null },
        ["resolution", "assigneeId"],
      ),
    ).toEqual([
      { fieldName: "resolution", oldValue: null, newValue: "FIXED" },
      { fieldName: "assigneeId", oldValue: "usr_old", newValue: null },
    ]);
  });

  it("does not emit unchanged fields", () => {
    expect(
      diffTrackedFields(
        { status: "CONFIRMED", isConfirmed: true },
        { status: "CONFIRMED", isConfirmed: true },
        ["status", "isConfirmed"],
      ),
    ).toEqual([]);
  });

  it("treats undefined and null as the same stored activity value", () => {
    expect(
      diffTrackedFields(
        { milestoneId: undefined },
        { milestoneId: null },
        ["milestoneId"],
      ),
    ).toEqual([]);
  });

  it("deduplicates repeated field names", () => {
    expect(
      diffTrackedFields(
        { severity: "normal" },
        { severity: "critical" },
        ["severity", "severity", "severity"],
      ),
    ).toEqual([
      { fieldName: "severity", oldValue: "normal", newValue: "critical" },
    ]);
  });
});
