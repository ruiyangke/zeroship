/**
 * R3 MINOR-12 — `_warnedAccShapes` is bounded.
 *
 * Sibling-bug of the `_warnedShapes` cap fix in `collection.ts`: the
 * accumulator-warning dedup set in `utils.ts` used to be an unbounded
 * `Set<string>`. An AI-generated pipeline that synthesises new
 * accumulator names would leak one entry per shape forever. The fix
 * caps the LRU at MAX_WARNED_ACC_SHAPES (1024) using the same pattern
 * as the collection-side warning set.
 */
import { test, describe, beforeEach, afterEach } from "node:test";
import assert from "node:assert/strict";
// Pull translateAggregatePipeline and the warning getters from the
// same module instance (compiled dist via @zeroship/db/internal) so
// the LRU state the warning getter inspects is the SAME one the
// translator writes to. Importing `translateAggregatePipeline` from
// `../src/utils.js` would compile through tsx and produce a separate
// `_warnedAccShapes` Set.
import {
  translateAggregatePipeline,
  __zeroshipDbResetAccShapeWarnings,
  __zeroshipDbWarnedAccShapesSize,
} from "@zeroship/db/internal";

const MAX_WARNED_ACC_SHAPES = 1024;

describe("R3 MINOR-12 — _warnedAccShapes is bounded at MAX_WARNED_ACC_SHAPES", () => {
  let warnings: string[];
  let origWarn: typeof console.warn;

  beforeEach(() => {
    warnings = [];
    origWarn = console.warn;
    console.warn = (msg: string) => { warnings.push(String(msg)); };
    __zeroshipDbResetAccShapeWarnings();
  });

  afterEach(() => {
    console.warn = origWarn;
    __zeroshipDbResetAccShapeWarnings();
  });

  test("Map size stays <= MAX across 2048 distinct unknown-accumulator shapes", () => {
    const N = MAX_WARNED_ACC_SHAPES * 2;
    for (let i = 0; i < N; i++) {
      // Distinct unknown-accumulator name per iteration. The dedup key
      // is the sorted set of unknown $op names — one new key per i.
      const pipeline = [
        {
          $group: {
            _id: "$x",
            total: { [`$unknownOp${i}`]: "$value" },
          },
        },
      ];
      translateAggregatePipeline(pipeline, (s) => s);
    }
    const sz = __zeroshipDbWarnedAccShapesSize();
    assert.ok(
      sz <= MAX_WARNED_ACC_SHAPES,
      `expected size <= ${MAX_WARNED_ACC_SHAPES}, got ${sz}`,
    );
    assert.equal(
      sz,
      MAX_WARNED_ACC_SHAPES,
      `expected size == ${MAX_WARNED_ACC_SHAPES} after filling, got ${sz}`,
    );
    assert.equal(
      warnings.length,
      N,
      `expected ${N} warnings (one per new shape), got ${warnings.length}`,
    );
  });

  test("re-firing a still-resident shape does not produce a new warning", () => {
    // Fill with N+1 unknown shapes, then re-fire the most-recent one.
    const N = MAX_WARNED_ACC_SHAPES;
    for (let i = 0; i <= N; i++) {
      translateAggregatePipeline(
        [{ $group: { _id: "$x", v: { [`$unknownOp${i}`]: "$y" } } }],
        (s) => s,
      );
    }
    const baseline = warnings.length;
    assert.equal(baseline, N + 1);

    // Re-fire shape #N (still resident; #0 was evicted to make room).
    translateAggregatePipeline(
      [{ $group: { _id: "$x", v: { [`$unknownOp${N}`]: "$y" } } }],
      (s) => s,
    );
    assert.equal(
      warnings.length,
      baseline,
      "re-firing a still-resident shape must not produce a new warning",
    );

    // Re-fire shape #0 (evicted). It's seen as new again -> one more warning.
    translateAggregatePipeline(
      [{ $group: { _id: "$x", v: { $unknownOp0: "$y" } } }],
      (s) => s,
    );
    assert.equal(
      warnings.length,
      baseline + 1,
      "re-firing an evicted shape produces one new warning",
    );

    assert.equal(__zeroshipDbWarnedAccShapesSize(), MAX_WARNED_ACC_SHAPES);
  });
});
