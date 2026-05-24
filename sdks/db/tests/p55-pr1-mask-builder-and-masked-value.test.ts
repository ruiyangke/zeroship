/**
 * **P5.5 PR 1** — masking foundation: `t.string().mask(...)` /
 * `t.encrypted().mask(...)` DSL modifier + default-mask rule for
 * encrypted columns + `MaskedValue<T>` wrapper class + TypeScript
 * `Row<S>` inference wrapping masked fields.
 *
 * These tests pin behaviour the runtime side cannot enforce:
 *
 *   1. `t.encrypted()` without explicit `.mask(...)` auto-populates
 *      the default mask `{ kind: "full", classification: "pii" }`.
 *   2. `t.string().mask({ kind: "email" })` records the mask
 *      metadata on the FieldDef.
 *   3. Chaining `.mask({ kind: "none" })` on encrypted is the
 *      explicit opt-out — no sibling emission (PR 2/3), no wrap on
 *      read.
 *   4. `.mask()` refuses non-primitive wrapped types (json, array,
 *      union, object) with `mask_on_unsupported_type`.
 *   5. `.mask()` on `t.ref()` refuses with
 *      `encrypted_on_ref_unsupported`.
 *   6. Invalid `kind` / `classification` are refused.
 *   7. `MaskedValue` coercions (`toString` / `toJSON` /
 *      `Symbol.toPrimitive`) all yield the masked representation,
 *      never the plaintext.
 *   8. `MaskedValue.unmask()` / `.canUnmask()` throw
 *      `unmask_not_implemented` in PR 1 (PR 4 wires them).
 *   9. TypeScript `Row<S>` inference wraps masked fields in
 *      `MaskedValue<T>` (smoke test).
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t, MaskedValue } from "@zeroship/db";
import type { Row, MaskedValueRepr } from "@zeroship/db";

describe("P5.5 PR 1 — t.encrypted() default-mask rule", () => {
  test("bare t.encrypted() auto-populates mask = { kind: 'full', classification: 'pii' }", () => {
    const b = t.encrypted();
    const def = b.toFieldDef();
    assert.ok(def.mask, "expected default mask metadata on bare t.encrypted()");
    assert.equal(def.mask!.kind, "full");
    assert.equal(def.mask!.classification, "pii");
  });

  test("t.encrypted({ mode: 'deterministic' }) also gets the default mask", () => {
    const b = t.encrypted({ mode: "deterministic" });
    const def = b.toFieldDef();
    assert.ok(def.mask);
    assert.equal(def.mask!.kind, "full");
    assert.equal(def.mask!.classification, "pii");
  });

  test("t.encrypted().mask({ kind: 'last4' }) overrides the default", () => {
    const b = t.encrypted().mask({ kind: "last4" });
    const def = b.toFieldDef();
    assert.equal(def.mask!.kind, "last4");
    // classification defaults to "pii" when omitted
    assert.equal(def.mask!.classification, "pii");
  });

  test("t.encrypted().mask({ kind: 'none' }) records the explicit opt-out", () => {
    const b = t.encrypted().mask({ kind: "none" });
    const def = b.toFieldDef();
    assert.equal(def.mask!.kind, "none");
  });
});

describe("P5.5 PR 1 — t.string().mask(...) DSL modifier", () => {
  test("t.string().mask({ kind: 'email' }) records mask metadata", () => {
    const b = t.string().mask({ kind: "email" });
    const def = b.toFieldDef();
    assert.equal(def.type, "string");
    assert.ok(def.mask, "mask metadata must be present");
    assert.equal(def.mask!.kind, "email");
    assert.equal(def.mask!.classification, "pii");
  });

  test("classification override sticks", () => {
    const b = t.string().mask({ kind: "full", classification: "internal" });
    const def = b.toFieldDef();
    assert.equal(def.mask!.kind, "full");
    assert.equal(def.mask!.classification, "internal");
  });

  test("t.number().mask({ kind: 'full' }) is permitted (number wrap)", () => {
    const b = t.number().mask({ kind: "full" });
    const def = b.toFieldDef();
    assert.equal(def.type, "number");
    assert.equal(def.mask!.kind, "full");
  });

  test("t.bytes().mask({ kind: 'full' }) is permitted (bytes wrap)", () => {
    const b = t.bytes().mask({ kind: "full" });
    const def = b.toFieldDef();
    assert.equal(def.type, "bytes");
    assert.equal(def.mask!.kind, "full");
  });

  test(".mask() on t.json() rejects with mask_on_unsupported_type", () => {
    assert.throws(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      () => (t.json() as any).mask({ kind: "full" }),
      (e: Error & { code?: string }) => e.code === "mask_on_unsupported_type",
    );
  });

  test(".mask() on t.boolean() rejects with mask_on_unsupported_type", () => {
    assert.throws(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      () => (t.boolean() as any).mask({ kind: "full" }),
      (e: Error & { code?: string }) => e.code === "mask_on_unsupported_type",
    );
  });

  test(".mask() on t.ref('users') rejects with encrypted_on_ref_unsupported (Q-P5-I)", () => {
    assert.throws(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      () => (t.ref("users") as any).mask({ kind: "full" }),
      (e: Error & { code?: string }) => e.code === "encrypted_on_ref_unsupported",
    );
  });

  test("invalid mask kind rejects with mask_invalid_kind", () => {
    assert.throws(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      () => (t.string() as any).mask({ kind: "bogus" }),
      (e: Error & { code?: string }) => e.code === "mask_invalid_kind",
    );
  });

  test("invalid classification rejects with mask_invalid_classification", () => {
    assert.throws(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      () => (t.string() as any).mask({ kind: "full", classification: "bogus" }),
      (e: Error & { code?: string }) => e.code === "mask_invalid_classification",
    );
  });

  test("opts = null rejects with mask_invalid_opts", () => {
    assert.throws(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      () => (t.string() as any).mask(null),
      (e: Error & { code?: string }) => e.code === "mask_invalid_opts",
    );
  });
});

describe("P5.5 PR 1 — MaskedValue<T> wrapper", () => {
  function makeRepr(masked = "***-**-6789", classification: "pii" | "spi" = "spi"): MaskedValueRepr {
    return { masked, classification, sentinel: "__zsmask__" };
  }
  const meta = { collection: "users", row_pk: "usr_xyz", column: "ssn" };

  test("constructor stores masked + classification + meta", () => {
    const v = new MaskedValue<string>(makeRepr(), meta);
    assert.equal(v.masked, "***-**-6789");
    assert.equal(v.classification, "spi");
    assert.equal(v._meta.collection, "users");
    assert.equal(v._meta.row_pk, "usr_xyz");
    assert.equal(v._meta.column, "ssn");
  });

  test("constructor rejects repr without the __zsmask__ sentinel", () => {
    assert.throws(
      () =>
        new MaskedValue<string>(
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          { masked: "x", classification: "pii" } as any,
          meta,
        ),
      (e: Error & { code?: string }) => e.code === "masked_value_invalid_repr",
    );
  });

  test("toString() returns the masked representation, never plaintext", () => {
    const v = new MaskedValue<string>(makeRepr(), meta);
    assert.equal(v.toString(), "***-**-6789");
    assert.equal(`${v}`, "***-**-6789");
    assert.equal(String(v), "***-**-6789");
  });

  test("toJSON() yields the masked representation so JSON.stringify is safe", () => {
    const v = new MaskedValue<string>(makeRepr(), meta);
    assert.equal(v.toJSON(), "***-**-6789");
    assert.equal(JSON.stringify(v), '"***-**-6789"');
  });

  test("template-literal interpolation calls Symbol.toPrimitive → masked repr", () => {
    const v = new MaskedValue<string>(makeRepr(), meta);
    assert.equal(`SSN: ${v}`, "SSN: ***-**-6789");
  });

  test("unmask() throws unmask_not_implemented in PR 1", async () => {
    const v = new MaskedValue<string>(makeRepr(), meta);
    await assert.rejects(
      () => v.unmask({ reason: "test" }),
      (e: Error & { code?: string }) => e.code === "unmask_not_implemented",
    );
  });

  test("canUnmask() throws unmask_not_implemented in PR 1", async () => {
    const v = new MaskedValue<string>(makeRepr(), meta);
    await assert.rejects(
      () => v.canUnmask(),
      (e: Error & { code?: string }) => e.code === "unmask_not_implemented",
    );
  });
});

describe("P5.5 PR 1 — Row<S> type inference (compile-time)", () => {
  // These tests live at the type layer; the runtime assertions are
  // soft (`Row<S>` resolves to the inferred shape per the type
  // system). A successful `tsc` build is the load-bearing assertion.

  test("masked encrypted field is wrapped in MaskedValue<string>", () => {
    const fields = {
      ssn: t.encrypted({ mode: "randomised" }).required(),
      name: t.string(),
    };
    type R = Row<typeof fields>;
    // Type-level assertion: `ssn` is MaskedValue<string>, `name` is string | undefined.
    // The lines below would fail to compile if the inference broke.
    const sample: R = {
      id: 1,
      createdAt: 0,
      updatedAt: 0,
      ssn: new MaskedValue<string>(
        { masked: "***", classification: "pii", sentinel: "__zsmask__" },
        { collection: "users", row_pk: "usr_xyz", column: "ssn" },
      ),
    };
    assert.ok(sample.ssn instanceof MaskedValue);
    // `name` is optional + bare string when present
    assert.equal(sample.name, undefined);
  });

  test("t.string().mask({ kind: 'none' }) stays bare string at the type level", () => {
    const fields = {
      email: t.string().mask({ kind: "none" }),
    };
    type R = Row<typeof fields>;
    const sample: R = {
      id: 1,
      createdAt: 0,
      updatedAt: 0,
      email: "plain@example.com",
    };
    assert.equal(typeof sample.email, "string");
  });

  test("explicit .mask({ kind: 'email' }) wraps in MaskedValue<string>", () => {
    const fields = {
      email: t.string().mask({ kind: "email" }).required(),
    };
    type R = Row<typeof fields>;
    const sample: R = {
      id: 1,
      createdAt: 0,
      updatedAt: 0,
      email: new MaskedValue<string>(
        { masked: "a****@example.com", classification: "pii", sentinel: "__zsmask__" },
        { collection: "users", row_pk: "usr_xyz", column: "email" },
      ),
    };
    assert.equal(sample.email.toString(), "a****@example.com");
  });
});
