/**
 * R7 m1 regression — `validateArrayPushOps`'s `date` branch was laxer
 * than `validate.ts`'s array-of-date branch: it accepted any string,
 * while `checkField` also requires `isParseableDateString`. So
 * `insert({dates: ["xyz"]})` was rejected but
 * `update({}, {$push: {dates: "xyz"}})` slipped through. Closed by
 * exporting the helper from `validate.ts` and calling it from both
 * sites.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t } from "@zeroship/db";
import { validateDoc } from "../src/validate.js";
import { normalizeSchema } from "@zeroship/bootstrap/install-schema";
import { ValidationError } from "../src/errors.js";
import { validateArrayPushOps } from "../src/collection.js";

describe("R7 m1 — validateArrayPushOps date branch parity with array-validate", () => {
  const schema = normalizeSchema({ dates: t.array(t.timestamp()) });

  test("validateDoc rejects an unparseable date string in the array", () => {
    assert.throws(
      () => validateDoc({ dates: ["xyz"] }, schema),
      ValidationError,
    );
  });

  test("$push rejects an unparseable date string (parity with validateDoc)", () => {
    assert.throws(
      () => validateArrayPushOps({ $push: { dates: "xyz" } }, schema),
      ValidationError,
    );
  });

  test("$addToSet rejects an unparseable date string", () => {
    assert.throws(
      () => validateArrayPushOps({ $addToSet: { dates: "not-a-date" } }, schema),
      ValidationError,
    );
  });

  test("$push rejects a year-only string (matches validate.ts's stricter regex)", () => {
    assert.throws(
      () => validateArrayPushOps({ $push: { dates: "2026" } }, schema),
      ValidationError,
    );
  });

  test("$push still accepts ISO 8601 date strings", () => {
    assert.doesNotThrow(() =>
      validateArrayPushOps(
        { $push: { dates: "2026-01-01T00:00:00Z" } },
        schema,
      ),
    );
    assert.doesNotThrow(() =>
      validateArrayPushOps({ $push: { dates: "2026-01-01" } }, schema),
    );
  });

  test("$push still accepts Date instances", () => {
    assert.doesNotThrow(() =>
      validateArrayPushOps({ $push: { dates: new Date() } }, schema),
    );
  });
});
