import assert from "node:assert/strict";
import { test } from "node:test";

const ROOTED_VENDOR_EXPORTS = [
  "createFunction",
  "domain",
  "dropFunction",
  "dropOwnedBy",
  "extension",
  "grant",
  "raw",
  "revoke",
  "role",
  "schema",
  "sequence",
] as const;

const FORBIDDEN_INTERNAL_EXPORTS = [
  "__begin",
  "__drain",
  "__pgDomain",
  "__pgPush",
  "__pgResolveExpr",
  "__pgSequence",
  "cAgg",
  "cCase",
  "opProducers",
  "opProducerRegistry",
  "pgTable",
] as const;

test("published runtime exposes the migration DSL without recorder internals", async () => {
  const runtimeRoot = await import("@zeroship/migrate");
  assert.equal(typeof runtimeRoot.table, "function");
  assert.equal((runtimeRoot as unknown as Record<string, unknown>).ids, undefined);
  assert.equal((runtimeRoot.t as unknown as Record<string, unknown>).id, undefined);
  assert.equal((runtimeRoot.t as unknown as Record<string, unknown>).ref, undefined);
  assert.equal(typeof runtimeRoot.t.text().references, "function");
  assert.equal(typeof runtimeRoot.perRow, "object");
  assert.equal(typeof runtimeRoot.perRow.uuidV4, "function");
  assert.equal(typeof runtimeRoot.perRow.uuidV7, "function");
  assert.equal(typeof runtimeRoot.perRow.typeId, "function");
  assert.equal((runtimeRoot.perRow as unknown as Record<string, unknown>).ulid, undefined);

  const tableHandle = runtimeRoot.table("public_surface_probe") as unknown as Record<string, unknown>;
  assert.equal(typeof tableHandle.primaryKey, "function");
  assert.equal(tableHandle.changeIdType, undefined);

  for (const name of ROOTED_VENDOR_EXPORTS) {
    assert.equal(typeof runtimeRoot[name], "function", `${name} must be a root runtime export`);
  }
  for (const name of FORBIDDEN_INTERNAL_EXPORTS) {
    assert.equal(
      (runtimeRoot as Record<string, unknown>)[name],
      undefined,
      `${name} must stay out of the root runtime export`,
    );
  }
});
