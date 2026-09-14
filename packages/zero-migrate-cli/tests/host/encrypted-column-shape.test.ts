// `t.encrypted({ of })` stores ciphertext in the hidden raw sibling and the
// creator-facing mask in the declared field. The catalogs must match that storage
// contract on every supported target:
//
//   authored   secret: t.encrypted({ of: t.text() })
//
//   PostgreSQL   __zs_raw__secret  bytea          encryption sentinel
//                secret            text           mask sentinel
//   SQLite       __zs_raw__secret  BLOB           encryption sentinel in the DDL
//                secret            TEXT           mask sentinel in the DDL
//   MySQL        __zs_raw__secret  longblob        no comment
//                secret            varchar(191)    no comment
//
// The SENTINEL, which is how anything downstream can tell an encrypted column from
// an ordinary blob. PostgreSQL carries it in a column comment; SQLite — which has
// no column-comment feature at all — carries it as an inline SQL comment preserved
// in `sqlite_master.sql`. MySQL, which DOES support `COLUMN_COMMENT` natively,
// carries nothing. That ordering is backwards from what capability predicts, which
// is why this file asserts the MySQL absence explicitly rather than skipping it:
// the assertion is a record of current behaviour, and it is the one that should
// fail if the sentinel is ever emitted there.
//
// The package test runner supplies owned PostgreSQL and MySQL containers; SQLite
// runs in process.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { mysqlUrl, pgUrl } from "./live-db.js";

// The host suite builds and resolves its addon in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zeroship-migrate-node/zeroship-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_encshape";
const TABLE = "enc_t";
const ENC_SENTINEL = "zero-migrate:enc:";
const MASK_SENTINEL = "zero-migrate:mask:";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(): string {
  const work = mkdtempSync(join(HERE, "encshape-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "schema.create_table"
value = true
scope = "all"
`,
  );
  writeFileSync(join(work, "registry.json"), JSON.stringify({ [TABLE]: OWNER_APP }));
  writeFileSync(
    join(work, "migrations", "20260101000000_a.ts"),
    `import { table, t } from "@zeroship/migrate";
export const name = "a";
export default {
  schema() {
    table("${TABLE}").create({
      columns: { id: t.int().notNull(), secret: t.encrypted({ of: t.text() }) },
      primaryKey: ["id"],
    });
  },
};
`,
  );
  return work;
}

function apply(
  work: string,
  databaseUrl: string,
  namespace: string | null,
): { code: number | null; text: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "apply", "--approve",
      "--dir", join(work, "migrations"),
      "--database-url", databaseUrl,
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      ...(namespace ? ["--schema", namespace] : []),
      "--owner-app", OWNER_APP,
    ],
    {
      cwd: work,
      encoding: "utf8",
      env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    },
  );
  return {
    code: result.status,
    text: `${result.stdout ?? ""}\n${result.stderr ?? ""}`.replace(/^WARNING.*$/gm, "").trim(),
  };
}

test("PostgreSQL creates the raw and mask columns and records both sentinels", async () => {
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("encshape_pg");
  const work = project();
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const applied = apply(work, pgUrl(), namespace);
    assert.equal(applied.code, 0, `the migration must apply; ${applied.text}`);

    const { rows } = await client.query(
      `SELECT column_name, data_type,
              col_description(($1 || '.' || $2)::regclass, ordinal_position) AS comment
         FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position`,
      [namespace, TABLE],
    );

    assert.deepEqual(
      rows.map((row: { column_name: string }) => row.column_name),
      ["id", "__zs_raw__secret", "secret"],
      "the catalog matches the encrypted field's physical storage contract",
    );
    assert.equal(rows[1].data_type, "bytea", "the encrypted column is opaque bytes");
    assert.ok(
      String(rows[1].comment).startsWith(ENC_SENTINEL),
      `the encrypted column must carry its sentinel; got ${JSON.stringify(rows[1].comment)}`,
    );
    assert.ok(
      String(rows[2].comment).startsWith(MASK_SENTINEL),
      `the companion must carry the mask sentinel; got ${JSON.stringify(rows[2].comment)}`,
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${namespace}" CASCADE;
         DROP SCHEMA IF EXISTS "${namespace}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});

test("SQLite carries both sentinels inline, having no comment feature", () => {
  const work = project();
  try {
    const appPath = join(work, "app.db");
    const applied = apply(work, `sqlite:${appPath}`, null);
    assert.equal(applied.code, 0, `the migration must apply; ${applied.text}`);

    const db = new DatabaseSync(appPath, { readOnly: true });
    const ddl = (
      db.prepare(`SELECT sql FROM sqlite_master WHERE name = ?`).get(TABLE) as { sql: string }
    ).sql;
    const columns = (
      db.prepare(`SELECT name FROM pragma_table_info(?)`).all(TABLE) as Array<{ name: string }>
    ).map((row) => row.name);
    db.close();

    assert.deepEqual(columns, ["id", "__zs_raw__secret", "secret"]);
    // Preserved in `sqlite_master.sql`, which is how a reader recovers them.
    assert.ok(ddl.includes(ENC_SENTINEL), `the DDL must carry the enc sentinel; got ${ddl}`);
    assert.ok(ddl.includes(MASK_SENTINEL), `the DDL must carry the mask sentinel; got ${ddl}`);
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("MySQL creates both columns but records NEITHER sentinel", async () => {
  const databaseUrl = mysqlUrl();
  const driver = (await import("mysql2/promise")).default;
  const admin = await driver.createConnection({ uri: databaseUrl });
  const base = databaseUrl.replace(/\/[^/]*$/, "");
  const namespace = uniqueNamespace("encshape_my");
  const work = project();
  try {
    await admin.query(`CREATE DATABASE \`${namespace}\``);
    const applied = apply(work, `${base}/${namespace}`, namespace);
    assert.equal(applied.code, 0, `the migration must apply; ${applied.text}`);

    const [rows] = await admin.query(
      `SELECT COLUMN_NAME AS n, COLUMN_TYPE AS t, COLUMN_COMMENT AS c
         FROM information_schema.COLUMNS
        WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION`,
      [namespace, TABLE],
    );
    const columns = rows as Array<{ n: string; t: string; c: string }>;

    assert.deepEqual(
      columns.map((row) => row.n),
      ["id", "__zs_raw__secret", "secret"],
      "the physical storage contract is the same on MySQL",
    );

    // Recorded, not endorsed. MySQL supports COLUMN_COMMENT natively, so this is
    // not a capability limit; SQLite, which has no comment feature at all, still
    // manages to carry both sentinels. If the sentinel is ever emitted here, this
    // assertion is the one that should fail and prompt a decision.
    assert.equal(
      columns[1].c,
      "",
      "current behaviour: the encrypted column carries no sentinel on MySQL",
    );
    assert.equal(
      columns[2].c,
      "",
      "current behaviour: the companion carries no mask sentinel on MySQL",
    );
  } finally {
    await admin.query(`DROP DATABASE IF EXISTS \`${namespace}\``).catch(() => {});
    await admin.query(`DROP DATABASE IF EXISTS \`${namespace}_migrations\``).catch(() => {});
    await admin.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
