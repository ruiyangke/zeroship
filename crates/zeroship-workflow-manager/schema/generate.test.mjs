import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, unlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { test } from "node:test";
import { table, t } from "../../../packages/zero-migrate/dist/index.js";
import { compileSchema, writeArtifacts } from "./generate.mjs";
import { managerIdentityColumns, workflowManagerSchema } from "./schema.ts";

test("compiles both manager backends with matching typed collection metadata", () => {
  const artifacts = compileSchema();
  assert.deepEqual(artifacts.map(artifact => artifact.name), ["postgres.sql", "sqlite.sql", "schema.runtime.json", "fingerprint.txt", "identity-columns.json"]);
  const { collections } = JSON.parse(artifacts.find(artifact => artifact.name === "schema.runtime.json").content);
  assert.equal(collections.queue_scopes.fields.id.primaryKey, true);
  assert.equal(collections.jobs.fields.id.primaryKey, true);
  assert.equal(collections.jobs.fields.deployment_id.required, true);
  assert.equal(collections.jobs.fields.worker_id.required ?? false, false);
  assert.equal(collections.jobs.fields.attempt.type, "bigInt");
  assert.equal(collections.queue_scopes.fields.dispatch_cursor.type, "bigInt");
  assert.equal(collections.queue_scopes.fields.dispatch_cursor.required, true);
  assert.equal(collections.queue_scopes.fields.dispatch_cursor.default, 0);
  assert.equal(collections.jobs.fields.dispatch_order.type, "bigInt");
  assert.equal(collections.jobs.fields.dispatch_order.required, true);
  assert.equal(collections.jobs.fields.dispatch_order.default, undefined);
  assert.deepEqual(collections.jobs.indexes.map(index => index.fields), [
    ["app_id", "state", "available_at", "id"],
    ["app_id", "state", "lease_deadline", "id"],
    ["app_id", "id"],
    ["app_id", "deployment_id", "state"],
    ["app_id", "dispatch_order"],
    ["app_id", "state", "dispatch_order"],
  ]);
  assert(collections.jobs.indexes.some(index => index.unique && index.fields.join() === "app_id,dispatch_order"));
  for (const [name, duplicateIdentity] of [["queue_scopes", "app_id"], ["workers", "worker_id"]]) {
    assert.equal(collections[name].fields[duplicateIdentity], undefined);
    assert.equal(collections[name].indexes?.some(index => index.unique) ?? false, false);
  }
  assert.deepEqual(Object.keys(collections).sort(), Object.keys(managerIdentityColumns).sort());
  assert.equal(collections.workers.fields.lock_version.default, 0);
  assert.equal(collections.workers.fields.capacity.default, 1);
  assert.equal(collections.workers.fields.state.default, "ready");
  assert.equal(collections.workers.fields.expires_at.default, 0);
  for (const collection of Object.values(collections)) {
    assert.deepEqual(Object.keys(collection.fields).filter(name => collection.fields[name].primaryKey), ["id"]);
  }
  for (const name of ["placement_receipts", "management"]) {
    assert(collections[name].indexes.some(index => index.unique && index.fields.join() === "app_id,request_id"));
  }
  assert(collections.assignments.indexes.some(index => index.unique && index.fields.join() === "app_id,worker_id"));
  assert.equal(collections.schema_version.fields.fingerprint.required, true);
  assert.deepEqual(JSON.parse(artifacts.find(artifact => artifact.name === "identity-columns.json").content), managerIdentityColumns);
});

test("fingerprints change with emitted metadata schema", () => {
  const fingerprint = outputs => outputs.find(output => output.name === "fingerprint.txt").content.trim();
  const original = fingerprint(compileSchema());
  assert.match(original, /^[a-f0-9]{64}$/);
  const changed = fingerprint(compileSchema(namespace => {
    workflowManagerSchema(namespace);
    table("jobs", { schema: namespace }).index("jobs_created_idx").add({ on: ["created_at"] });
  }));
  assert.notEqual(changed, original);
});

test("refuses empty and incomplete schema recordings", () => {
  assert.throws(() => compileSchema(() => {}), /recorder is empty/);
  assert.throws(() => compileSchema(namespace => {
    table("queue_scopes", { schema: namespace }).create({
      columns: { id: t.text().notNull() }, primaryKey: ["id"],
    });
  }), /unexpected collections/);
});

test("refuses operations the selected backend cannot lower", () => {
  assert.throws(() => compileSchema(namespace => {
    workflowManagerSchema(namespace);
    table("jobs", { schema: namespace }).column("outcome").setType({ to: t.bigInt() });
  }), /failed to render/);
});

test("check requires complete byte-exact artifacts and never repairs drift", async () => {
  const path = await mkdtemp(join(tmpdir(), "workflow-manager-schema-"));
  const directory = pathToFileURL(`${path}/`);
  try {
    const outputs = compileSchema();
    await writeArtifacts(outputs, { directory });
    await writeArtifacts(outputs, { directory, check: true });
    const sql = new URL("postgres.sql", directory);
    const drifted = `${outputs[0].content}\n`;
    await writeFile(sql, drifted);
    await assert.rejects(writeArtifacts(outputs, { directory, check: true }), /stale workflow manager schema/);
    assert.equal(await readFile(sql, "utf8"), drifted);
    await writeArtifacts(outputs, { directory });
    await unlink(sql);
    await assert.rejects(writeArtifacts(outputs, { directory, check: true }), { code: "ENOENT" });
    await assert.rejects(writeArtifacts([], { directory }), /artifacts are empty/);
  } finally {
    await rm(path, { recursive: true, force: true });
  }
});
