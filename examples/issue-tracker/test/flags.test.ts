import { describe, expect, it } from "vitest";
import {
  InvalidFlagTargetError,
  assertFlagTarget,
} from "../src/lib/flags";

describe("flag target invariant", () => {
  it("accepts and normalizes an issue flag target", () => {
    expect(assertFlagTarget("issue", { issueId: "issue_123" })).toEqual({
      issueId: "issue_123",
      attachmentId: null,
    });
  });

  it("accepts and normalizes an attachment flag target", () => {
    expect(
      assertFlagTarget("attachment", { attachmentId: "atta_123" }),
    ).toEqual({ issueId: null, attachmentId: "atta_123" });
  });

  it("rejects setting neither target", () => {
    expect(() => assertFlagTarget("issue", {})).toThrow(
      /exactly one of issueId and attachmentId/u,
    );
  });

  it("rejects setting both targets", () => {
    expect(() =>
      assertFlagTarget("issue", {
        issueId: "issue_123",
        attachmentId: "atta_123",
      }),
    ).toThrow(/exactly one of issueId and attachmentId/u);
  });

  it("rejects an attachment target for an issue flag type", () => {
    expect(() =>
      assertFlagTarget("issue", { attachmentId: "atta_123" }),
    ).toThrow(/requires issueId/u);
  });

  it("rejects an issue target for an attachment flag type", () => {
    expect(() =>
      assertFlagTarget("attachment", { issueId: "issue_123" }),
    ).toThrow(/requires attachmentId/u);
  });

  it.each([
    { issueId: "" },
    { issueId: "   " },
    { attachmentId: "" },
  ])("rejects empty ids: %o", (target) => {
    expect(() => assertFlagTarget("issue", target)).toThrow(
      InvalidFlagTargetError,
    );
  });

  it("rejects an unknown target type arriving from the wire", () => {
    expect(() =>
      assertFlagTarget("comment" as never, { issueId: "issue_123" }),
    ).toThrow(/unsupported flag target type/u);
  });
});
