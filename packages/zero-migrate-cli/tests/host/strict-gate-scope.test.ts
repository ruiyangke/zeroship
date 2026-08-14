// What `status --strict` — the documented CI gate — does and does not watch.
//
// `docs/cli.md`: "Strict status exits 1 when anything is pending, drifted, or
// checksum-mismatched."
//
// An operator reads "drifted" as covering the schema. It does not. Drift here is
// the supplied migration set disagreeing with the JOURNAL; the live schema is a
// separate comparison that `security-model.md` says "is not automatically run on
// every JavaScript apply". So the gate someone puts in front of a production
// deploy exits 0 after the table it manages has been dropped.
//
// That is the documented design, not a defect, and this file is not arguing with
// it. It exists because the consequence is severe and was stated nowhere an
// operator would look: `cli.md` described the gate, `security-model.md` described
// the limitation, and neither pointed at the other. `cli.md` now does, and this
// pins the behaviour so the two cannot drift apart again.
//
// The escalation is deliberate — a column, then an unexpected column, then the
// whole table. If some middle case ever starts being caught, the arm that catches
// it is the one that fails, which locates the change precisely.
//
// The first arm is the control: a genuinely pending migration DOES exit 1, so
// these zeroes are the gate's scope and not the gate being broken.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { connectLivePg, pgUrl } from "./live-db.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const MIGRATION = `import { table, t } from "zero-migrate";
export const name = "create_notes";
export default {
  schema() {
    table("notes").create({
      columns: { id: t.int().notNull(), body: t.string({ length: 64 }) },
      primaryKey: ["id"],
    });
  },
};
`;

/** The control's pending migration creates its OWN table rather than altering
 *  `notes`. Touching an existing table needs an ownership registry the CLI takes
 *  from project config, and the control only needs "something is pending". */
const SECOND_MIGRATION = `import { table, t } from "zero-migrate";
export const name = "create_more";
export default {
  schema() {
    table("more_notes").create({
      columns: { id: t.int().notNull() },
      primaryKey: ["id"],
    });
  },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

test("status --strict watches the journal, not the live schema", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("strict_scope");
  const work = mkdtempSync(join(HERE, "strict-scope-"));
  const migrations = join(work, "migrations");

  const cli = (verb: string, extra: readonly string[] = []) =>
    spawnSync(
      process.execPath,
      [
        "--import",
        "tsx",
        CLI_BIN,
        verb,
        "--dir",
        migrations,
        "--database-url",
        pgUrl(),
        "--policy",
        join(work, "policy.toml"),
        "--schema",
        schema,
        "--owner-app",
        "app_strict_scope",
        ...extra,
      ],
      {
        encoding: "utf8",
        cwd: work,
        env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
      },
    );

  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    mkdirSync(migrations);
    writeFileSync(
      join(work, "policy.toml"),
      `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = [${JSON.stringify(schema)}] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = [${JSON.stringify(schema)}] }
`,
    );
    writeFileSync(join(migrations, "20260101000000_create_notes.ts"), MIGRATION);

    assert.equal(cli("apply").status, 0, "the setup migration must apply");
    assert.equal(cli("status", ["--strict"]).status, 0, "a clean project must exit 0");

    // THE CONTROL. An unapplied migration - something the gate DOES watch - must
    // exit 1. Without it the zeroes below could mean the gate never fails at all.
    writeFileSync(join(migrations, "20260102000000_create_more.ts"), SECOND_MIGRATION);
    assert.equal(
      cli("status", ["--strict"]).status,
      1,
      "a pending migration must fail the strict gate",
    );
    assert.equal(cli("apply").status, 0, "and applying it must clear that");
    assert.equal(cli("status", ["--strict"]).status, 0, "back to clean");

    // Now the scope. Each of these is an out-of-band schema change the journal
    // knows nothing about, and the gate reads the journal.
    for (const [label, sql] of [
      ["a dropped column", `ALTER TABLE "${schema}".notes DROP COLUMN body`],
      ["an unexpected column", `ALTER TABLE "${schema}".notes ADD COLUMN sneaky int`],
      ["the whole table dropped", `DROP TABLE "${schema}".notes`],
    ] as ReadonlyArray<readonly [string, string]>) {
      await client.query(sql);
      const result = cli("status", ["--strict"]);
      assert.equal(
        result.status,
        0,
        `TODAY the strict gate does not see ${label}; if this now exits 1, structural ` +
          `drift reached this path and cli.md needs updating with it`,
      );
    }

    // And it still reports the migration as applied, which is the honest reading:
    // the journal is intact, and the journal is what it read.
    const json = cli("status", ["--strict", "--json"]);
    assert.equal(json.status, 0);
    assert.match(
      json.stdout,
      /"state"\s*:\s*"applied"/,
      "the plan still reads applied, because the journal still says so",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
