// The fluent-only `table()` surface — behavioral suite (design
// `2026-06-25-op-dsl-fluent-redesign.md`). The byte-identity oracle lives in
// golden-parity.test.ts; this suite pins the surface BEHAVIOR the redesign adds:
//
//   - chaining + var-assign reuse (§4): terminals return the handle.
//   - the immutable `t.*` chain (§4): a hoisted type var does not alias.
//   - the SELECTOR_NOT_TERMINATED / SELECTOR_ALREADY_TERMINATED guard (§5).
//   - schema propagation + per-op override precedence (§3).
//   - guard pass-through (ifNotExists / ifExists → existenceGuard).
//   - EAGER recording (the terminal IS the recording).

import assert from "node:assert/strict";
import { test } from "node:test";

import { t, table } from "../src/index.js";
import { __begin, __drain } from "../src/ops.js";

function record(up: () => void): any[] {
  __begin();
  up();
  return __drain();
}

// ── §4 — chaining + var-assign reuse ──

test("CHAIN: terminals return the handle, so calls chain", () => {
  const ops = record(() => {
    table("users")
      .column("a").add({ type: t.text() })
      .column("b").drop({ ifExists: true })
      .insert({ rows: [{ a: "x" }] });
  });
  assert.deepEqual(
    ops.map((o) => o.op),
    ["addColumn", "dropColumn", "insert"],
  );
});

test("VAR-ASSIGN: a var-held handle is reusable across statements ({ schema } set once)", () => {
  const ops = record(() => {
    const users = table("users", { schema: "app" });
    users.column("email").add({ type: t.text().notNull() });
    users.unique("uq_email").add({ columns: ["email"] });
    users.insert({ rows: [{ id: "u1", email: "a@b.co" }] });
  });
  assert.equal(ops.length, 3);
  for (const op of ops) assert.equal(op.schema, "app", `op ${op.op} carries the handle schema`);
});

// ── §4 — the IMMUTABLE t.* chain ──

test("IMMUTABLE: a hoisted t.* type var does not alias across columns", () => {
  const ops = record(() => {
    const base = t.text().notNull();
    table("u")
      .column("a").add({ type: base.unique() }) // a is unique
      .column("b").add({ type: base }); // b is NOT unique (base untouched)
  });
  // a: addColumn + a follow-on unique constraint (C2).
  assert.equal(ops[0].op, "addColumn");
  assert.equal(ops[0].column, "a");
  assert.equal(ops[1].op, "addConstraint");
  assert.deepEqual(ops[1].constraint, { kind: { kind: "unique", columns: ["a"] } });
  // b: a plain addColumn, no unique — proving base.unique() did NOT mutate base.
  assert.equal(ops[2].op, "addColumn");
  assert.equal(ops[2].column, "b");
  assert.equal(ops.length, 3, "b must NOT emit a unique constraint");
});

test("IMMUTABLE: each modifier returns a fresh ColumnDef (no receiver mutation)", () => {
  const ops = record(() => {
    const c1 = t.int();
    const c2 = c1.notNull();
    const c3 = c2.default(0);
    table("u").create({ columns: { a: c1, b: c2, c: c3 } });
  });
  const [a, b, c] = ops[0].columns;
  assert.equal(a.nullable, undefined, "c1 stayed nullable (notNull returned a fresh def)");
  assert.equal(b.nullable, false);
  assert.equal(b.default, undefined, "c2 has no default (default returned a fresh def)");
  assert.equal(c.nullable, false);
  assert.deepEqual(c.default, { literal: { value: 0 } });
});

// ── §5 — the SELECTOR_NOT_TERMINATED guard ──

test("SELECTOR: a forgotten terminal throws SELECTOR_NOT_TERMINATED at drain", () => {
  assert.throws(
    () =>
      record(() => {
        table("u").column("email"); // no terminal
      }),
    (e: any) => e.code === "SELECTOR_NOT_TERMINATED" && e.selector === "column" && e.name === "email",
  );
});

test("SELECTOR: a var-held selector terminated on a LATER line does NOT trip the guard (§5)", () => {
  const ops = record(() => {
    const sel = table("u").column("email");
    table("u").insert({ rows: [{ x: 1 }] }); // an intervening op
    sel.add({ type: t.text() }); // terminated later — fine (drain-time check)
  });
  assert.deepEqual(
    ops.map((o) => o.op),
    ["insert", "addColumn"],
  );
});

test("SELECTOR: every selector kind trips the guard when its terminal is forgotten", () => {
  const kinds: Array<[string, (h: ReturnType<typeof table>) => unknown]> = [
    ["column", (h) => h.column("c")],
    ["foreignKey", (h) => h.foreignKey("fk")],
    ["unique", (h) => h.unique("uq")],
    ["check", (h) => h.check("ck")],
    ["constraint", (h) => h.constraint("cn")],
    ["index", (h) => h.index("ix")],
  ];
  for (const [kind, mk] of kinds) {
    assert.throws(
      () =>
        record(() => {
          mk(table("u"));
        }),
      (e: any) => e.code === "SELECTOR_NOT_TERMINATED" && e.selector === kind,
      `forgotten .${kind}() must throw SELECTOR_NOT_TERMINATED`,
    );
  }
});

test("SELECTOR: terminating twice throws SELECTOR_ALREADY_TERMINATED", () => {
  assert.throws(
    () =>
      record(() => {
        const sel = table("u").column("email");
        sel.add({ type: t.text() });
        sel.add({ type: t.text() }); // double-terminate
      }),
    (e: any) => e.code === "SELECTOR_ALREADY_TERMINATED" && e.selector === "column",
  );
});

test("SELECTOR: a terminated selector records its op; all selectors terminated ⇒ no error", () => {
  const ops = record(() => {
    const u = table("u");
    u.column("a").add({ type: t.text() });
    u.foreignKey("fk").add({ columns: ["a"], references: { table: "o", columns: ["id"] } });
    u.unique("uq").add({ columns: ["a"] });
    u.check("ck").add({ expr: (c) => c("a").isNotNull() });
    u.index("ix").add({ on: ["a"] });
    u.constraint("cn").drop();
  });
  assert.deepEqual(
    ops.map((o) => o.op),
    ["addColumn", "addConstraint", "addConstraint", "addConstraint", "createIndex", "dropConstraint"],
  );
});

test("COLUMN ALTER: per-intent terminals replace the old alter bag", () => {
  record(() => {
    const col = table("u").column("age") as any;
    assert.equal(col.alter, undefined, ".column(name).alter must not exist");
    col.drop();
  });

  const ops = record(() => {
    const u = table("u", { schema: "app2" });
    u.column("age").setType({ to: t.bigInt() });
    u.column("age").setNotNull();
    u.column("age").dropNotNull();
    u.column("age").setDefault(42);
    u.column("age").dropDefault();
  });

  assert.deepEqual(
    ops.map((o) => o.op),
    [
      "setColumnType",
      "setColumnNotNull",
      "dropColumnNotNull",
      "setColumnDefault",
      "dropColumnDefault",
    ],
  );
  assert.equal(ops.length, 5, "each terminal records exactly one op");
  assert.deepEqual(ops[0], {
    op: "setColumnType",
    table: "u",
    column: "age",
    toType: "bigInt",
    schema: "app2",
  });
  assert.deepEqual(ops[1], {
    op: "setColumnNotNull",
    table: "u",
    column: "age",
    schema: "app2",
  });
  assert.deepEqual(ops[2], {
    op: "dropColumnNotNull",
    table: "u",
    column: "age",
    schema: "app2",
  });
  assert.deepEqual(ops[3], {
    op: "setColumnDefault",
    table: "u",
    column: "age",
    value: { literal: { value: 42 } },
    schema: "app2",
  });
  assert.deepEqual(ops[4], {
    op: "dropColumnDefault",
    table: "u",
    column: "age",
    schema: "app2",
  });
});

// ── §3 — schema propagation + precedence ──

test("SCHEMA: the table() default propagates onto every recorded op", () => {
  const ops = record(() => {
    const u = table("users", { schema: "app2" });
    u.column("a").add({ type: t.int() });
    u.column("b").drop();
    u.index("ix_a").add({ on: ["a"] });
    u.index("ix_b").drop();
    u.insert({ rows: [{ a: 1 }] });
    u.update({ set: { a: (c) => c("a") } });
    u.delete({ where: (c) => c("a").gt(0) });
    u.backfill({ set: { a: (c) => c("a") } });
  });
  for (const op of ops) assert.equal(op.schema, "app2", `op ${op.op} must carry the table default schema`);
});

test("SCHEMA: a per-op schema OVERRIDES the table default", () => {
  const ops = record(() => {
    const u = table("users", { schema: "app2" });
    u.column("a").add({ type: t.int(), schema: "other" });
    u.index("ix_a").add({ on: ["a"], schema: "idx_schema" });
    u.insert({ rows: [{ a: 1 }], schema: "dml_schema" });
  });
  assert.equal(ops[0].schema, "other");
  assert.equal(ops[1].schema, "idx_schema");
  assert.equal(ops[2].schema, "dml_schema");
});

test("SCHEMA: a per-op args bag WITHOUT a schema key keeps the table default", () => {
  const ops = record(() => {
    const u = table("users", { schema: "app2" });
    u.column("a").add({ type: t.int(), ifNotExists: true }); // guard only, no schema
    u.column("b").drop({ ifExists: true });
  });
  assert.equal(ops[0].schema, "app2", "guard-only args must not wipe the table default");
  assert.equal(ops[1].schema, "app2");
});

test("SCHEMA: table() with no schema records ops with NO schema key", () => {
  const ops = record(() => table("users").column("a").add({ type: t.int() }));
  assert.ok(!("schema" in ops[0]), "no schema default ⇒ schema key omitted");
});

// ── guard pass-through ──

test("GUARD: ifNotExists / ifExists pass through to existenceGuard", () => {
  const ops = record(() => {
    const u = table("users");
    u.column("a").add({ type: t.int(), ifNotExists: true });
    u.column("b").drop({ ifExists: true });
    u.create({ columns: { id: t.id() }, ifNotExists: true });
    u.drop({ ifExists: true });
    u.index("ix_a").add({ on: ["a"], ifNotExists: true });
    u.index("ix_b").drop({ ifExists: true });
  });
  assert.equal(ops[0].existenceGuard, "ifNotExists");
  assert.equal(ops[1].existenceGuard, "ifExists");
  assert.equal(ops[2].existenceGuard, "ifNotExists");
  assert.equal(ops[3].existenceGuard, "ifExists");
  assert.equal(ops[4].existenceGuard, "ifNotExists");
  assert.equal(ops[5].existenceGuard, "ifExists");
});

test("GUARD: no guard option ⇒ existenceGuard omitted", () => {
  const ops = record(() => table("users").column("a").add({ type: t.int() }));
  assert.ok(!("existenceGuard" in ops[0]), "absent guard ⇒ existenceGuard key omitted");
});

// ── EAGER ──

test("EAGER: a single terminal records immediately — the terminal IS the recording", () => {
  const ops = record(() => {
    table("users").column("age").add({ type: t.int() });
  });
  assert.equal(ops.length, 1);
  assert.equal(ops[0].op, "addColumn");
  assert.equal(ops[0].table, "users");
  assert.equal(ops[0].column, "age");
});
