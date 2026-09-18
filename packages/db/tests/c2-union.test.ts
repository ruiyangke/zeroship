/**
 * @zeroship/db — Phase 7 / C2: discriminated union document shapes.
 *
 * Exercises `t.literal()`, `t.union(...)`, the auto-detected
 * discriminator, and validator dispatch over the flat projection the Rust
 * migration fold emits for a top-level union.
 *
 * ORM value validation is covered by the typed codec tests.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { Collection } from "../../../crates/zeroship-data-v8/js/runtime/collection.js";
import { t, ValidationError, type FieldDef } from "../src/index.js";
// Import validation helpers through crate test support so thrown errors share
// the source entry's module instance.
import { validateDoc, checkPartial } from "../../../crates/zeroship-data-v8/js/testing.js";
import { fieldsOf } from "./_install-helper.js";
import type { NativeDb } from "../src/native.js";

// ---------------------------------------------------------------------------
// t.literal() — primitive literal field
// ---------------------------------------------------------------------------

describe("C2 — t.literal()", () => {
  test("t.literal('login') produces FieldDef.type === 'literal' with literalValue", () => {
    const def = t.literal("login").toFieldDef();
    assert.equal(def.type, "literal");
    assert.equal(def.literalValue, "login");
    // Literal fields are implicitly required so the variant always
    // carries its discriminator value.
    assert.equal(def.required, true);
  });

  test("t.literal accepts string, number, boolean", () => {
    assert.equal(t.literal("x").toFieldDef().literalValue, "x");
    assert.equal(t.literal(42).toFieldDef().literalValue, 42);
    assert.equal(t.literal(true).toFieldDef().literalValue, true);
  });

  test("t.literal rejects null / undefined / object", () => {
    // @ts-expect-error — null not allowed
    assert.throws(() => t.literal(null));
    // @ts-expect-error — undefined not allowed
    assert.throws(() => t.literal(undefined));
    // @ts-expect-error — object not allowed
    assert.throws(() => t.literal({}));
  });
});

// ---------------------------------------------------------------------------
// t.union() — discriminator auto-detection
// ---------------------------------------------------------------------------

describe("C2 — t.union() discriminator auto-detection", () => {
  test("auto-detects 'kind' when every variant uses it as a literal", () => {
    const def = t
      .union(
        t.object({ kind: t.literal("login"), ip: t.string() }),
        t.object({ kind: t.literal("error"), message: t.string() }),
      )
      .toFieldDef();
    assert.equal(def.type, "union");
    assert.equal(def.discriminator, "kind");
    assert.ok(def.variants);
    assert.equal(def.variants!.length, 2);
  });

  test("rejects union with fewer than 2 variants", () => {
    assert.throws(
      () => t.union(t.object({ kind: t.literal("only") })),
      /at least 2 variants/,
    );
  });

  test("rejects variant that isn't a t.object()", () => {
    assert.throws(
      () =>
        t.union(
          t.object({ kind: t.literal("a") }),
          t.string() as unknown as ReturnType<typeof t.object>,
        ),
      /must be a t\.object/,
    );
  });

  test("rejects when no field is literal in every variant", () => {
    assert.throws(
      () =>
        t.union(
          t.object({ kind: t.literal("a"), x: t.string() }),
          t.object({ kind: t.string(), y: t.string() }),
        ),
      /no discriminator field found/,
    );
  });

  test("rejects ambiguous discriminator (two candidate literals)", () => {
    // Both `kind` and `tag` are literals in every variant with distinct
    // values — ambiguous, must throw.
    assert.throws(
      () =>
        t.union(
          t.object({ kind: t.literal("a"), tag: t.literal("x") }),
          t.object({ kind: t.literal("b"), tag: t.literal("y") }),
        ),
      /ambiguous discriminator/,
    );
  });

  test("rejects when literal values are not distinct", () => {
    // Both variants use `kind: "same"` — can't dispatch on the
    // discriminator. With no other literal field that's distinct, this
    // is rejected.
    assert.throws(
      () =>
        t.union(
          t.object({ kind: t.literal("same"), a: t.string() }),
          t.object({ kind: t.literal("same"), b: t.string() }),
        ),
    );
  });
});

// ---------------------------------------------------------------------------
// Validation — discriminator dispatch
// ---------------------------------------------------------------------------

/**
 * The flat projection Rust's migration fold emits for a top-level
 * `t.union(...)`: the discriminator column the runtime validator dispatches
 * on. The JS side no longer expands unions, so this fixture stands in for the
 * Rust output and keeps validator dispatch covered.
 */
function flatUnionSchema(union: { toFieldDef(): FieldDef }): Record<string, FieldDef> {
  const def = union.toFieldDef();
  const discriminator = def.discriminator;
  const variants = def.variants;
  if (discriminator === undefined || variants === undefined) {
    throw new Error("flatUnionSchema: expected a discriminated union FieldDef");
  }
  return {
    [discriminator]: {
      type: "string",
      required: true,
      enum: variants.map((variant) => variant[discriminator].literalValue) as (string | number)[],
      discriminator: "__discriminator__",
      variants,
    },
  };
}

describe("C2 — validation dispatch on discriminator", () => {
  function eventsSchema() {
    return flatUnionSchema(
      t.union(
        t.object({
          kind: t.literal("login"),
          userId: t.number().required(),
          ip: t.string().required(),
        }),
        t.object({
          kind: t.literal("error"),
          message: t.string().required(),
          stack: t.string(),
        }),
        t.object({
          kind: t.literal("metric"),
          name: t.string().required(),
          value: t.number().required(),
        }),
      ),
    );
  }

  test("c2_union_validate_login_variant — valid login event passes", () => {
    const s = eventsSchema();
    const out = validateDoc({ kind: "login", userId: 7, ip: "10.0.0.1" }, s);
    assert.equal(out.kind, "login");
    assert.equal(out.userId, 7);
    assert.equal(out.ip, "10.0.0.1");
    // Other variants' fields are stripped
    assert.equal(out.message, undefined);
    assert.equal(out.name, undefined);
  });

  test("c2_union_validate_error_variant — valid error event passes", () => {
    const s = eventsSchema();
    const out = validateDoc({ kind: "error", message: "boom" }, s);
    assert.equal(out.kind, "error");
    assert.equal(out.message, "boom");
    // Optional `stack` absent → stripped
    assert.equal(out.stack, undefined);
  });

  test("c2_union_rejects_wrong_discriminator — kind: 'unknown' fails", () => {
    const s = eventsSchema();
    try {
      validateDoc({ kind: "unknown", message: "x" }, s);
      assert.fail("should have thrown");
    } catch (e) {
      assert.ok(e instanceof ValidationError);
      assert.ok("kind" in e.errors);
      assert.match(e.errors.kind.message, /login.*error.*metric/);
    }
  });

  test("c2_union_rejects_missing_required_in_variant — login without userId fails", () => {
    const s = eventsSchema();
    try {
      validateDoc({ kind: "login", ip: "1.2.3.4" }, s);
      assert.fail("should have thrown");
    } catch (e) {
      assert.ok(e instanceof ValidationError);
      assert.ok("userId" in e.errors, `expected userId error in ${JSON.stringify(e.errors)}`);
      assert.match(e.errors.userId.message, /required/);
    }
  });

  test("rejects missing discriminator value", () => {
    const s = eventsSchema();
    assert.throws(() => validateDoc({ userId: 1, ip: "x" }, s), ValidationError);
  });

  test("rejects type mismatch within matched variant", () => {
    const s = eventsSchema();
    try {
      validateDoc({ kind: "metric", name: "rps", value: "not a number" }, s);
      assert.fail("should have thrown");
    } catch (e) {
      assert.ok(e instanceof ValidationError);
      assert.ok("value" in e.errors);
    }
  });

  test("strips fields from the wrong variant", () => {
    const s = eventsSchema();
    // `message` is on the error variant; on a login event it should
    // be silently dropped (consistent with top-level validateDoc
    // stripping unknown keys for safety).
    const out = validateDoc(
      { kind: "login", userId: 1, ip: "x", message: "leak?" },
      s,
    );
    assert.equal(out.message, undefined);
  });
});

// ---------------------------------------------------------------------------
// Partial-update validation (`checkPartial` via Collection.update)
// ---------------------------------------------------------------------------

describe("C2 — partial update against a flat-expanded union", () => {
  function makeMockNative() {
    const native = {
      collection(_name: string) {
        return {
          async update(_f: Record<string, unknown>, _u: Record<string, unknown>) {
            return { id: 1, kind: "login", userId: 1, ip: "x" };
          },
        };
      },
    };
    return native as unknown as NativeDb;
  }

  test("update({id}, {kind: 'invalidLiteral'}) rejects with ValidationError on the enum guard", async () => {
    // The flat expansion turns the discriminator into a `string` column
    // with `enum: ["login", "error"]`. `checkPartial` exercises the
    // enum branch — invalid literals must reject.
    const eventsSchema = {
      ...flatUnionSchema(t.union(
        t.object({
          kind: t.literal("login"),
          userId: t.number().required(),
          ip: t.string().required(),
        }),
        t.object({
          kind: t.literal("error"),
          message: t.string().required(),
        }),
      )),
      id: t.string().required().primaryKey(),
    };
    const Events = new Collection<Record<string, unknown>>(
      "events",
      fieldsOf(eventsSchema),
      makeMockNative(),
    );
    const { data, error } = await Events.update({ id: "1" }, { kind: "invalidLiteral" });
    assert.equal(data, null);
    assert.ok(error);
    assert.ok(error instanceof ValidationError);
    assert.ok("kind" in (error as ValidationError).errors);
    assert.match(error.message, /kind.*login.*error/);
  });

  // Gap J — patching a valid discriminator value without re-stating
  // the new variant's required fields used to silently succeed and
  // leave the row in an inconsistent state.
  test("c2_union_gap_j_discriminator_only_patch_rejects_missing_variant_required", () => {
    const s = flatUnionSchema(
      t.union(
        t.object({
          kind: t.literal("login"),
          userId: t.number().required(),
        }),
        t.object({
          kind: t.literal("signup"),
          name: t.string().required(),
        }),
      ),
    );
    assert.throws(
      () => checkPartial({ kind: "signup" }, s),
      (e: unknown) => {
        assert.ok(e instanceof ValidationError);
        assert.ok("name" in e.errors, `expected name error, got ${JSON.stringify(e.errors)}`);
        assert.match(e.errors.name.message, /changing kind to "signup".*name/);
        return true;
      },
    );
  });

  test("c2_union_gap_j_discriminator_with_variant_fields_passes", () => {
    const s = flatUnionSchema(
      t.union(
        t.object({
          kind: t.literal("login"),
          userId: t.number().required(),
        }),
        t.object({
          kind: t.literal("signup"),
          name: t.string().required(),
        }),
      ),
    );
    assert.doesNotThrow(() =>
      checkPartial({ kind: "signup", name: "Ada" }, s),
    );
  });

  test("c2_union_gap_j_non_discriminator_patch_unaffected", () => {
    // A patch that doesn't touch the discriminator must not invoke
    // the Gap J variant-required check.
    const s = flatUnionSchema(
      t.union(
        t.object({
          kind: t.literal("login"),
          userId: t.number().required(),
        }),
        t.object({
          kind: t.literal("signup"),
          name: t.string().required(),
        }),
      ),
    );
    assert.doesNotThrow(() => checkPartial({ userId: 42 }, s));
  });

  test("c2_union_gap_j_multiple_missing_listed_in_message", () => {
    const s = flatUnionSchema(
      t.union(
        t.object({ kind: t.literal("a"), x: t.string().required() }),
        t.object({
          kind: t.literal("b"),
          y: t.string().required(),
          z: t.number().required(),
        }),
      ),
    );
    try {
      checkPartial({ kind: "b" }, s);
      assert.fail("should have thrown");
    } catch (e) {
      assert.ok(e instanceof ValidationError);
      assert.ok("y" in e.errors);
      assert.ok("z" in e.errors);
      assert.match(e.errors.y.message, /y.*z|z.*y/);
    }
  });
});

// ---------------------------------------------------------------------------
// Nested t.union() inside t.object() — validation passes through
// ---------------------------------------------------------------------------

describe("C2 — nested t.union() inside t.object()", () => {
  test("nested union dispatch via t.object validator", () => {
    const s = fieldsOf({
      payload: t.union(
        t.object({ kind: t.literal("a"), x: t.string().required() }),
        t.object({ kind: t.literal("b"), y: t.number().required() }),
      ),
    });
    assert.equal(s.payload.type, "union");
    assert.doesNotThrow(() =>
      validateDoc({ payload: { kind: "a", x: "ok" } }, s),
    );
    assert.throws(
      () => validateDoc({ payload: { kind: "a", y: 1 } }, s),
      ValidationError,
    );
    assert.throws(
      () => validateDoc({ payload: { kind: "nope" } }, s),
      ValidationError,
    );
  });
});
