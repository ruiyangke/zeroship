import test from "node:test";
import assert from "node:assert/strict";
import { isValidCalendarDate } from "../src/validate";

test("calendar dates accept early years and Gregorian leap days", () => {
  for (const date of ["0001-01-01", "0004-02-29", "0099-12-31", "0100-03-01", "2000-02-29", "9999-12-31"]) {
    assert.equal(isValidCalendarDate(date), true, date);
  }
});

test("calendar dates reject invalid dates and non-calendar representations", () => {
  for (const date of ["0000-01-01", "1900-02-29", "2026-02-30", "2026-04-31", "10000-01-01", "2026-01-01T00:00:00Z", "secret_not_a_date"]) {
    assert.equal(isValidCalendarDate(date), false, date);
  }
});
