/**
 * **P5.5 PR 3** — read-side rehydration: the SDK's `mapResultDoc`
 * detects `__zsmask__`-tagged wire payloads emitted by
 * `mask_pass::wrap_row_on_read` and constructs `MaskedValue<T>`
 * instances at the boundary.
 *
 * These tests pin the rehydration contract — they don't exercise
 * the native side; they hand `mapResultDoc` the wire shape directly
 * and assert the SDK constructs the right wrapper.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
// Import MaskedValue from `src/` (not `@zeroship/db`) so we get the
// same class identity `mapResultDoc` uses for `new MaskedValue(...)`.
// The two paths resolve to different classes under tsx (src vs. dist),
// which would break `instanceof` if mixed.
import { MaskedValue } from "../src/types.js";
import { mapResultDoc, rehydrateMaskedValues } from "../src/utils.js";

describe("P5.5 PR 3 — rehydrate __zsmask__ wire payloads", () => {
  test("mapResultDoc wraps a __zsmask__-tagged value in MaskedValue", () => {
    const wire = {
      id: "usr_01",
      ssn: {
        sentinel: "__zsmask__",
        masked: "***-**-6789",
        classification: "spi",
        _meta: { collection: "users", row_pk: "usr_01", column: "ssn" },
      },
      name: "alice",
    };
    const out = mapResultDoc(wire, (s) => s);
    assert.equal(out.id, "usr_01");
    assert.equal(out.name, "alice");
    assert.ok(out.ssn instanceof MaskedValue, "ssn must be MaskedValue");
    const mv = out.ssn as MaskedValue;
    assert.equal(mv.masked, "***-**-6789");
    assert.equal(mv.classification, "spi");
    assert.equal(mv._meta.collection, "users");
    assert.equal(mv._meta.row_pk, "usr_01");
    assert.equal(mv._meta.column, "ssn");
  });

  test("mapResultDoc passes through non-masked values verbatim", () => {
    const wire = { id: 1, name: "alice", age: 30, payload: { nested: "obj" } };
    const out = mapResultDoc(wire, (s) => s);
    assert.equal(out.id, 1);
    assert.equal(out.name, "alice");
    assert.equal(out.age, 30);
    assert.deepEqual(out.payload, { nested: "obj" });
  });

  test("mapResultDoc applies field renaming alongside rehydration", () => {
    const wire = {
      user_id: "usr_01",
      ssn_field: {
        sentinel: "__zsmask__",
        masked: "***",
        classification: "spi",
        _meta: { collection: "users", row_pk: "usr_01", column: "ssn_field" },
      },
    };
    const out = mapResultDoc(wire, (col) => (col === "user_id" ? "userId" : col === "ssn_field" ? "ssnField" : col));
    assert.equal(out.userId, "usr_01");
    assert.ok(out.ssnField instanceof MaskedValue);
    assert.equal((out.ssnField as MaskedValue).masked, "***");
  });

  test("rehydrateMaskedValues handles a row with multiple masked columns", () => {
    const wire = {
      id: "usr_01",
      ssn: {
        sentinel: "__zsmask__",
        masked: "***-**-6789",
        classification: "spi",
        _meta: { collection: "users", row_pk: "usr_01", column: "ssn" },
      },
      email: {
        sentinel: "__zsmask__",
        masked: "a***@example.com",
        classification: "pii",
        _meta: { collection: "users", row_pk: "usr_01", column: "email" },
      },
      name: "alice",
    };
    const out = rehydrateMaskedValues(wire);
    assert.ok(out.ssn instanceof MaskedValue);
    assert.ok(out.email instanceof MaskedValue);
    assert.equal((out.ssn as MaskedValue).classification, "spi");
    assert.equal((out.email as MaskedValue).classification, "pii");
    assert.equal(out.name, "alice");
  });

  test("rehydrateMaskedValues leaves a value with the wrong sentinel untouched", () => {
    // Defensive: a stray object shaped like the wire payload but with
    // a different sentinel must NOT be auto-wrapped (this would let a
    // malicious caller forge a MaskedValue).
    const wire = {
      ssn: {
        sentinel: "totally-different",
        masked: "***",
        classification: "spi",
      },
    };
    const out = rehydrateMaskedValues(wire);
    assert.ok(!(out.ssn instanceof MaskedValue));
    assert.deepEqual(out.ssn, wire.ssn);
  });

  test("MaskedValue.toString() yields the masked string (coercion safety)", () => {
    // Smoke test that the rehydration chain preserves the
    // never-leak-plaintext-on-coercion invariant from PR 1.
    const wire = {
      ssn: {
        sentinel: "__zsmask__",
        masked: "***-**-6789",
        classification: "spi",
        _meta: { collection: "users", row_pk: "usr_01", column: "ssn" },
      },
    };
    const out = mapResultDoc(wire, (s) => s);
    const mv = out.ssn as MaskedValue;
    assert.equal(String(mv), "***-**-6789");
    assert.equal(JSON.stringify({ ssn: mv }), JSON.stringify({ ssn: "***-**-6789" }));
  });
});
