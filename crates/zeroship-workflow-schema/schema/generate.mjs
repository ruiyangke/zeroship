import { readFile, writeFile, readdir } from "node:fs/promises";
import { createHash } from "node:crypto";
import { isDeepStrictEqual } from "node:util";
import { genArtifacts } from "../../../crates/zeroship-migrate-node/index.js";
import { workflowSchema } from "./schema.ts";
import { bindOwnedNames } from "./names.mjs";
import { buildEnvelope } from "../../../packages/zero-migrate/dist/internal/recorder.js";
import { currentIrVersion, previewSql } from "../../../packages/zero-migrate-cli/dist/index.js";

const check = process.argv.includes("--check");

// THE ORDERED SERIES. Version 1 is `schema.ts`; every later version is one
// module in `migrations/`, named `NNNN_*.ts` and exporting `up(namespace)`. The
// snapshot artifacts are the fold of the whole series, never authored directly,
// so the snapshot and the series cannot describe different journals.
const migrationFiles = (await readdir(new URL("./migrations/", import.meta.url)))
  .filter(name => /^\d{4}_.*\.ts$/.test(name))
  .sort();
const series = [{ version: 1, apply: workflowSchema }];
for (const file of migrationFiles) {
  const version = Number(file.slice(0, 4));
  if (version !== series.length + 1) {
    throw new Error(`workflow schema migrations must be contiguous from 0002; ${file} is out of order`);
  }
  const { up } = await import(new URL(`./migrations/${file}`, import.meta.url));
  if (typeof up !== "function") throw new Error(`workflow schema migration ${file} must export up(namespace)`);
  series.push({ version, apply: up });
}

// No `sql.raw` grant: the journal is authored entirely in the portable op DSL,
// and withholding the grant is what keeps it that way — a raw escape fails the
// charter here rather than reaching an artifact.
const charterFor = namespace => `policy_version = 1
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["${namespace}"] }
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["${namespace}"] }
`;

const fingerprints = {};
const outputs = [];
let runtimeDescriptor;
for (const dialect of ["postgres", "sqlite"]) {
  const namespace = dialect === "postgres" ? "__zeroship_workflow_schema" : "main";
  const charter = charterFor(namespace);
  const envelopes = [];
  const identifiers = new Set();
  const columns = new Set();
  // Lower CUMULATIVELY and take each version's SQL as the suffix its envelope
  // added. A version's ops may depend on the state earlier versions left, so a
  // version cannot be lowered alone; the prefix assertion below is what proves
  // the suffix really is that version's contribution and nothing else.
  let previous = "";
  const bodies = [];
  for (const { version, apply } of series) {
    let declared;
    const envelope = buildEnvelope(
      { name: `workflow_journal_${String(version).padStart(4, "0")}`, schema: () => { declared = apply(namespace); } },
      { irVersion: currentIrVersion() },
    );
    if (!envelope.ops.length) throw new Error(`workflow schema version ${version} recorded no operations`);
    for (const name of declared.identifiers) identifiers.add(name);
    for (const name of declared.columns) columns.add(name);
    envelopes.push(envelope);
    const statements = previewSql({
      envelopes: envelopes.map(one => JSON.stringify(one)),
      dialect,
      defaultSchema: namespace,
      ownerApp: "workflow",
      charterLayers: [charter],
    });
    const emitted = statements.join("\n");
    if (emitted.includes("[runtime-resolved]")) throw new Error("workflow schema must lower completely");
    // Preview annotations contain measured tallies; shipped SQL retains statements.
    const cumulative = bindOwnedNames(
      emitted.split("\n").filter(line => !line.startsWith("--")).join("\n").trim(),
      { identifiers, columns },
    );
    if (!cumulative.startsWith(previous)) {
      throw new Error(`workflow schema version ${version} rewrote the SQL of an earlier version`);
    }
    bodies.push({ version, sql: cumulative.slice(previous.length).trim() });
    previous = cumulative;
  }
  const snapshot = previous;
  const fingerprint = createHash("sha256").update(snapshot).digest("hex");
  fingerprints[dialect] = fingerprint;

  const artifacts = genArtifacts({ envelopes, dialect, projectSchema: namespace, charterLayers: [charter] });
  if (!artifacts.ok) throw new Error(artifacts.error);
  const descriptor = JSON.parse(artifacts.runtimeJson);
  if (!Object.keys(descriptor.collections).length) throw new Error("workflow descriptor is empty");
  descriptor.collections = Object.fromEntries(Object.entries(descriptor.collections).map(([name, collection]) => {
    if (!identifiers.has(name)) throw new Error(`unexpected workflow collection ${name}`);
    const indexes = collection.indexes.map(index => {
      if (!identifiers.has(index.name)) throw new Error(`unexpected workflow index ${index.name}`);
      return { ...index, name: `__zeroship_workflow_${index.name}` };
    });
    return [`__zeroship_workflow_${name}`, { ...collection, indexes }];
  }));
  if (runtimeDescriptor && !isDeepStrictEqual(runtimeDescriptor, descriptor)) {
    throw new Error("workflow model metadata differs between database dialects");
  }
  runtimeDescriptor = descriptor;

  // Per-version DDL, with NO stamp write: the installer records the stamp once,
  // after the versions it applied have all committed.
  for (const { version, sql } of bodies) {
    outputs.push({
      path: new URL(`./versions/${String(version).padStart(4, "0")}.${dialect}.sql`, import.meta.url),
      content: `-- Generated by schema/generate.mjs through the migration compiler.\n${sql}\n`,
    });
  }
  // The snapshot: the whole series plus its stamp, applied verbatim by the
  // in-process SQLite initializer and read by hosts as the journal's identity.
  const stamp = `INSERT INTO "${namespace}".__zeroship_workflow_schema_version (id, version, fingerprint) VALUES ('workflow', ${series.length}, '${fingerprint}');`;
  outputs.push({
    path: new URL(`./${dialect}.sql`, import.meta.url),
    content: `-- Generated by schema/generate.mjs through the migration compiler.\n${snapshot}\n${stamp}\n`,
  });
}

outputs.push({ path: new URL("./fingerprints.json", import.meta.url), content: JSON.stringify(fingerprints, null, 2) + "\n" });
outputs.push({ path: new URL("./version.txt", import.meta.url), content: `${series.length}\n` });
outputs.push({ path: new URL("./schema.runtime.json", import.meta.url), content: JSON.stringify(runtimeDescriptor, null, 2) + "\n" });
// Finish compiling every dialect before changing any generated artifact.
for (const { path, content } of outputs) {
  if (check) {
    if (await readFile(path, "utf8") !== content) throw new Error(`stale workflow schema ${path.pathname}; run node crates/zeroship-workflow-schema/schema/generate.mjs`);
  } else {
    await writeFile(path, content);
  }
}
