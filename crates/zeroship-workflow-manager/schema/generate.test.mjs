import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, unlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { test } from "node:test";
import { table, t } from "../../../packages/zero-migrate/dist/index.js";
import { compileSchema, writeArtifacts } from "./generate.mjs";
import { workflowManagerSchema } from "./schema.ts";

test("compiles both queue backends with matching typed collection metadata", () => {
  const artifacts = compileSchema();
  assert.deepEqual(artifacts.map(artifact => artifact.name), ["postgres.sql", "sqlite.sql", "schema.runtime.json"]);
  const { collections } = JSON.parse(artifacts.find(artifact => artifact.name === "schema.runtime.json").content);
  assert.equal(collections.queue_scopes.fields.id.primaryKey, true);
  assert.equal(collections.jobs.fields.id.primaryKey, true);
  assert.equal(collections.jobs.fields.deployment_id.required, true);
  assert.equal(collections.jobs.fields.worker_id.required ?? false, false);
  assert.equal(collections.jobs.fields.attempt.type, "bigInt");
  assert.deepEqual(collections.jobs.indexes.map(index => index.fields), [
    ["app_id", "state", "available_at", "id"],
    ["app_id", "state", "lease_deadline", "id"],
  ]);
  assert(collections.queue_scopes.indexes.some(index => index.unique && index.fields.join() === "app_id"));
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
