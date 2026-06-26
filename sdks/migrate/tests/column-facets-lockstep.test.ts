// Lock-step parity for the column-level SENSITIVE-DATA facets (#173/#174/#178):
// `t.id({ prefix })`, `t.vector(n, { metric })`, and the standalone
// `t.text().mask({ kind, classification })`. These were added to the engine
// recorder (`crates/zeroship-migrate-js/src/migrate_ops.js`) + the IR + fold +
// gen-types FIRST, but never to the PUBLIC `@zeroship/migrate` authoring surface
// (`src/ops.ts` / `src/types.ts`). #178 closes that gap.
//
// This is the byte-identity oracle for the facets: re-author the SAME migration
// through BOTH the public `ops.ts` `table()`/`t.*` and the authoritative embedded
// recorder `migrate_ops.js` `table()`/`t.*`, then assert the two recorded op lists
// are byte-identical. Because the recorder twin is the source of truth the Rust
// engine `include_str!`s into V8, this proves the public DSL records the EXACT
// camelCase wire form (`idPrefix` / `vectorMetric` / `mask:{kind,classification}`)
// the engine deserializes.
//
// RED before #178: the public `t.id`/`t.vector` ignored their option bags and
// `ColumnDef` had no `.mask`, so the public recording dropped every facet and the
// deepEqual diverged (and the `.mask()` call was a TypeError at runtime + a tsc
// error). GREEN after #178: the two recordings match.

import assert from "node:assert/strict";
import { test } from "node:test";

import { __begin as pubBegin, __drain as pubDrain, t as pubT, table as pubTable } from "../src/ops.js";
// The authoritative engine recorder twin (the file the Rust runtime include_str!s
// into V8). Importing it directly makes this an oracle against the real engine
// recording, not a self-referential restatement of the public surface.
import {
  __begin as engBegin,
  __drain as engDrain,
  t as engT,
  table as engTable,
} from "../../../crates/zeroship-migrate-js/src/migrate_ops.js";

type Rec = { begin: () => void; drain: () => any[]; t: any; table: any };

const PUBLIC: Rec = { begin: pubBegin, drain: pubDrain, t: pubT, table: pubTable };
const ENGINE: Rec = { begin: engBegin, drain: engDrain, t: engT, table: engTable };

/** Author a facet-bearing migration against the given recorder + lexicon, return
 *  the recorded op list. The SAME author body runs against both impls. */
function authorWith({ begin, drain, t, table }: Rec): any[] {
  begin();
  // createTable carrying ALL THREE facets:
  //  - t.id({ prefix })            → IrColumn.idPrefix
  //  - t.vector(n, { metric })     → IrColumn.vectorMetric (closed cosine|l2|innerProduct)
  //  - t.text().mask({ kind, classification }) → IrColumn.mask:{kind,classification}
  table("documents").create({
    columns: {
      id: t.id({ prefix: "doc" }),
      embedding: t.vector(1536, { metric: "cosine" }),
      // a standalone mask with an explicit classification
      ssn: t.text().mask({ kind: "last4", classification: "pci" }),
      // a standalone mask defaulting classification → "pii"
      email: t.text().mask({ kind: "email" }),
      title: t.text(),
    },
  });
  // addColumn carries vectorMetric + mask (NOT idPrefix — fail-closed on add):
  table("documents").column("summary_vec").add({ type: t.vector(768, { metric: "innerProduct" }) });
  table("documents").column("phone").add({ type: t.text().mask({ kind: "last4" }) });
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
  const byName = (n: string) => create.columns.find((c: any) => c.name === n);

  // t.id({ prefix }) → idPrefix
  assert.equal(byName("id").idPrefix, "doc");

  // t.vector(n, { metric }) → vectorMetric (closed token)
  assert.equal(byName("embedding").vectorMetric, "cosine");

  // standalone .mask({ kind, classification }) → mask:{kind,classification}
  assert.deepEqual(byName("ssn").mask, { kind: "last4", classification: "pci" });
  // classification defaults to "pii"
  assert.deepEqual(byName("email").mask, { kind: "email", classification: "pii" });

  // a facet-less column carries NONE of the facet keys (checksum-neutral).
  const title = byName("title");
  assert.ok(!("idPrefix" in title) && !("vectorMetric" in title) && !("mask" in title));

  // addColumn carries vectorMetric + mask on the op tail.
  const addVec = ops.find((o: any) => o.op === "addColumn" && o.column === "summary_vec");
  assert.equal(addVec.vectorMetric, "innerProduct");
  const addPhone = ops.find((o: any) => o.op === "addColumn" && o.column === "phone");
  assert.deepEqual(addPhone.mask, { kind: "last4", classification: "pii" });
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
