import assert from "node:assert/strict";
import { test } from "node:test";

import {
  __begin as pubBegin,
  __drain as pubDrain,
  t as pubT,
  table as pubTable,
} from "../src/ops.js";
import { sequence as pubSequence } from "../src/pg.js";
import {
  __begin as engBegin,
  __drain as engDrain,
  __pgSequence as engSequence,
  t as engT,
  table as engTable,
} from "../../../crates/zeroship-migrate/src/frontend/migrate_ops.js";

type Rec = {
  begin: () => void;
  drain: () => any[];
  sequence: any;
  t: any;
  table: any;
};

const PUBLIC: Rec = {
  begin: pubBegin,
  drain: pubDrain,
  sequence: pubSequence,
  t: pubT,
  table: pubTable,
};

const ENGINE: Rec = {
  begin: engBegin,
  drain: engDrain,
  sequence: engSequence,
  t: engT,
  table: engTable,
};

function authorWith({ begin, drain, sequence, t, table }: Rec): any[] {
  begin();
  sequence("invoice_seq").create({
    as: t.bigInt(),
    increment: 5,
    start: 100,
    cache: 10,
    cycle: true,
    ownedBy: { table: "invoices", column: "id" },
    schema: "app",
  });
  sequence("invoice_seq").alter({
    increment: 7,
    restart: 200,
    minValue: 1,
    maxValue: 999,
    cache: 20,
    cycle: false,
    ownedBy: null,
    schema: "app",
  });
  sequence("invoice_seq").drop({ ifExists: true, schema: "app" });

  table("bookings", { schema: "app" }).create({
    columns: {
      room: t.text(),
      during: t.text(),
      cancelled: t.boolean(),
    },
    exclusions: [{
      name: "bookings_no_overlap",
      using: "gist",
      elements: [
        { target: "room", operator: "=" },
        { target: "during", operator: "&&" },
      ],
      where: (c: any) => c("cancelled").eq(false),
      deferrable: true,
    }],
  });
  table("bookings", { schema: "app" }).exclusion("bookings_no_overlap").add({
    using: "gist",
    elements: [
      { target: "room", operator: "=" },
      { target: "during", operator: "&&" },
    ],
    where: (c: any) => c("cancelled").eq(false),
    deferrable: true,
    ifNotExists: true,
  });
  return drain();
}

test("sequences and exclusion constraints record byte-identically to engine recorder", () => {
  assert.deepEqual(authorWith(PUBLIC), authorWith(ENGINE));
});

test("sequence and exclusion recorder emits the canonical IR shape", () => {
  const ops = authorWith(PUBLIC);
  assert.deepEqual(ops.slice(0, 3), [
    {
      op: "createSequence",
      name: "invoice_seq",
      schema: "app",
      as: "bigInt",
      increment: 5,
      start: 100,
      cache: 10,
      cycle: true,
      ownedBy: { table: "invoices", column: "id" },
    },
    {
      op: "alterSequence",
      name: "invoice_seq",
      schema: "app",
      increment: 7,
      restart: 200,
      minValue: 1,
      maxValue: 999,
      cache: 20,
      cycle: false,
      ownedBy: null,
    },
    {
      op: "dropSequence",
      name: "invoice_seq",
      schema: "app",
      existenceGuard: "ifExists",
    },
  ]);

  const inline = ops[3].constraints[0];
  assert.deepEqual(inline.kind.elements, [
    { target: { kind: "column", name: "room" }, operator: "=" },
    { target: { kind: "column", name: "during" }, operator: "&&" },
  ]);
  assert.equal(inline.kind.kind, "exclusion");
  assert.equal(inline.kind.usingMethod, "gist");
  assert.equal(inline.kind.deferrable, true);

  const standalone = ops[4];
  assert.equal(standalone.op, "addConstraint");
  assert.equal(standalone.existenceGuard, "ifNotExists");
  assert.equal(standalone.constraint.kind.kind, "exclusion");
});

function assertInvalidSequenceOption(rec: Rec, author: (sequence: any) => void, pattern: RegExp) {
  rec.begin();
  assert.throws(() => author(rec.sequence), pattern);
  assert.deepEqual(rec.drain(), []);
}

test("sequence recorder rejects invalid numeric options in public and engine copies", () => {
  for (const rec of [PUBLIC, ENGINE]) {
    assertInvalidSequenceOption(
      rec,
      (sequence) => sequence("bad_seq").create({ increment: 0 }),
      /increment.*non-zero/,
    );
    assertInvalidSequenceOption(
      rec,
      (sequence) => sequence("bad_seq").alter({ cache: 0 }),
      /cache.*positive/,
    );
    assertInvalidSequenceOption(
      rec,
      (sequence) => sequence("bad_seq").create({ minValue: 10, maxValue: 9 }),
      /minValue.*<= maxValue/,
    );
    assertInvalidSequenceOption(
      rec,
      (sequence) => sequence("bad_seq").create({ start: Number.MAX_SAFE_INTEGER + 1 }),
      /safe integer/,
    );
  }
});
