// A mandatory table-shape injection lands, and the preview shows the same table
// apply creates.
//
// `docs/cli.md` on `plan`: it prints each pending migration's rendered SQL "with
// the selected policy's table-shape injection already applied, so the previewed
// `CREATE TABLE` is the one apply runs."
//
// That is two claims, and neither had a test. A mandatory `[[inject]]` is an
// operator-enforced invariant - "every table in this schema carries
// `created_at`" - imposed on migrations the operator did not write. Both failure
// modes are quiet:
//
//   * injection silently not happening on apply voids the mandate, and the
//     tables that are missing the column look exactly like the ones that are not;
//   * injection happening on apply but not in the preview means review passes on
//     a `CREATE TABLE` that is not the one that runs, which is the whole reason
//     the preview exists.
//
// The suite had the opposite case only: `e2e-pg.test.ts` asserts that under
// `noInjectPolicy` no policy-managed column appears. Nothing exercised an
// injecting charter end to end.
//
// Both halves are read from the places that decide them - the preview from the
// CLI's own stdout, the result from `information_schema` - and compared on COLUMN
// ORDER as well as membership, since the injected column leads.
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
    table("notes").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** The charter, with the mandatory injection present or absent. */
function charter(schema: string, inject: boolean): string {
  const scope = `{ include = [${JSON.stringify(schema)}] }`;
  return `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = ${scope}

[[grant]]
key = "schema.create_table"
value = true
scope = ${scope}
${
  inject
    ? `
[[inject]]
scope = { include = [${JSON.stringify(`${schema}.*`)}] }
mandatory = true
columns = [
  { name = "created_at", type = "timestamptz", nullable = false },
]
`
    : ""
}`;
}

function runCli(verb: string, work: string, migrations: string, schema: string) {
  return spawnSync(
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
      "app_inject_parity",
    ],
    {
      encoding: "utf8",
      cwd: work,
      env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    },
  );
}

async function withProject<T>(
  client: import("pg").Client,
  inject: boolean,
  body: (work: string, migrations: string, schema: string) => Promise<T>,
): Promise<T> {
  const schema = uniqueNamespace(inject ? "inject_on" : "inject_off");
  const work = mkdtempSync(join(HERE, "inject-parity-"));
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const migrations = join(work, "migrations");
    mkdirSync(migrations);
    writeFileSync(join(work, "policy.toml"), charter(schema, inject));
    writeFileSync(join(migrations, "20260101000000_create_notes.ts"), MIGRATION);
    return await body(work, migrations, schema);
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
      )
      .catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
}

async function liveColumns(
  client: import("pg").Client,
  schema: string,
): Promise<string[]> {
  const { rows } = await client.query(
    `SELECT column_name FROM information_schema.columns
      WHERE table_schema = $1 AND table_name = 'notes'
      ORDER BY ordinal_position`,
    [schema],
  );
  return rows.map((row) => row.column_name as string);
}

test("a mandatory injection reaches both the preview and the applied table", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  try {
    await withProject(client, true, async (work, migrations, schema) => {
      const planned = runCli("plan", work, migrations, schema);
      assert.equal(
        planned.status,
        0,
        `plan must succeed; stdout=${planned.stdout} stderr=${planned.stderr}`,
      );

      // The preview, read from the CLI's own stdout - not from an API that could
      // agree with apply for the wrong reason.
      const ddl = planned.stdout.match(/CREATE TABLE[^;]*/)?.[0].replace(/\s+/g, " ");
      assert.ok(ddl, `the preview must render a CREATE TABLE; got ${planned.stdout}`);
      assert.match(
        ddl,
        /"created_at"\s+timestamptz NOT NULL/,
        `the previewed DDL must carry the injected column; got ${ddl}`,
      );

      const applied = runCli("apply", work, migrations, schema);
      assert.equal(
        applied.status,
        0,
        `apply must succeed; stdout=${applied.stdout} stderr=${applied.stderr}`,
      );

      // ORDER, not just membership. The injected column leads, and a preview that
      // agreed on the set while disagreeing on the order would still be showing a
      // reviewer a different table.
      assert.deepEqual(
        await liveColumns(client, schema),
        ["created_at", "id"],
        "the applied table must match the previewed shape, injected column first",
      );

      // And the preview really did put it first, so the two agree on order.
      assert.ok(
        ddl.indexOf('"created_at"') < ddl.indexOf('"id"'),
        `the preview must order the injected column first too; got ${ddl}`,
      );
    });

    // THE CONTROL. The same migration under a charter WITHOUT the injection must
    // produce only the authored column. Without it, "created_at is present" would
    // also hold for a build that added the column for some other reason, and this
    // file would not be measuring the policy at all.
    await withProject(client, false, async (work, migrations, schema) => {
      const applied = runCli("apply", work, migrations, schema);
      assert.equal(
        applied.status,
        0,
        `apply must succeed; stdout=${applied.stdout} stderr=${applied.stderr}`,
      );
      assert.deepEqual(
        await liveColumns(client, schema),
        ["id"],
        "without the inject rule the authored column is the only one",
      );
    });
  } finally {
    await client.end().catch(() => {});
  }
});
