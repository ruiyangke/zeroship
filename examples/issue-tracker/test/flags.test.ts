import { describe, expect, it } from "vitest";
import {
  InvalidFlagTargetError,
  assertFlagTarget,
} from "../src/lib/flags";

describe("flag target invariant", () => {
  it("accepts and normalizes a bug flag target", () => {
    expect(assertFlagTarget("bug", { bugId: "bug_123" })).toEqual({
      bugId: "bug_123",
      attachmentId: null,
    });
  });

  it("accepts and normalizes an attachment flag target", () => {
    expect(
      assertFlagTarget("attachment", { attachmentId: "atta_123" }),
    ).toEqual({ bugId: null, attachmentId: "atta_123" });
  });

  it("rejects setting neither target", () => {
    expect(() => assertFlagTarget("bug", {})).toThrow(
      /exactly one of bugId and attachmentId/u,
    );
  });

  it("rejects setting both targets", () => {
    expect(() =>
      assertFlagTarget("bug", {
        bugId: "bug_123",
        attachmentId: "atta_123",
      }),
    ).toThrow(/exactly one of bugId and attachmentId/u);
  });

  it("rejects an attachment target for a bug flag type", () => {
    expect(() =>
      assertFlagTarget("bug", { attachmentId: "atta_123" }),
    ).toThrow(/requires bugId/u);
  });

  it("rejects a bug target for an attachment flag type", () => {
    expect(() =>
      assertFlagTarget("attachment", { bugId: "bug_123" }),
    ).toThrow(/requires attachmentId/u);
  });

  it.each([
    { bugId: "" },
    { bugId: "   " },
    { attachmentId: "" },
  ])("rejects empty ids: %o", (target) => {
    expect(() => assertFlagTarget("bug", target)).toThrow(
      InvalidFlagTargetError,
    );
  });

  it("rejects an unknown target type arriving from the wire", () => {
    expect(() =>
      assertFlagTarget("comment" as never, { bugId: "bug_123" }),
    ).toThrow(/unsupported flag target type/u);
  });
});
