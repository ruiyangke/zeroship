// @vitest-environment node
import { afterEach, describe, expect, it, vi } from "vitest";
import { assertInsertedTimestamps, assertUpdatedTimestamps, waitForClockAfter } from "../tests/timestamps";

const instant = Date.parse("2026-09-11T00:00:00.123Z");
const inserted = { created_at: instant, updated_at: instant };

afterEach(() => vi.restoreAllMocks());

describe("system timestamp contract", () => {
  it("allows separate rows to share their creation timestamp", () => {
    const window = { started: instant, finished: instant };
    for (const id of ["todo_first", "todo_second"]) {
      expect(() => assertInsertedTimestamps({ ...inserted, id }, window)).not.toThrow();
    }
  });

  it.each([instant / 1_000, instant + 0.5, String(instant), NaN, Infinity])(
    "rejects a value that is not an integer millisecond timestamp: %s",
    (created_at) => {
      expect(() => assertInsertedTimestamps({ ...inserted, created_at }, {
        started: instant, finished: instant + 1,
      })).toThrow("integer Unix millisecond timestamp");
    },
  );

  it("rejects integral seconds through the observed request window", () => {
    expect(() => assertInsertedTimestamps({ created_at: Math.floor(instant / 1_000), updated_at: instant }, {
      started: instant, finished: instant + 1,
    })).toThrow("created_at must fall within its request");
  });

  it("checks updated_at as well as created_at on insert", () => {
    expect(() => assertInsertedTimestamps({ ...inserted, updated_at: "invalid" }, {
      started: instant, finished: instant,
    })).toThrow("updated_at must be an integer");
    expect(() => assertInsertedTimestamps({ ...inserted, updated_at: instant - 1 }, {
      started: instant - 1, finished: instant,
    })).toThrow("updated_at must not precede created_at");
  });

  it("preserves creation time and advances update time inside its request window", () => {
    expect(() => assertUpdatedTimestamps(inserted, { ...inserted, updated_at: instant + 1 }, {
      started: instant + 1, finished: instant + 2,
    })).not.toThrow();
  });

  it("rejects an update that rewrites created_at", () => {
    expect(() => assertUpdatedTimestamps(inserted, { created_at: instant + 1, updated_at: instant + 1 }, {
      started: instant + 1, finished: instant + 2,
    })).toThrow("preserve created_at");
  });

  it("rejects a stale update timestamp after a clock boundary", () => {
    expect(() => assertUpdatedTimestamps(inserted, inserted, {
      started: instant + 1, finished: instant + 2,
    })).toThrow("updated_at must fall within its request");
  });

  it("waits for an observed clock change before issuing an update", async () => {
    const clock = vi.spyOn(Date, "now")
      .mockReturnValueOnce(instant - 1)
      .mockReturnValueOnce(instant)
      .mockReturnValue(instant + 1);
    await expect(waitForClockAfter(instant)).resolves.toBe(instant + 1);
    expect(clock).toHaveBeenCalledTimes(3);
  });
});
