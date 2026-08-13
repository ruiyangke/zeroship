// Composite and non-`id` foreign keys, applied to a real MySQL server.
//
// `docs/support-matrix.md` publishes both as `Yes` for all three dialects:
//
//   Composite foreign key                    | Yes | Yes | Yes
//   Foreign key referencing a non-`id` column | Yes | Yes | Yes
//
// `TODO.md` said the opposite - "MySQL 8: ... no composite/non-id FK" - and used
// it to argue where roadmap effort should go. Nothing tested either claim against
// MySQL, so the contradiction sat there with no way to settle it. The matrix is
// right: both shapes apply and land as real constraints. The TODO entry was
// pointing effort at something already shipped, and has been corrected.
//
// This is the MySQL counterpart to what `index-facet-matrix.test.ts` does for the
// index rows: a matrix cell nothing exercises against a server is a claim, not a
// fact, and a `Yes` that never reached the emitter would leave every other gate
// green.
//
// WHAT WOULD FAIL HERE: an emitter that renders the composite key as two separate
// single-column foreign keys, or that silently drops the non-`id` reference
// because it assumes references target a primary key. Both would leave `apply`
// exiting 0 with referential integrity quietly weaker than authored, which is why
// the assertions read the live catalog per column and ordinal rather than just
// counting constraints.
//
// GATE: `ZERO_MIGRATE_MYSQL_URL`.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import "./addon.js";

const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_fkm";

// `parent` carries a composite candidate key (a,b) and a single non-`id` one
// (code). Both are declared via `uniques`, which is what makes them referenceable
// - the engine requires a reference to name an exact ordered candidate key.
const MIGRATION = `import { table, t } from "zero-migrate";
export const name = "fk_shapes";
export default {
  up() {
    table("parent").create({
      columns: {
        id: t.int().notNull(),
        a: t.int().notNull(),
        b: t.int().notNull(),
        code: t.string({ length: 32 }).notNull(),
      },
      primaryKey: ["id"],
      uniques: [
        { name: "parent_ab_key", columns: ["a", "b"] },
        { name: "parent_code_key", columns: ["code"] },
      ],
    });
    table("child").create({
      columns: {
        id: t.int().notNull(),
        pa: t.int().notNull(),
        pb: t.int().notNull(),
        pcode: t.string({ length: 32 }).notNull(),
      },
      primaryKey: ["id"],
      foreignKeys: [
        { name: "child_ab_fkey", columns: ["pa", "pb"], references: { table: "parent", columns: ["a", "b"] } },
        { name: "child_code_fkey", columns: ["pcode"], references: { table: "parent", columns: ["code"] } },
      ],
    });
  },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(database: string): string {
  const work = mkdtempSync(join(HERE, "fkmy-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = [${JSON.stringify(database)}] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = [${JSON.stringify(database)}] }

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`,
  );
  writeFileSync(
    join(work, "registry.json"),
    JSON.stringify({ parent: OWNER_APP, child: OWNER_APP }),
  );
  writeFileSync(join(work, "migrations", "20260101000000_fk_shapes.ts"), MIGRATION);
  return work;
}

function apply(work: string, database: string) {
  const base = String(MYSQL_URL).replace(/\/[^/]*$/, "");
  const child = spawn(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "apply", "--approve",
      "--dir", join(work, "migrations"),
      "--database-url", `${base}/${database}`,
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      "--schema", database,
      "--owner-app", OWNER_APP,
    ],
    { cwd: work, env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" } },
  );
  let err = "";
  child.stderr.on("data", (chunk) => (err += chunk));
  return new Promise<{ code: number | null; err: string }>((done) =>
    child.on("close", (code) =>
      done({ code, err: err.replace(/^WARNING.*$/gm, "").trim() }),
    ),
  );
}

test("MySQL applies a composite foreign key and a non-`id` one", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; composite/non-`id` FK coverage skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const connection = await mysql.createConnection({ uri: MYSQL_URL });

  const database = uniqueNamespace("fkmy");
  const work = project(database);
  try {
    await connection.query(`CREATE DATABASE \`${database}\``);
    const ran = await apply(work, database);
    assert.equal(ran.code, 0, `the migration must apply on MySQL; ${ran.err}`);

    const [rows] = await connection.query(
      `SELECT CONSTRAINT_NAME AS n, COLUMN_NAME AS c,
              REFERENCED_TABLE_NAME AS rt, REFERENCED_COLUMN_NAME AS rc,
              ORDINAL_POSITION AS p
         FROM information_schema.KEY_COLUMN_USAGE
        WHERE TABLE_SCHEMA = ? AND REFERENCED_TABLE_NAME IS NOT NULL
        ORDER BY CONSTRAINT_NAME, ORDINAL_POSITION`,
      [database],
    );
    const fks = rows as Array<{ n: string; c: string; rt: string; rc: string; p: number }>;
    const of = (name: string) => fks.filter((row) => row.n === name);

    // The composite key must be ONE constraint spanning two columns in order,
    // not two single-column keys that happen to exist.
    const composite = of("child_ab_fkey");
    assert.equal(
      composite.length,
      2,
      `child_ab_fkey must span exactly two columns, got ${JSON.stringify(composite)}`,
    );
    assert.deepEqual(
      composite.map((row) => [row.c, row.rc]),
      [["pa", "a"], ["pb", "b"]],
      "the composite key's column pairs must be in the authored order",
    );
    assert.ok(
      composite.every((row) => row.rt === "parent"),
      "the composite key must reference `parent`",
    );

    // And the non-`id` reference must point at `code`, not silently at the
    // primary key.
    const nonId = of("child_code_fkey");
    assert.equal(nonId.length, 1, `child_code_fkey must be single-column, got ${JSON.stringify(nonId)}`);
    assert.equal(nonId[0].c, "pcode", "the local column must be `pcode`");
    assert.equal(
      nonId[0].rc,
      "code",
      "the reference must target the non-`id` column `code`, not the primary key",
    );
  } finally {
    await connection.query(`DROP DATABASE IF EXISTS \`${database}\``).catch(() => {});
    await connection.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
