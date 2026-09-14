import { test } from "node:test";
import assert from "node:assert/strict";
import { t } from "../src/index.js";
import { normalizeSchema } from "../../../crates/zeroship-data-v8/js/testing.js";
import { validateDoc, isTimestampValue } from "../src/validate.js";
import { ValidationError } from "../src/errors.js";
import { validateArrayPushOps } from "../src/collection.js";

test("timestamps accept integral Unix milliseconds, Dates, and real ISO instants", () => {
  const schema = normalizeSchema({ instant: t.timestamp() });
  for (const instant of [
    -62_135_596_800_000, -1, 0, 253_402_300_790_001, 253_402_300_799_999,
    new Date(-1), new Date("0001-01-01T00:00:00Z"),
    "0001-01-01", "2000-02-29T00:00:00", "1969-12-31T23:59:59.999999Z",
    "1970-01-01T02:30:00+02:30", "1970-01-01T02:30:00+0230",
    "1969-12-31T22:00:00-02", "9999-12-31T23:59:59.999Z",
  ]) {
    assert.ok(isTimestampValue(instant), String(instant));
    const input = { instant };
    assert.doesNotThrow(() => validateDoc(input, schema));
    assert.strictEqual(input.instant, instant, "validation must preserve caller-owned values");
  }
});

test("timestamps reject impossible dates, times, offsets, and unrepresentable instants", () => {
  const scalar = normalizeSchema({ instant: t.timestamp() });
  const array = normalizeSchema({ instants: t.array(t.timestamp()) });
  for (const instant of [
    new Date(NaN), NaN, Infinity, -Infinity, 0.5, -62_135_596_800_001, 253_402_300_800_000,
    "0000-01-01", "1900-02-29", "2026-02-30T00:00:00Z", "2026-04-31T00:00:00Z",
    "2026-01-01T24:00:00Z", "2026-01-01T00:60:00Z", "2026-01-01T00:00:60Z",
    "2026-01-01T+1:00:00Z", "2026-01-01T00:00:00+01:-1",
    "2026-01-01T00:00:00+24:00", "2026-01-01T00:00:00+00:60",
    "2026-01-01T00:00:00.Z", "2026-01-01T00:00:00Z ", "2026-01-01T00:00:00Z\n",
    "0001-01-01T00:00:00+01:00", "9999-12-31T23:59:59-01:00", "2026",
  ]) {
    assert.equal(isTimestampValue(instant), false, String(instant));
    assert.throws(() => validateDoc({ instant }, scalar), ValidationError);
    assert.throws(() => validateDoc({ instants: [instant] }, array), ValidationError);
    assert.throws(() => validateArrayPushOps({ $push: { instants: instant } }, array), ValidationError);
  }
});
