// `concatWs` means the same thing on all three targets, NULLs included.
//
// Two of the three get it from the server: PostgreSQL and MySQL both have a native
// `concat_ws`. SQLite has none, so `render_concat_ws` emulates it by hand — a fold
// that appends `delimiter || value` only when the value is not NULL, then strips the
// leading delimiter with `substr(…, length(delim) + 1)`. Its own comment notes the
// strip is only correct because the delimiter is a literal by the op shape.
//
// That is a hand-written re-implementation of another engine's semantics, and it
// had ZERO end-to-end coverage — `concatWs` appeared in no host test at all.
//
// NULL IS THE WHOLE DIFFICULTY, and the cases below are chosen so that a plausible
// wrong emulation fails at least one:
//
//   all set     a-b-c   the trivial case
//   p NULL      b-c     a leading NULL must not leave a leading delimiter
//   q NULL      a-c     a middle NULL must not double the delimiter
//   r NULL      a-b     a trailing NULL must not leave a trailing one
//   all NULL    ""      concat_ws returns the EMPTY STRING, not NULL
//   q empty     a--c    an empty string is NOT a NULL: it is included, and the
//                       delimiter doubles around it
//
// The last two are the ones that separate a correct implementation from a
// coalesce-everything-to-'' shortcut: that shortcut passes the first four, returns
// "--" instead of "" for all-NULL, and cannot tell case 5 from case 6 at all.
//
// The assertion is cross-dialect EQUALITY rather than a hardcoded expectation per
// dialect. The property that matters to an author is that one authored expression
// means one thing everywhere; pinning three separate literals would still pass if
// all three drifted together in a way the DSL never promised.
//
// GATES: `ZERO_MIGRATE_TEST_PG_URL` and `ZERO_MIGRATE_MYSQL_URL`; SQLite always runs.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
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

const OWNER_APP = "app_concatws";
const TABLE = "cws_t";

/** The six NULL/empty shapes, and the values every dialect must agree on. */
const EXPECTED: readonly string[] = ["a-b-c", "b-c", "a-c", "a-b", "", "a--c"];

const ROWS = [
  `{ id: 1, p: "a", q: "b", r: "c" }`,
  `{ id: 2, p: null, q: "b", r: "c" }`,
  `{ id: 3, p: "a", q: null, r: "c" }`,
  `{ id: 4, p: "a", q: "b", r: null }`,
  `{ id: 5, p: null, q: null, r: null }`,
  `{ id: 6, p: "a", q: "", r: "c" }`,
];

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(): string {
  const work = mkdtempSync(join(HERE, "concatws-"));
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

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`,
  );
  writeFileSync(join(work, "registry.json"), JSON.stringify({ [TABLE]: OWNER_APP }));
  writeFileSync(
    join(work, "migrations", "20260101000000_a.ts"),
    `import { table, t } from "zero-migrate";
export const name = "a";
export default {
  schema() {
    table("${TABLE}").create({
      columns: {
        id: t.int().notNull(), p: t.text(), q: t.text(), r: t.text(), out: t.text(),
      },
      primaryKey: ["id"],
    });
  },
};
`,
  );
  writeFileSync(
    join(work, "migrations", "20260102000000_b.ts"),
    `import { table } from "zero-migrate";
export const name = "b";
export default {
  data() {
    table("${TABLE}").insert({ rows: [${ROWS.join(", ")}] });
  },
  inverse() {
    table("${TABLE}").delete({ where: (col) => col("id").in([1, 2, 3, 4, 5, 6]) });
  },
};
`,
  );
  writeFileSync(
    join(work, "migrations", "20260103000000_c.ts"),
    `import { table, concatWs } from "zero-migrate";
export const name = "c";
export default {
  data() {
    table("${TABLE}").update({
      set: { out: (col) => concatWs("-", col("p"), col("q"), col("r")) },
      where: (col) => col("id").gt(0),
    });
  },
  irreversible: "overwrites out for rows with positive ids; prior out values are not recorded",
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

test("PostgreSQL's native concat_ws produces the expected NULL semantics", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("cws_pg");
  const work = project();
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const applied = apply(work, pgUrl(), namespace);
    assert.equal(applied.code, 0, `the migration must apply; ${applied.text}`);
    const { rows } = await client.query(
      `SELECT out FROM "${namespace}"."${TABLE}" ORDER BY id`,
    );
    assert.deepEqual(rows.map((row: { out: string }) => row.out), EXPECTED);
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

test("MySQL's native concat_ws matches", async (ctx) => {
  const mysqlUrl = process.env.ZERO_MIGRATE_MYSQL_URL;
  if (!mysqlUrl) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset");
    return;
  }
  const driver = (await import("mysql2/promise")).default;
  const admin = await driver.createConnection({ uri: String(mysqlUrl) });
  const base = String(mysqlUrl).replace(/\/[^/]*$/, "");
  const namespace = uniqueNamespace("cws_my");
  const work = project();
  try {
    await admin.query(`CREATE DATABASE \`${namespace}\``);
    const applied = apply(work, `${base}/${namespace}`, namespace);
    assert.equal(applied.code, 0, `the migration must apply; ${applied.text}`);
    const [rows] = await admin.query(
      `SELECT \`out\` AS o FROM \`${namespace}\`.\`${TABLE}\` ORDER BY id`,
    );
    assert.deepEqual((rows as Array<{ o: string }>).map((row) => row.o), EXPECTED);
  } finally {
    await admin.query(`DROP DATABASE IF EXISTS \`${namespace}\``).catch(() => {});
    await admin.query(`DROP DATABASE IF EXISTS \`${namespace}_migrations\``).catch(() => {});
    await admin.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});

test("SQLite's hand-written emulation matches the native ones", () => {
  const work = project();
  try {
    const appPath = join(work, "app.db");
    const applied = apply(work, `sqlite:${appPath}`, null);
    assert.equal(applied.code, 0, `the migration must apply; ${applied.text}`);

    const db = new DatabaseSync(appPath, { readOnly: true });
    const rows = (
      db.prepare(`SELECT out FROM ${TABLE} ORDER BY id`).all() as Array<{ out: string }>
    ).map((row) => row.out);
    db.close();

    assert.deepEqual(
      rows,
      EXPECTED,
      "SQLite has no concat_ws, so this is an emulation: a leading NULL must not " +
        "leave a leading delimiter, all-NULL must be the empty string rather than " +
        "NULL, and an empty string must be INCLUDED where a NULL is skipped",
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
