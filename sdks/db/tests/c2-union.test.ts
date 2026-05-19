/**
 * @zeroship/db — Phase 7 / C2: discriminated union document shapes.
 *
 * Exercises `t.literal()`, `t.union(...)`, the auto-detected
 * discriminator, validation dispatch, and the normalized flat-column
 * expansion that hands off to the Rust DDL emitter.
 *
 * Postgres-side DDL coverage lives in
 * `crates/plugin-db/src/query.rs` golden tests. TypeScript discriminated
 * narrowing is exercised in `tests/typecheck.ts`.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t } from "../src/types.js";
import { normalizeSchema, expandUnionToFlatColumns } from "../src/schema.js";
import { validateDoc } from "../src/validate.js";
import { ValidationError } from "../src/errors.js";

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
// Flat-column expansion (normalizeSchema accepts a top-level union)
// ---------------------------------------------------------------------------

describe("C2 — normalizeSchema flat expansion", () => {
  test("top-level t.union expands to flat columns + discriminator", () => {
    const events = t.union(
      t.object({ kind: t.literal("login"), userId: t.number().required(), ip: t.string().required() }),
      t.object({ kind: t.literal("error"), message: t.string().required() }),
      t.object({ kind: t.literal("metric"), name: t.string().required(), value: t.number().required() }),
    );
    const flat = normalizeSchema(events);

    // Discriminator column
    assert.ok(flat.kind);
    assert.equal(flat.kind.type, "string");
    assert.equal(flat.kind.required, true);
    assert.deepEqual(flat.kind.enum, ["login", "error", "metric"]);
    assert.equal(flat.kind.discriminator, "__discriminator__");
    assert.ok(flat.kind.variants);
    assert.equal(flat.kind.variants!.length, 3);

    // Non-discriminator columns are present at the top level — each
    // nullable since they only apply to a subset of variants.
    for (const k of ["userId", "ip", "message", "name", "value"]) {
      assert.ok(flat[k], `expected flat column ${k}`);
      assert.notEqual(flat[k].required, true, `${k} must be column-level nullable`);
    }
  });

  test("fields shared across variants with the same type dedupe", () => {
    // Both `login` and `error` carry an `ip` string — must collapse.
    const events = t.union(
      t.object({ kind: t.literal("login"), ip: t.string().required() }),
      t.object({ kind: t.literal("error"), ip: t.string(), message: t.string().required() }),
    );
    const flat = normalizeSchema(events);
    assert.ok(flat.ip);
    assert.equal(flat.ip.type, "string");
    assert.notEqual(flat.ip.required, true);
  });

  test("fields shared across variants with conflicting types throw", () => {
    assert.throws(
      () =>
        normalizeSchema(
          t.union(
            t.object({ kind: t.literal("a"), x: t.string() }),
            t.object({ kind: t.literal("b"), x: t.number() }),
          ),
        ),
      /incompatible types/,
    );
  });

  test("top-level non-union TypeBuilder is rejected", () => {
    // A bare t.string() at the top level isn't a valid schema input —
    // it must be a union (top-level row shape).
    assert.throws(
      () => normalizeSchema(t.string() as unknown as Parameters<typeof normalizeSchema>[0]),
      /must be a t\.union/,
    );
  });
});

// ---------------------------------------------------------------------------
// Validation — discriminator dispatch
// ---------------------------------------------------------------------------

describe("C2 — validation dispatch on discriminator", () => {
  function eventsSchema() {
    return normalizeSchema(
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
// Nested t.union() inside t.object() — validation passes through
// ---------------------------------------------------------------------------

describe("C2 — nested t.union() inside t.object()", () => {
  test("nested union dispatch via t.object validator", () => {
    const s = normalizeSchema({
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

// ---------------------------------------------------------------------------
// expandUnionToFlatColumns — direct unit
// ---------------------------------------------------------------------------

describe("C2 — expandUnionToFlatColumns", () => {
  test("produces the same shape as normalizeSchema(t.union(...))", () => {
    const u = t.union(
      t.object({ kind: t.literal("a"), x: t.string() }),
      t.object({ kind: t.literal("b"), y: t.number() }),
    );
    const def = u.toFieldDef();
    const flat = expandUnionToFlatColumns(def as Parameters<typeof expandUnionToFlatColumns>[0]);
    assert.ok(flat.kind);
    assert.ok(flat.x);
    assert.ok(flat.y);
  });

  test("rejects a non-union def", () => {
    assert.throws(() =>
      expandUnionToFlatColumns({ type: "string" } as Parameters<typeof expandUnionToFlatColumns>[0]),
    );
  });
});
