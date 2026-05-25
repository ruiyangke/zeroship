/**
 * **P5.5 PR 1** — masking foundation: `t.string().mask(...)` /
 * `t.encrypted().mask(...)` DSL modifier + default-mask rule for
 * encrypted columns + TypeScript `Row<S>` inference wrapping masked
 * fields.
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
 *      union, object) with `MASK_ON_UNSUPPORTED_TYPE`.
 *   5. `.mask()` on `t.ref()` refuses with
 *      `ENCRYPTED_ON_REF_UNSUPPORTED`.
 *   6. Invalid `kind` / `classification` are refused.
 *   7. TypeScript `Row<S>` inference wraps masked fields in
 *      `MaskedValue<T>` (compile-time assertion).
 *
 * **P9 PR 2** — the `MaskedValue` coercion / `unmask` / `canUnmask`
 * runtime tests that used to live here were removed: `MaskedValue` is
 * now a native v8_class minted Rust-side (the SDK export is a type-only
 * `declare class`), so those invariants are pinned by the Rust unit
 * tests in `masked_value.rs` + the `p9-pr2-masked-value-v8-class`
 * suite, not by `new MaskedValue(...)` in JS.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t } from "@zeroship/db";
// **P9 PR 2** — `MaskedValue` is now a native v8_class; the SDK export
// is a type-only `declare class`. It can only be imported as a type
// (no runtime constructor). Its runtime behaviour (coercion, unmask,
// brand check) is covered by the Rust unit tests in
// `crates/plugin-db/src/v8_classes/masked_value.rs` and the
// `p9-pr2-masked-value-v8-class` suite.
import type { Row, MaskedValueRepr, MaskedValue } from "@zeroship/db";

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
      (e: Error & { code?: string }) => e.code === "MASK_ON_UNSUPPORTED_TYPE",
    );
  });

  test(".mask() on t.boolean() rejects with mask_on_unsupported_type", () => {
    assert.throws(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      () => (t.boolean() as any).mask({ kind: "full" }),
      (e: Error & { code?: string }) => e.code === "MASK_ON_UNSUPPORTED_TYPE",
    );
  });

  test(".mask() on t.ref('users') rejects with encrypted_on_ref_unsupported (Q-P5-I)", () => {
    assert.throws(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      () => (t.ref("users") as any).mask({ kind: "full" }),
      (e: Error & { code?: string }) => e.code === "ENCRYPTED_ON_REF_UNSUPPORTED",
    );
  });

  test("invalid mask kind rejects with mask_invalid_kind", () => {
    assert.throws(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      () => (t.string() as any).mask({ kind: "bogus" }),
      (e: Error & { code?: string }) => e.code === "MASK_INVALID_KIND",
    );
  });

  test("invalid classification rejects with mask_invalid_classification", () => {
    assert.throws(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      () => (t.string() as any).mask({ kind: "full", classification: "bogus" }),
      (e: Error & { code?: string }) => e.code === "MASK_INVALID_CLASSIFICATION",
    );
  });

  test("opts = null rejects with mask_invalid_opts", () => {
    assert.throws(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      () => (t.string() as any).mask(null),
      (e: Error & { code?: string }) => e.code === "MASK_INVALID_OPTS",
    );
  });
});

describe("P5.5 PR 1 — Row<S> type inference (compile-time)", () => {
  // **P9 PR 2** — `MaskedValue` is a type-only `declare class`, so the
  // masked-column slots below use `as unknown as MaskedValue<string>`
  // casts rather than `new MaskedValue(...)`. The load-bearing
  // assertion is that `tsc` accepts these assignments — i.e. `Row<S>`
  // still infers the masked column as `MaskedValue<T>`, not bare `T`.
  // (A `MaskedValueRepr`-typed value is used to keep the import live and
  // double as documentation of the wire shape.)

  test("masked encrypted field is wrapped in MaskedValue<string>", () => {
    const fields = {
      ssn: t.encrypted({ mode: "randomised" }).required(),
      name: t.string(),
    };
    type R = Row<typeof fields>;
    // Type-level assertion: `ssn` is MaskedValue<string>, `name` is
    // string | undefined. The line below would fail to compile if the
    // inference broke (e.g. a bare string assigned to `ssn`).
    const repr: MaskedValueRepr = {
      masked: "***",
      classification: "pii",
      sentinel: "__zsmask__",
    };
    const sample: R = {
      id: 1,
      created_at: 0,
      updated_at: 0,
      ssn: repr as unknown as MaskedValue<string>,
    };
    // Runtime sanity: the cast value's masked field is reachable.
    assert.equal((sample.ssn as unknown as MaskedValueRepr).masked, "***");
    // `name` is optional + bare string when present.
    assert.equal(sample.name, undefined);
  });

  test("t.string().mask({ kind: 'none' }) stays bare string at the type level", () => {
    const fields = {
      email: t.string().mask({ kind: "none" }),
    };
    type R = Row<typeof fields>;
    const sample: R = {
      id: 1,
      created_at: 0,
      updated_at: 0,
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
      created_at: 0,
      updated_at: 0,
      email: "a****@example.com" as unknown as MaskedValue<string>,
    };
    // The slot types as MaskedValue<string>; tsc would reject a bare
    // string here without the cast — that's the compile-time invariant.
    assert.equal(sample.email as unknown as string, "a****@example.com");
  });
});
