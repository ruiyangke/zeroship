// Artifact-identity parity for the column-level facets (#173/#174/#178 +
// generated/identity): `t.id({ prefix })`, `t.vector({ dimensions, metric })`, standalone
// `t.text().mask({ kind, classification })`, `.generated(...)`, and `.identity(...)`.
//
// S0.5 collapsed the recorder twin: there is no longer a hand-kept
// `migrate_ops.js`. The SDK recorder (`src/ops.ts`) and the engine-embedded
// recorder (`dist/embedded-recorder.js`, the `tsup` build output the
// `zeroship-migrate` crate `include_str!`s into V8) are now the SAME source,
// compiled two ways. This test is the design's "one-release parity tripwire →
// artifact-identity assertion": re-author the SAME migration through BOTH the
// `ops.ts` SOURCE (`pub*`) and the COMPILED artifact (`eng*`), then assert the
// two recorded op lists are byte-identical — proving the shipped engine artifact
// records the EXACT camelCase wire form (`idPrefix` / `vectorMetric` /
// `mask:{kind,classification}` / `generated:{expr,stored}` / `identity:{always}`)
// the source authors, with no compile-time drift.

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  __begin as pubBegin,
  __drain as pubDrain,
  maxValue as pubMaxValue,
  minValue as pubMinValue,
  t as pubT,
  table as pubTable,
} from "../src/ops.js";
import { pgTable as pubPgTable } from "../src/pg.js";
// The COMPILED engine-embedded recorder artifact (the file the Rust runtime
// include_str!s into V8). Importing it directly makes this an oracle against the
// real shipped engine recording, not a self-referential restatement of the source.
import {
  __begin as engBegin,
  __drain as engDrain,
  maxValue as engMaxValue,
  minValue as engMinValue,
  pgTable as engPgTable,
  t as engT,
  table as engTable,
} from "../dist/embedded-recorder.js";

type Rec = {
  begin: () => void;
  drain: () => any[];
  pgTable: any;
  t: any;
  table: any;
  minValue: any;
  maxValue: any;
};

const PUBLIC: Rec = {
  begin: pubBegin,
  drain: pubDrain,
  pgTable: pubPgTable,
  t: pubT,
  table: pubTable,
  minValue: pubMinValue,
  maxValue: pubMaxValue,
};
const ENGINE: Rec = {
  begin: engBegin,
  drain: engDrain,
  pgTable: engPgTable,
  t: engT,
  table: engTable,
  minValue: engMinValue,
  maxValue: engMaxValue,
};

/** Author a facet-bearing migration against the given recorder + lexicon, return
 *  the recorded op list. The SAME author body runs against both impls. */
function authorWith({ begin, drain, t, table }: Rec): any[] {
  begin();
  // createTable carrying the column facets:
  //  - t.id({ prefix })            → IrColumn.idPrefix
  //  - t.vector({ dimensions, metric }) → IrColumn.vectorMetric (closed cosine|l2|innerProduct)
  //  - t.text().mask({ kind, classification }) → IrColumn.mask:{kind,classification}
  //  - t.int().generated(expr)     → IrColumn.generated:{expr,stored}
  //  - t.bigInt().identity(opts)   → IrColumn.identity:{always}
  table("documents").create({
    columns: {
      id: t.id({ prefix: "doc" }),
      seq: t.bigInt().identity({ always: true }),
      shard: t.smallInt(),
      qty: t.int(),
      unit_cents: t.int(),
      ratio: t.real(),
      source_ip: t.inet(),
      total_cents: t.int().generated((col: any) => col("qty").mul(col("unit_cents"))),
      virtual_total: t.int().generated((col: any) => col("qty").mul(col("unit_cents")), { virtual: true }),
      embedding: t.vector({ dimensions: 1536, metric: "cosine" }),
      // a standalone mask with an explicit classification
      ssn: t.text().mask({ kind: "last4", classification: "pci" }),
      // a standalone mask defaulting classification → "pii"
      email: t.text().mask({ kind: "email" }),
      title: t.text(),
    },
  });
  // addColumn carries vectorMetric + mask (NOT idPrefix — fail-closed on add):
  table("documents").column("summary_vec").add({ type: t.vector({ dimensions: 768, metric: "innerProduct" }) });
  table("documents").column("phone").add({ type: t.text().mask({ kind: "last4" }) });
  table("documents").column("added_total").add({
    type: t.int().generated((col: any) => col("qty").mul(col("unit_cents"))),
  });
  table("documents").column("added_seq").add({ type: t.bigInt().identity() });
  return drain();
}

test("public t.id/t.vector/.mask facets record byte-identically to the engine recorder", () => {
  const pub = authorWith(PUBLIC);
  const eng = authorWith(ENGINE);
  assert.deepEqual(pub, eng);
});

test("the recorded facets carry the exact camelCase wire form", () => {
  const ops = authorWith(PUBLIC);
  const create = ops[0];
  assert.equal(create.op, "createTable");
  const byName = (n: string) => create.columns.find((column: any) => column.name === n);

  // t.id({ prefix }) → idPrefix
  assert.equal(byName("id").idPrefix, "doc");

  // t.vector({ dimensions, metric }) → vectorMetric (closed token)
  assert.equal(byName("embedding").vectorMetric, "cosine");
  assert.equal(byName("shard").type, "smallInt");
  assert.equal(byName("ratio").type, "real");
  assert.equal(byName("source_ip").type, "inet");

  // standalone .mask({ kind, classification }) → mask:{kind,classification}
  assert.deepEqual(byName("ssn").mask, { kind: "last4", classification: "pci" });
  // classification defaults to "pii"
  assert.deepEqual(byName("email").mask, { kind: "email", classification: "pii" });

  // generated/identity facets carry their exact nested camelCase shape.
  assert.deepEqual(byName("seq").identity, { always: true });
  assert.deepEqual(byName("total_cents").generated, {
    expr: {
      node: "binOp",
      op: "mul",
      lhs: { node: "colRef", name: "qty" },
      rhs: { node: "colRef", name: "unit_cents" },
    },
    stored: true,
  });
  assert.equal(byName("virtual_total").generated.stored, false);

  // a facet-less column carries NONE of the facet keys (checksum-neutral).
  const title = byName("title");
  assert.ok(!("idPrefix" in title) && !("vectorMetric" in title) && !("mask" in title) && !("generated" in title) && !("identity" in title));

  // addColumn carries vectorMetric + mask + generated + identity on the op tail.
  const addVec = ops.find((o: any) => o.op === "addColumn" && o.column === "summary_vec");
  assert.equal(addVec.vectorMetric, "innerProduct");
  const addPhone = ops.find((o: any) => o.op === "addColumn" && o.column === "phone");
  assert.deepEqual(addPhone.mask, { kind: "last4", classification: "pii" });
  const addGenerated = ops.find((o: any) => o.op === "addColumn" && o.column === "added_total");
  assert.equal(addGenerated.generated.stored, true);
  const addIdentity = ops.find((o: any) => o.op === "addColumn" && o.column === "added_seq");
  assert.deepEqual(addIdentity.identity, { always: false });
});

function authorPartitionWith({
  begin,
  drain,
  pgTable,
  t,
  table,
  minValue,
  maxValue,
}: Rec): any[] {
  begin();
  table("events").create({
    columns: {
      ts: t.timestamp(),
      tenant_id: t.text(),
    },
    partitionBy: { range: ["ts"] },
  });
  table("events", { schema: "app" }).partition("events_2026_05").create({
    from: [minValue, "2026-05-01T00:00:00Z"],
    to: ["2026-06-01T00:00:00Z", maxValue],
  }, { ifNotExists: true });
  table("events").partition("events_default").create({ default: true });
  pgTable("events")
    .index("events_ts_brin_idx")
    .add({
      on: ["ts"],
      using: "brin",
      include: ["tenant_id"],
      with: { pagesPerRange: 32 },
      only: true,
    });
  pgTable("events", { schema: "app" }).partition("events_2026_05").detach({ concurrently: true });
  table("events", { schema: "app" }).partition("events_2026_05").drop({ ifExists: true, cascade: true });
  return drain();
}

test("partition DSL records byte-identically to the engine recorder", () => {
  assert.deepEqual(authorPartitionWith(PUBLIC), authorPartitionWith(ENGINE));
});

test("an out-of-set mask kind/classification/metric is a structured OP_INVALID (runtime guard)", () => {
  pubBegin();
  try {
    assert.throws(
      () => pubT.text().mask({ kind: "bogus" as any }),
      (e: any) => e.code === "OP_INVALID",
    );
    assert.throws(
      () => pubT.text().mask({ kind: "full", classification: "secret" as any }),
      (e: any) => e.code === "OP_INVALID",
    );
    assert.throws(
      () => pubT.vector(8, { metric: "manhattan" as any }),
      (e: any) => e.code === "OP_INVALID",
    );
  } finally {
    pubDrain();
  }
});

// A typed-id prefix on an ADDED column is fail-closed (an added column is never the
// system PK) — the public surface must REFUSE it with the same structured error
// the engine recorder raises, never silently drop it.
test("t.id({ prefix }) on an addColumn is a structured OP_INVALID (fail-closed)", () => {
  pubBegin();
  try {
    assert.throws(
      () => pubTable("documents").column("alt_id").add({ type: pubT.id({ prefix: "doc" }) }),
      (e: any) => e.code === "OP_INVALID",
    );
  } finally {
    pubDrain();
  }
});
