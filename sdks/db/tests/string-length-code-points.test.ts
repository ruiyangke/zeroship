/**
 * String length bounds are stated in CHARACTERS and must be counted that way.
 *
 * `String.prototype.length` is UTF-16 CODE UNITS, a third unit that is neither
 * bytes nor characters. Anything outside the Basic Multilingual Plane - emoji,
 * the CJK extensions, most mathematical symbols - is one character and TWO code
 * units, so a code-unit count double-charges it.
 *
 * That matters because it makes validation stricter than storage. PostgreSQL
 * counts `varchar(n)` in characters: `varchar(3)` accepts three emoji, stores
 * them, and reports `length` 3 with `octet_length` 12. Refusing that input here
 * rejects a value the database would have taken, while the error message
 * promises a limit in characters.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t } from "@zeroship/db";
import { validateDoc } from "../src/validate.js";
import { normalizeSchema } from "@zeroship/bootstrap/install-schema";

describe("string length bounds count characters, not UTF-16 code units", () => {
  // Three characters, whatever plane they come from.
  const schema = normalizeSchema({ title: t.string().min(1).max(3) });

  const THUMB = "\u{1F44D}"; // 1 character, 2 code units, 4 bytes

  test("accepts three non-BMP characters against max 3", () => {
    assert.doesNotThrow(
      () => validateDoc({ title: THUMB.repeat(3) }, schema),
      "three characters must satisfy max 3 even at two code units each",
    );
  });

  test("a single non-BMP character satisfies min 1", () => {
    assert.doesNotThrow(
      () => validateDoc({ title: THUMB }, schema),
      "one character is one character, not two",
    );
  });

  test("still rejects four non-BMP characters against max 3", () => {
    assert.throws(
      () => validateDoc({ title: THUMB.repeat(4) }, schema),
      /at most 3/,
      "four characters must exceed max 3",
    );
  });

  test("plain ASCII bounds are unchanged", () => {
    assert.doesNotThrow(() => validateDoc({ title: "abc" }, schema));
    assert.throws(() => validateDoc({ title: "abcd" }, schema), /at most 3/);
  });
});
