// The apply-time diagnosis of a journal step whose migration file is gone, driven
// end to end through the shipped CLI against live PostgreSQL.
//
// A deploy applies one file per addon call, each call carrying the authored prefix
// that ends at the file it is applying. So no single call is handed the operator's
// whole directory, and a completed journal step missing from ONE call's prefix is
// two different things: a file the operator deleted, or a later file this call was
// simply not given yet. The journal's own `event_seq` separates them -- a completed
// step recorded BEFORE the newest step this call did supply cannot be a later file,
// so it is a deleted one.
//
// Three one-step files, not two: the rerun makes two per-file calls, so three files
// are what show the diagnosis firing ONCE for the operator's set instead of once per
// call, and show it naming only the deleted file. It is the same in-lock refusal the
// deploy already raises for a pending migration, so it refuses rather than prints:
// asserting a `tracing::warn!` with no subscriber installed would assert nothing.
//
// GATE: `connectLivePg` (see `live-db.ts`). Runs under `node --import tsx --test`.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { connectLivePg, pgUrl } from "./live-db.js";
import { noInjectPolicy } from "./policy.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

/** The text the apply-time diagnosis prints, wherever it surfaces. */
const DIAGNOSIS = /net-applied journal step (\S+) was not supplied/g;

function spawnCli(args: readonly string[], cwd: string) {
  return spawnSync(process.execPath, ["--import", "tsx", CLI_BIN, ...args], {
    encoding: "utf8",
    cwd,
    env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
  });
}

function temporaryDirectory(prefix: string): string {
  return mkdtempSync(join(HERE, prefix));
}

function uniqueSchema(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** One migration file authoring exactly one `create table`, hence one journal step. */
function writeStep(dir: string, filename: string, name: string, tableName: string): string {
  const path = join(dir, filename);
  writeFileSync(
    path,
    `import { table, t } from "zero-migrate";
export const name = ${JSON.stringify(name)};
export function up() {
  table(${JSON.stringify(tableName)}).create({ columns: { id: t.int() } });
}
`,
  );
  return path;
}

function applyArgs(schema: string): string[] {
  return [
    "apply",
    "--dir=.",
    `--database-url=${pgUrl()}`,
    `--schema=${schema}`,
    "--policy=policy.toml",
    "--approve",
  ];
}

/** Every distinct journal step id the diagnosis named, in the order it named them. */
function diagnosedStepIds(result: { stdout: string; stderr: string }): string[] {
  const text = `${result.stdout}\n${result.stderr}`;
  return [...text.matchAll(DIAGNOSIS)].map((match) => match[1]);
}

// The whole scenario in one arm: the clean deploy must stay silent, and the deploy
// that lost a file must name that file's journal step exactly once. Splitting it
// would make the second arm re-run the first as unasserted setup.
test("CLI apply names a deleted migration's journal step once, and only it", async (t) => {
  const client = await connectLivePg(t);
  if (client === null) return;
  const cwd = temporaryDirectory(".cli-orphan-journal-");
  const schema = uniqueSchema("zm_orphan");
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    writeFileSync(join(cwd, "policy.toml"), noInjectPolicy(schema));
    const fileOne = writeStep(cwd, "20260801000001_m1_alpha.ts", "m1_alpha", "alpha");
    writeStep(cwd, "20260801000002_m2_beta.ts", "m2_beta", "beta");
    writeStep(cwd, "20260801000003_m3_gamma.ts", "m3_gamma", "gamma");

    const first = spawnCli(applyArgs(schema), cwd);
    assert.equal(first.status, 0, first.stderr);
    assert.equal(
      first.stdout.split("\n").filter((line) => line.startsWith("apply ")).length,
      3,
      first.stdout,
    );
    assert.deepEqual(diagnosedStepIds(first), [], "a complete set has no missing step");

    const journal = await client.query(
      `SELECT name, version FROM "${schema}_migrations".schema_migrations
        WHERE event_kind = 'applied' ORDER BY event_seq`,
    );
    const stepIdByName = new Map<string, string>(
      (journal.rows as Array<{ name: string; version: string }>).map((row) => [
        row.name,
        row.version,
      ]),
    );
    assert.deepEqual(
      [...stepIdByName.keys()],
      ["create_table_alpha", "create_table_beta", "create_table_gamma"],
      "each file journals exactly one step, in file order",
    );
    const deletedStepId = stepIdByName.get("create_table_alpha");
    assert.ok(deletedStepId);

    rmSync(fileOne);
    const second = spawnCli(applyArgs(schema), cwd);
    assert.deepEqual(
      diagnosedStepIds(second),
      [deletedStepId],
      `expected exactly one diagnosis naming ${deletedStepId}\n${second.stdout}\n${second.stderr}`,
    );
    assert.equal(second.status, 1, "the diagnosis refuses the deploy, it does not just print");
    // The first call is handed only file 2, which is a prefix and not the operator's
    // set, so it says nothing. The refusal comes from the call that was handed both
    // remaining files -- once, not once per call.
    assert.equal(
      second.stdout.split("\n").filter((line) => line.startsWith("apply ")).length,
      1,
      second.stdout,
    );

    const strict = spawnCli(
      [
        "status",
        "--dir=.",
        `--database-url=${pgUrl()}`,
        `--schema=${schema}`,
        "--policy=policy.toml",
        "--strict",
        "--json",
      ],
      cwd,
    );
    assert.equal(strict.status, 1, strict.stderr);
    const reply = JSON.parse(strict.stdout) as {
      unexpectedJournal: Array<{ version: string }>;
    };
    assert.deepEqual(
      reply.unexpectedJournal.map((entry) => entry.version),
      [deletedStepId],
      "status is the oracle: the same one step, and no other",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(cwd, { recursive: true, force: true });
  }
});

// The shipped in-lock refusal, which had no test: a PENDING migration supplied with
// its authored priors, over a journal holding a completed step that no supplied file
// owns. Renaming the second file re-derives its plan identity, so the journal keeps
// the old step and the deploy is asked to apply a migration whose prefix no longer
// accounts for it.
test("CLI apply refuses a pending migration when a completed step has no file", async (t) => {
  const client = await connectLivePg(t);
  if (client === null) return;
  const cwd = temporaryDirectory(".cli-missing-prefix-");
  const schema = uniqueSchema("zm_prefix");
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    writeFileSync(join(cwd, "policy.toml"), noInjectPolicy(schema));
    writeStep(cwd, "20260801000001_m1_alpha.ts", "m1_alpha", "alpha");
    const second = writeStep(cwd, "20260801000002_m2_beta.ts", "m2_beta", "beta");

    const applied = spawnCli(applyArgs(schema), cwd);
    assert.equal(applied.status, 0, applied.stderr);
    const journal = await client.query(
      `SELECT version FROM "${schema}_migrations".schema_migrations
        WHERE event_kind = 'applied' AND name = 'create_table_beta'`,
    );
    const strandedStepId = (journal.rows as Array<{ version: string }>)[0]?.version;
    assert.ok(strandedStepId);

    rmSync(second);
    writeStep(cwd, "20260801000002_m2_delta.ts", "m2_delta", "delta");

    const refused = spawnCli(applyArgs(schema), cwd);
    assert.equal(refused.status, 1, refused.stdout);
    assert.deepEqual(diagnosedStepIds(refused), [strandedStepId], refused.stderr);
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(cwd, { recursive: true, force: true });
  }
});
