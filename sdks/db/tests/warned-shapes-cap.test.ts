/**
 * Gap P — `_warnedShapes` is bounded.
 *
 * Module-scope dedup state can grow without bound if an AI-generated app
 * synthesises filter shapes (metric names, dynamic identifiers, etc.).
 * Over a long-lived dev server that's a slow memory leak. The fix caps
 * the LRU at MAX_WARNED_SHAPES (1024) and evicts the oldest entry on
 * overflow.
 *
 * This suite fires >MAX distinct shapes through `find()` and asserts:
 *   1. the internal Map size never exceeds the cap;
 *   2. each new shape still fires a `console.warn` (functional behaviour
 *      is preserved — eviction only affects which shapes get a *repeat*
 *      warning if seen again later).
 */
import { test, describe, beforeEach, afterEach } from "node:test";
import assert from "node:assert/strict";
import { schema, t } from "../src/types.js";
import { _installSchema } from "../src/db.js";
import {
  __zeroshipDbResetIndexWarnings,
  __zeroshipDbWarnedShapesSize,
} from "../src/collection.js";

const MAX_WARNED_SHAPES = 1024;

describe("Gap P — _warnedShapes is bounded at MAX_WARNED_SHAPES", () => {
  let warnings: string[];
  let origWarn: typeof console.warn;

  beforeEach(() => {
    warnings = [];
    origWarn = console.warn;
    console.warn = (msg: string) => {
      warnings.push(String(msg));
    };
    (globalThis as { __zeroshipDbWarnIndexInTest?: boolean }).__zeroshipDbWarnIndexInTest = true;
    __zeroshipDbResetIndexWarnings();
  });

  afterEach(() => {
    console.warn = origWarn;
    (globalThis as { __zeroshipDbWarnIndexInTest?: boolean }).__zeroshipDbWarnIndexInTest = false;
    __zeroshipDbResetIndexWarnings();
  });

  test("Map size stays <= MAX_WARNED_SHAPES across 2048 distinct schema-known shapes", async () => {
    // Build a schema with N+M known fields f0..fN+M-1 so each shape key
    // (which filters on a single distinct field per iteration) lands a
    // *new* entry in _warnedShapes. Declared index is on `f_special`
    // only; no other field is covered, so every other-field filter fires
    // the warning and inserts a new shape.
    const N = MAX_WARNED_SHAPES * 2;
    const fields: Record<string, ReturnType<typeof t.string>> = {
      f_special: t.string(),
    };
    for (let i = 0; i < N; i++) {
      fields[`f${i}`] = t.string();
    }

    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const native = {
      registerModel: () => Promise.resolve(),
      collection() {
        return {
          async find() {
            return [];
          },
          async findOne() {
            return null;
          },
        };
      },
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
    } as any;

    const db = _installSchema(
      {
        big: schema(fields).index("by_special", ["f_special"]),
      },
      { native },
    );

    // Fire N distinct filter shapes. Each filter is `{fI: "x"}`, so the
    // sorted-key dedup gives N distinct shape keys.
    for (let i = 0; i < N; i++) {
      const filter: Record<string, string> = {};
      filter[`f${i}`] = "x";
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      await (db.big as any).find(filter);
    }

    // The Map must NOT have grown past the cap.
    const sz = __zeroshipDbWarnedShapesSize();
    assert.ok(
      sz <= MAX_WARNED_SHAPES,
      `expected size <= ${MAX_WARNED_SHAPES}, got ${sz}`,
    );
    // We're shoving exactly 2N shapes through — the cap should be hit.
    assert.equal(
      sz,
      MAX_WARNED_SHAPES,
      `expected size == ${MAX_WARNED_SHAPES} after filling, got ${sz}`,
    );

    // Every shape was new on first sight -> N warnings fired.
    assert.equal(
      warnings.length,
      N,
      `expected ${N} warnings (one per new shape), got ${warnings.length}`,
    );
  });

  test("re-firing a shape that survived eviction still dedups (no double-warn)", async () => {
    // Fill with N+1 shapes, then immediately re-fire shape #N (which is
    // still in the Map — only shape #0 was evicted). The dedup must hold.
    const N = MAX_WARNED_SHAPES;
    const fields: Record<string, ReturnType<typeof t.string>> = {};
    for (let i = 0; i <= N; i++) {
      fields[`f${i}`] = t.string();
    }

    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const native = {
      registerModel: () => Promise.resolve(),
      collection() {
        return {
          async find() {
            return [];
          },
          async findOne() {
            return null;
          },
        };
      },
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
    } as any;

    const db = _installSchema(
      {
        big: schema(fields), // no declared indexes -> every filter warns
      },
      { native },
    );

    for (let i = 0; i <= N; i++) {
      const filter: Record<string, string> = {};
      filter[`f${i}`] = "x";
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      await (db.big as any).find(filter);
    }

    assert.equal(__zeroshipDbWarnedShapesSize(), MAX_WARNED_SHAPES);
    const baseline = warnings.length;
    assert.equal(baseline, N + 1, `expected ${N + 1} warnings, got ${baseline}`);

    // Re-fire the most-recently-inserted shape (#N) — should NOT warn.
    const refilter: Record<string, string> = {};
    refilter[`f${N}`] = "x";
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    await (db.big as any).find(refilter);
    assert.equal(
      warnings.length,
      baseline,
      "re-firing a still-resident shape must not produce a new warning",
    );

    // Re-fire the oldest shape (#0) — it was evicted when #N was
    // inserted, so this counts as new and fires another warning.
    const oldfilter: Record<string, string> = {};
    oldfilter.f0 = "x";
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    await (db.big as any).find(oldfilter);
    assert.equal(
      warnings.length,
      baseline + 1,
      "re-firing an evicted shape should produce one new warning",
    );

    // Cap still holds.
    assert.equal(__zeroshipDbWarnedShapesSize(), MAX_WARNED_SHAPES);
  });
});
