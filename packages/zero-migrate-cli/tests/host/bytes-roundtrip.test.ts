// Authored bytes must reach the database as the SAME bytes on every target.
//
// `byteValue(...)` is the only way to author binary data, and it is rendered by
// THREE different dialect encodings that share no code:
//
//   bound DML     PG `decode($1,'base64')` | MySQL `FROM_BASE64(?)` | SQLite raw bind
//   inline literal PG `decode('…','base64')` | MySQL `(X'…')`      | SQLite `X'…'`
//
// Two encodings (base64 and hex) and one direct bind, chosen per dialect and per
// position. That is six independent chances to lose a byte, and before this file
// every one of them was covered only by IR-shape unit tests in `ops.test.ts` --
// which assert what the RECORDER built, never what the DATABASE stored.
//
// A wrong encoding here does not fail the migration. `FROM_BASE64` on hex input,
// or `decode` on already-decoded bytes, returns SOMETHING; the row lands, apply
// exits 0, and the corruption is found later by whatever reads the column. So the
// assertion is the stored bytes read back through the database's own `hex()`, not
// the driver's rendering of them -- the same lesson `literal-value-binding.test.ts`
// records about `DatabaseSync` returning TEXT for a BLOB on some Node versions.
//
// THE PAYLOAD IS CHOSEN TO BREAK NAIVE ENCODINGS, not to look like data:
//   0x00  a NUL, which truncates anything routed through a C string
//   0x80  the first byte that is not valid standalone UTF-8
//   0xFF  the byte a sign-extending conversion turns into -1
//   and a length of 10, so the base64 form carries `==` padding
//
// Both POSITIONS are covered, because they are different renderers: `payload`
// arrives through the bound-DML path, `stamped` through the inline-literal path
// as a column DEFAULT the insert never mentions. MySQL is the interesting leg for
// the default -- BLOB columns there cannot take a plain literal default at all,
// which is why the renderer wraps it as the expression default `(X'…')`.
//
// GATES: `ZERO_MIGRATE_TEST_PG_URL`, `ZERO_MIGRATE_MYSQL_URL`. SQLite always runs.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const PG_URL = process.env.ZERO_MIGRATE_TEST_PG_URL;
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const OWNER_APP = "app_bytes_roundtrip";
const TABLE = "bytes_rows";

/** Bound through DML. NUL, a lone 0x80, 0xFF, and a length whose base64 pads. */
const PAYLOAD = [0x00, 0x01, 0x7f, 0x80, 0xff, 0xde, 0xad, 0xbe, 0xef, 0x10] as const;
/** Rendered as an inline literal in the column DEFAULT. Same hostile bytes. */
const STAMPED = [0xff, 0x00, 0x80, 0x7f, 0x00, 0x01] as const;

const PAYLOAD_HEX = Buffer.from(PAYLOAD).toString("hex").toUpperCase();
const STAMPED_HEX = Buffer.from(STAMPED).toString("hex").toUpperCase();

// How each dialect spells the SAME bytes inside its stored column default. These
// are what the catalogs actually report, not a guess: PostgreSQL keeps the
// engine's `decode(…,'base64')` call, MySQL normalizes `X'…'` to `0x…`, SQLite
// keeps `X'…'` verbatim.
const STAMPED_B64 = Buffer.from(STAMPED).toString("base64");
const STAMPED_HEX_LOWER = Buffer.from(STAMPED).toString("hex");

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(): string {
  const work = mkdtempSync(join(HERE, "bytesrt-"));
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
    join(work, "migrations", "20260101000000_base.ts"),
    `import { byteValue, table, t } from "zero-migrate";
export const name = "base";
export default {
  schema() {
    table("${TABLE}").create({
      columns: {
        id: t.int().notNull(),
        payload: t.bytes(),
        stamped: t.bytes().default(byteValue(new Uint8Array([${STAMPED.join(", ")}]))),
      },
      primaryKey: ["id"],
    });
  },
};
`,
  );
  writeFileSync(
    join(work, "migrations", "20260102000000_seed.ts"),
    `import { byteValue, table } from "zero-migrate";
export const name = "seed";
export default {
  data() {
    table("${TABLE}").insert({
      rows: [{ id: 1, payload: byteValue(new Uint8Array([${PAYLOAD.join(", ")}])) }],
    });
  },
  inverse() {
    table("${TABLE}").delete({ where: (col) => col("id").eq(1) });
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
): Promise<{ code: number | null; text: string }> {
  const schemaArgs = namespace ? ["--schema", namespace] : [];
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, "apply", "--approve",
        "--dir", join(work, "migrations"),
        "--database-url", databaseUrl,
        "--policy", join(work, "policy.toml"),
        "--registry", join(work, "registry.json"),
        ...schemaArgs,
        "--owner-app", OWNER_APP,
      ],
      {
        cwd: work,
        env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
      },
    );
    let out = "";
    let err = "";
    child.stdout.on("data", (chunk) => (out += chunk));
    child.stderr.on("data", (chunk) => (err += chunk));
    child.on("close", (code) =>
      resolvePromise({
        code,
        text: `${out}\n${err}`.replace(/^WARNING.*$/gm, "").trim(),
      }),
    );
  });
}

/** Compared through the DATABASE's own hex(), so no driver's idea of how to
 *  represent a blob can turn a byte-exact store into a false failure -- or, worse,
 *  a corrupt store into a false pass. */
function assertBytes(hex: unknown, expected: string, where: string): void {
  assert.equal(
    String(hex).toUpperCase(),
    expected,
    `${where}: the stored bytes must be exactly what was authored`,
  );
}

/** The stamped column's value only proves the INLINE-LITERAL renderer if the
 *  default is really declared in the DDL. If the engine ever started expanding
 *  defaults into the INSERT instead, the round-trip assertion above would still
 *  pass while silently testing the bound path twice -- so the catalog is checked
 *  too, and this is the assertion that would notice. */
function assertCatalogDefault(actual: unknown, needle: string, where: string): void {
  assert.ok(
    actual !== null && actual !== undefined,
    `${where}: the bytes default must be declared in the DDL, not expanded into the INSERT`,
  );
  assert.ok(
    String(actual).includes(needle),
    `${where}: the stored default must carry the authored bytes; got ${String(actual)}`,
  );
}

test("PostgreSQL stores authored bytes exactly, bound and defaulted", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("bytesrt_pg");
  const work = project();
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const applied = await apply(work, pgUrl(), namespace);
    assert.equal(applied.code, 0, `the bytes migration must apply; ${applied.text}`);

    const { rows } = await client.query(
      `SELECT encode(payload, 'hex') AS p, encode(stamped, 'hex') AS s
         FROM "${namespace}"."${TABLE}" WHERE id = 1`,
    );
    assert.equal(rows.length, 1, "the authored row must exist");
    // `decode($1,'base64')` on the bound path.
    assertBytes(rows[0].p, PAYLOAD_HEX, "PostgreSQL bound payload");
    // `decode('…','base64')` inlined into the column DEFAULT.
    assertBytes(rows[0].s, STAMPED_HEX, "PostgreSQL defaulted stamp");

    const catalog = await client.query(
      `SELECT column_default FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = $2 AND column_name = 'stamped'`,
      [namespace, TABLE],
    );
    assertCatalogDefault(
      catalog.rows[0]?.column_default,
      STAMPED_B64,
      "PostgreSQL stamped default",
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

test("MySQL stores authored bytes exactly, bound and defaulted", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset");
    return;
  }
  const driver = (await import("mysql2/promise")).default;
  const connection = await driver.createConnection({ uri: String(MYSQL_URL) });
  const namespace = uniqueNamespace("bytesrt_my");
  const work = project();
  try {
    await connection.query(`CREATE DATABASE \`${namespace}\``);
    const base = String(MYSQL_URL).replace(/\/[^/]*$/, "");
    const applied = await apply(work, `${base}/${namespace}`, namespace);
    // A BLOB column on MySQL cannot take a plain literal DEFAULT, so this exit
    // code is also the check that the renderer emitted the `(X'…')` expression
    // form rather than a bare literal MySQL would reject.
    assert.equal(applied.code, 0, `the bytes migration must apply on MySQL; ${applied.text}`);

    const [rows] = await connection.query(
      `SELECT HEX(payload) AS p, HEX(stamped) AS s
         FROM \`${namespace}\`.\`${TABLE}\` WHERE id = 1`,
    );
    const row = (rows as Array<{ p: string; s: string }>)[0];
    assert.ok(row, "the authored row must exist");
    // `FROM_BASE64(?)` on the bound path.
    assertBytes(row.p, PAYLOAD_HEX, "MySQL bound payload");
    // `(X'…')` as the BLOB expression default.
    assertBytes(row.s, STAMPED_HEX, "MySQL defaulted stamp");

    const [catalog] = await connection.query(
      `SELECT COLUMN_DEFAULT AS d, EXTRA AS e FROM information_schema.COLUMNS
        WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = 'stamped'`,
      [namespace, TABLE],
    );
    const declared = (catalog as Array<{ d: string | null; e: string }>)[0];
    // MySQL normalizes the engine's `X'…'` to `0x…` and marks the column
    // `DEFAULT_GENERATED` -- the expression form a BLOB column requires.
    assertCatalogDefault(declared?.d, `0x${STAMPED_HEX_LOWER}`, "MySQL stamped default");
    assert.equal(
      declared?.e,
      "DEFAULT_GENERATED",
      "a BLOB default must be the expression form MySQL accepts, not a bare literal",
    );
  } finally {
    await connection.query(`DROP DATABASE IF EXISTS \`${namespace}\``).catch(() => {});
    await connection.query(`DROP DATABASE IF EXISTS \`${namespace}_migrations\``).catch(() => {});
    await connection.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});

test("SQLite stores authored bytes exactly, bound and defaulted", async () => {
  const work = project();
  try {
    const applied = await apply(work, `sqlite:${join(work, "app.db")}`, null);
    assert.equal(applied.code, 0, `the bytes migration must apply on SQLite; ${applied.text}`);

    const db = new DatabaseSync(join(work, "app.db"), { readOnly: true });
    const row = db
      .prepare(`SELECT hex(payload) AS p, hex(stamped) AS s FROM ${TABLE} WHERE id = 1`)
      .get() as { p: string; s: string } | undefined;
    const declared = db
      .prepare(`SELECT dflt_value AS d FROM pragma_table_info('${TABLE}') WHERE name = 'stamped'`)
      .get() as { d: string | null } | undefined;
    db.close();

    assert.ok(row, "the authored row must exist");
    // The direct rusqlite byte bind -- the one leg that encodes nothing.
    assertBytes(row.p, PAYLOAD_HEX, "SQLite bound payload");
    // `X'…'` inlined into the column DEFAULT.
    assertBytes(row.s, STAMPED_HEX, "SQLite defaulted stamp");
    assertCatalogDefault(declared?.d, `X'${STAMPED_HEX_LOWER}'`, "SQLite stamped default");
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
