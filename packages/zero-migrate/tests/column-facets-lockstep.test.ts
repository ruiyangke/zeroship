// Artifact-identity parity for the column-level facets (column facets +
// generated/identity): typed `.references()`, `t.typedId(prefix)`, `ids.ulid()`,
// `t.vector({ dimensions, metric })`, standalone
// `t.text().mask({ kind, classification })`, `.generated(...)`, and `.identity(...)`.
//
// The recorder twin was collapsed: there is no longer a hand-kept second
// recorder. The SDK recorder (`src/ops.ts`) and the engine-embedded
// recorder (`dist/embedded-recorder.js`, the `tsup` build output this suite
// imports) are now the SAME source,
// compiled two ways. This test is the artifact-identity assertion:
// re-author the SAME migration through BOTH the
// `ops.ts` SOURCE (`pub*`) and the COMPILED artifact (`eng*`), then assert the
// two recorded op lists are byte-identical — proving the shipped engine artifact
// records the EXACT camelCase wire form (`valueFormat` / `references` / `vectorMetric` /
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
// The COMPILED engine-embedded recorder artifact (the tracked build output).
// Importing it directly makes this an oracle against the shipped bytes rather
// than a self-referential restatement of the source. It does NOT prove the Rust
// engine reads this file: nothing under crates/ does, and ops reach the engine
// over the N-API bridge already recorded.
import {
  __begin as engBegin,
  __drain as engDrain,
  maxValue as engMaxValue,
  minValue as engMinValue,
  t as engT,
  table as engTable,
} from "../dist/embedded-recorder.js";

type Rec = {
  begin: () => void;
  drain: () => any[];
  table: any;
  t: any;
  minValue: any;
  maxValue: any;
};

const PUBLIC: Rec = {
  begin: pubBegin,
  drain: pubDrain,
  t: pubT,
  table: pubTable,
  minValue: pubMinValue,
  maxValue: pubMaxValue,
};
const ENGINE: Rec = {
  begin: engBegin,
  drain: engDrain,
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
  //  - t.*.references(table, column, actions) -> IrColumn.references
  //  - t.typedId(prefix)         → IrColumn.idPrefix + string(36) storage
  //  - t.vector({ dimensions, metric }) → IrColumn.vectorMetric (closed cosine|l2|innerProduct)
  //  - t.text().mask({ kind, classification }) → IrColumn.mask:{kind,classification}
  //  - t.int().generated(expr)     → IrColumn.generated:{expr,stored}
  //  - t.bigInt().identity(opts)   → IrColumn.identity:{always}
  table("documents").create({
    columns: {
      id: t.typedId("doc" ).primaryKey(),
      public_id: t.typedId("docum" ).required().unique(),
      opaque_id: t.typedId("" ),
      event_id: t.typedId("evt" ).required().unique(),
      owner_id: t.uuid().references("accounts", "id", {
        onDelete: "cascade",
        onUpdate: "restrict",
      }),
      seq: t.bigInt().identity({ always: true }),
      shard: t.int(),
      qty: t.int(),
      unit_cents: t.int(),
      ratio: t.double(),
      source_ip: t.inet(),
      total_cents: t.int().generated((col: any) => col("qty").mul(col("unit_cents"))),
      virtual_total: t.int().generated((col: any) => col("qty").mul(col("unit_cents")), { virtual: true }),
      embedding: t.vector({ dimensions: 1536, metric: "cosine" }),
      // a standalone mask with an explicit classification
      ssn: t.text().mask({ kind: "last4", classification: "pci" }),
      // a standalone mask defaulting classification → "pii"
      email: t.text().mask({ kind: "email" }),
      // .collation(intent) → IrColumn.collation (closed bytewise)
      cursor_key: t.text().collation("bytewise"),
      bounded_key: t.string({ length: 40 }).collation("bytewise"),
      title: t.text(),
    },
  });
  // addColumn carries valueFormat + vectorMetric + mask:
  table("documents").column("summary_vec").add({ type: t.vector({ dimensions: 768, metric: "innerProduct" }) });
  table("documents").column("external_id").add({ type: t.typedId("ext" ) });
  table("documents").column("phone").add({ type: t.text().mask({ kind: "last4" }) });
  table("documents").column("added_total").add({
    type: t.int().generated((col: any) => col("qty").mul(col("unit_cents"))),
  });
  table("documents").column("added_seq").add({ type: t.bigInt().identity() });
  return drain();
}

test("public column facets record byte-identically to the engine recorder", () => {
  const pub = authorWith(PUBLIC);
  const eng = authorWith(ENGINE);
  assert.deepEqual(pub, eng);
});

test("the recorded facets carry the exact camelCase wire form", () => {
  const ops = authorWith(PUBLIC);
  const create = ops[0];
  assert.equal(create.op, "createTable");
  const byName = (n: string) => create.columns.find((column: any) => column.name === n);

  // A typed-id column records bounded string(36) storage plus its declared prefix.
  assert.deepEqual(byName("id").type, { string: { length: 36 } });
  assert.equal(byName("id").idPrefix, "doc");

  // t.vector({ dimensions, metric }) → vectorMetric (closed token)
  assert.equal(byName("embedding").vectorMetric, "cosine");
  assert.equal(byName("shard").type, "int");
  assert.equal(byName("ratio").type, "double");
  assert.equal(byName("source_ip").type, "inet");
  assert.equal(byName("public_id").idPrefix, "docum");
  assert.equal(byName("opaque_id").idPrefix, "");
  assert.equal(byName("event_id").idPrefix, "evt");
  assert.equal(byName("owner_id").type, "uuid");
  assert.deepEqual(byName("owner_id").references, {
    table: "accounts",
    column: "id",
    onDelete: "cascade",
    onUpdate: "restrict",
  });

  // .collation(intent) → collation (the closed intent token, never a SQL name)
  assert.equal(byName("cursor_key").collation, "bytewise");
  assert.equal(byName("bounded_key").collation, "bytewise");
  assert.deepEqual(byName("bounded_key").type, { string: { length: 40 } });

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
  assert.ok(
    !("idPrefix" in title) &&
      !("references" in title) &&
      !("vectorMetric" in title) &&
      !("collation" in title) &&
      !("mask" in title) &&
      !("generated" in title) &&
      !("identity" in title),
  );

  // addColumn carries vectorMetric + mask + generated + identity on the op tail.
  const addVec = ops.find((o: any) => o.op === "addColumn" && o.column === "summary_vec");
  assert.equal(addVec.vectorMetric, "innerProduct");
  const addTypeId = ops.find((o: any) => o.op === "addColumn" && o.column === "external_id");
  assert.deepEqual(addTypeId.type, { string: { length: 36 } });
  assert.ok(!("idPrefix" in addTypeId), "addColumn has no typed-id prefix slot");
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
  table,
  t,
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
  table("events")
    .index("events_ts_brin_idx")
    .add({
      on: ["ts"],
      using: "brin",
      include: ["tenant_id"],
      with: { pagesPerRange: 32 },
      only: true,
    });
  table("events", { schema: "app" }).partition("events_2026_05").detach({ concurrently: true });
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

test("both recorders refuse the same out-of-set and out-of-type collations", () => {
  for (const { begin, drain, t } of [PUBLIC, ENGINE]) {
    begin();
    try {
      // REJECTION CONTROLS: a recorder that accepted any of these would record a
      // column the engine's validator rejects, so the two impls must agree on
      // the refusals as well as on the accepted wire shape.
      assert.throws(() => t.text().collation("C"), (e: any) => e.code === "OP_INVALID");
      assert.throws(() => t.int().collation("bytewise"), (e: any) => e.code === "OP_INVALID");
      assert.throws(
        () => t.text({ caseSensitive: false }).collation("bytewise"),
        (e: any) => e.code === "OP_INVALID",
      );
    } finally {
      drain();
    }
  }
});
