import assert from "node:assert/strict";
import { test } from "node:test";
import { t, table } from "../src/index.js";
import { __begin, __drain } from "../src/ops.js";

test("named relations retain their navigation name separately from the SQL constraint", () => {
  __begin();
  const base = t.text();
  table("posts").create({ columns: {
    id: base.primaryKey(),
    author_id: base.references("users", "id", { relation: "author", name: "posts_author_fk" }),
    reviewer_id: base.references("users", "id", { relation: "reviewer" }),
  } });
  const [op] = __drain() as any[];
  assert.deepEqual(op.columns[1].references, {
    table: "users", column: "id", relation: "author", name: "posts_author_fk",
  });
  assert.equal(op.columns[2].references.relation, "reviewer");
  assert.equal(op.columns[0].references, undefined);
});

test("named relations reject unsafe identifiers and ambiguous source names", () => {
  for (const relation of ["_meta", "_custom", "", "author-name", "é", "__proto__", "constructor", "prototype", "__zs_meta", "__ZEROSHIP_meta", "SQLITE_author", "x".repeat(64)]) {
    assert.throws(() => t.text().references("users", "id", { relation }),
      (error: any) => error.code === "OP_INVALID", relation);
  }
  for (const columns of [
    { author: t.text(), author_id: t.text().references("users", "id", { relation: "author" }) },
    { author_id: t.text().references("users", "id", { relation: "author" }), reviewer_id: t.text().references("users", "id", { relation: "author" }) },
  ]) {
    __begin();
    try {
      assert.throws(() => table("posts").create({ columns }), (error: any) => error.code === "OP_INVALID");
    } finally { __drain(); }
  }
});
