import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "../src/index.js";
import { __begin, __drain, opProducerRegistry } from "../src/ops.js";
import { pgTable } from "../src/pg.js";

function record(up: () => void): any[] {
  __begin();
  up();
  return __drain();
}

test("tier-1 op producer census allows only disjoint addConstraint slots", () => {
  // Tier-1 rule: every op kind should have one producer-function identity.
  // Disjoint-sub-kind producers are census-legal only when documented here,
  // so an accidental second spelling for the same (op kind, sub-kind) fails.
  const allowedMultiProducerSlots = new Map<string, ReadonlyMap<string, string>>([
    [
      "addConstraint",
      new Map([
        ["addColumn.unique", "column-unique"],
        ["foreignKey", "foreignKey"],
        ["unique", "unique"],
        ["check", "check"],
        ["exclusion", "exclusion"],
      ]),
    ],
  ]);

  const multiProducerKinds: string[] = [];

  for (const [kind, producers] of opProducerRegistry()) {
    if (producers.length === 1) continue;

    multiProducerKinds.push(kind);
    const allowedSlots = allowedMultiProducerSlots.get(kind);
    assert.ok(allowedSlots, `${kind} has ${producers.length} producers but is not census-allowlisted`);

    const producerNames = producers.map((producer) => producer.producer).sort();
    assert.deepEqual(
      producerNames,
      [...allowedSlots.keys()].sort(),
      `${kind} producer identities must match the documented disjoint slots`,
    );

    const slots = producers.map((producer) => allowedSlots.get(producer.producer));
    assert.equal(
      new Set(slots).size,
      producers.length,
      `${kind} producers must map one-to-one to disjoint sub-kind slots`,
    );
  }

  assert.deepEqual(multiProducerKinds.sort(), ["addConstraint"]);
});

test("tier-2 addConstraint byte collision check covers serialized constraint kinds", () => {
  const ops = record(() => {
    table("orders").foreignKey("orders_customer_fk").add({
      columns: ["customer_id"],
      references: { table: "customers", columns: ["id"] },
    });
    table("users").unique("users_email_key").add({ columns: ["email"] });
    table("users").check("users_email_present").add({ expr: (col) => col("email").isNotNull() });
    pgTable("bookings").exclusion("bookings_room_excl").add({
      using: "gist",
      elements: [{ target: "room_id", operator: "=" }],
    });
  });

  const constraintKinds = ops.map((op) => op.constraint.kind.kind);
  assert.deepEqual([...constraintKinds].sort(), ["check", "exclusion", "fk", "unique"]);
  assert.equal(new Set(constraintKinds).size, constraintKinds.length);
});

test("column-unique addConstraint slot remains byte-distinguishable from named unique", () => {
  const ops = record(() => {
    table("users").column("email").add({ type: t.text().unique() });
    table("users").unique("users_email_key").add({ columns: ["email"] });
  }).filter((op) => op.op === "addConstraint");

  assert.equal(ops.length, 2);
  assert.deepEqual(ops[0].constraint, { kind: { kind: "unique", columns: ["email"] } });
  assert.deepEqual(ops[1].constraint, { name: "users_email_key", kind: { kind: "unique", columns: ["email"] } });
});
