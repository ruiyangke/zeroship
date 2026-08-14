// The PostgreSQL-only expression helpers are refused by LINT, not just by apply.
//
// `dialects.md#expressions` lists them:
//
//   | `currentSetting`, `currentUser` | Yes | No | No |
//   | `interval`                      | Yes | No | No |
//
// F627 is why this file exists. There, `t.vector` and `t.geoPoint` passed
// `lint --dialect mysql` and then failed at apply — green CI, broken deploy, which
// is the one failure the lint verb exists to prevent. That defect was specific to a
// derived index, but nothing had checked whether the same gap existed for the
// documented PG-only EXPRESSIONS, which reach the renderer by a different route.
//
// They do not: lint refuses all three on both non-PostgreSQL targets, matching what
// apply does.
//
// THE POSTGRESQL ARM IS NOT DECORATION. Six results reading "lint 1, apply 1" are
// equally consistent with three expressions that are correctly dialect-gated and
// with three call shapes that are simply malformed — a typo'd helper fails
// everywhere too. The PostgreSQL arm is what tells those apart: `interval` and
// `currentUser` apply cleanly there, so the refusals elsewhere are genuine dialect
// discrimination.
//
// `currentSetting` is asserted only as far as LINT on PostgreSQL. Applying it needs
// the named GUC to exist on the server — an unset one fails at apply with
// `unrecognized configuration parameter`, which is a property of the server's
// configuration rather than of this engine, and lint cannot know it without a
// connection. Asserting the apply there would pin the test host's GUCs.
//
// GATES: SQLite lint runs anywhere; the apply arms need `ZERO_MIGRATE_TEST_PG_URL`
// and `ZERO_MIGRATE_MYSQL_URL`.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
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

const OWNER_APP = "app_pgexpr";
const TABLE = "pgx_t";

interface CaseBase {
  readonly what: string;
  readonly imports: string;
  readonly body: string;
  /** Whether applying it on PostgreSQL depends only on the engine (see header). */
  readonly appliesOnPg: boolean;
}

type Case = CaseBase &
  (
    | { readonly inverseBody: string; readonly irreversible?: never }
    | { readonly inverseBody?: never; readonly irreversible: string }
  );

const CASES: readonly Case[] = [
  {
    what: "interval",
    imports: `import { table, interval } from "zero-migrate";`,
    body: `table("${TABLE}").update({ set: { ts: (col) => col("ts").add(interval({ minutes: 1 })) }, where: (col) => col("id").gt(0) });`,
    inverseBody: `table("${TABLE}").update({ set: { ts: (col) => col("ts").sub(interval({ minutes: 1 })) }, where: (col) => col("id").gt(0) });`,
    appliesOnPg: true,
  },
  {
    what: "currentUser",
    imports: `import { table, currentUser } from "zero-migrate";`,
    body: `table("${TABLE}").update({ set: { note: () => currentUser() }, where: (col) => col("id").gt(0) });`,
    irreversible: `overwrites note with the applying database user for existing ${TABLE} rows; prior note values are not recorded`,
    appliesOnPg: true,
  },
  {
    what: "currentSetting",
    imports: `import { table, currentSetting } from "zero-migrate";`,
    body: `table("${TABLE}").update({ set: { note: () => currentSetting("app.tenant") }, where: (col) => col("id").gt(0) });`,
    irreversible: `overwrites note with app.tenant for existing ${TABLE} rows; prior note values are not recorded`,
    appliesOnPg: false,
  },
];

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(testCase: Case): string {
  const work = mkdtempSync(join(HERE, "pgexpr-"));
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
      columns: { id: t.int().notNull(), ts: t.timestamp(), note: t.text() },
      primaryKey: ["id"],
    });
  },
};
`,
  );
  writeFileSync(
    join(work, "migrations", "20260101000001_seed.ts"),
    `import { table } from "zero-migrate";
export const name = "seed";
export default {
  data() {
    table("${TABLE}").insert({ rows: [{ id: 1 }] });
  },
  inverse() {
    table("${TABLE}").delete({ where: (col) => col("id").eq(1) });
  },
};
`,
  );
  writeFileSync(
    join(work, "migrations", "20260102000000_b.ts"),
    `${testCase.imports}
export const name = "b";
export default {
  data() { ${testCase.body} },
  ${
    testCase.inverseBody === undefined
      ? `irreversible: ${JSON.stringify(testCase.irreversible)},`
      : `inverse() { ${testCase.inverseBody} },`
  }
};
`,
  );
  return work;
}

function run(
  work: string,
  verb: string,
  extra: readonly string[],
  databaseUrl?: string,
  namespace?: string,
): { code: number | null; text: string } {
  const args = [
    "--import", "tsx", CLI_BIN, verb, ...extra,
    "--dir", join(work, "migrations"),
    "--policy", join(work, "policy.toml"),
    "--registry", join(work, "registry.json"),
    "--owner-app", OWNER_APP,
  ];
  if (databaseUrl) args.push("--database-url", databaseUrl);
  if (namespace) args.push("--schema", namespace);
  const result = spawnSync(process.execPath, args, {
    cwd: work,
    encoding: "utf8",
    env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
  });
  return {
    code: result.status,
    text: `${result.stdout ?? ""}\n${result.stderr ?? ""}`.replace(/^WARNING.*$/gm, "").trim(),
  };
}

test("lint refuses every PG-only expression on SQLite, and apply agrees", () => {
  for (const testCase of CASES) {
    const work = project(testCase);
    try {
      const linted = run(work, "lint", ["--dialect", "sqlite"]);
      const applied = run(work, "apply", ["--approve"], `sqlite:${join(work, "app.db")}`);
      assert.equal(
        linted.code,
        1,
        `${testCase.what}: lint must refuse it offline, not leave it to apply; ${linted.text}`,
      );
      assert.equal(applied.code, 1, `${testCase.what}: apply must refuse it too; ${applied.text}`);
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  }
});

test("lint refuses every PG-only expression on MySQL, and apply agrees", async (ctx) => {
  const mysqlUrl = process.env.ZERO_MIGRATE_MYSQL_URL;
  if (!mysqlUrl) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset");
    return;
  }
  const driver = (await import("mysql2/promise")).default;
  const admin = await driver.createConnection({ uri: String(mysqlUrl) });
  const base = String(mysqlUrl).replace(/\/[^/]*$/, "");
  try {
    for (const testCase of CASES) {
      const work = project(testCase);
      const namespace = uniqueNamespace("pgx_my");
      try {
        const linted = run(work, "lint", ["--dialect", "mysql"]);
        await admin.query(`CREATE DATABASE \`${namespace}\``);
        const applied = run(
          work,
          "apply",
          ["--approve"],
          `${base}/${namespace}`,
          namespace,
        );
        assert.equal(linted.code, 1, `${testCase.what}: lint must refuse it; ${linted.text}`);
        assert.equal(applied.code, 1, `${testCase.what}: apply must agree; ${applied.text}`);
      } finally {
        await admin.query(`DROP DATABASE IF EXISTS \`${namespace}\``).catch(() => {});
        await admin.query(`DROP DATABASE IF EXISTS \`${namespace}_migrations\``).catch(() => {});
        rmSync(work, { recursive: true, force: true });
      }
    }
  } finally {
    await admin.end().catch(() => {});
  }
});

/** Without this, every refusal above is equally consistent with three malformed
 *  call shapes: a typo'd helper is refused on every dialect too. */
test("CONTROL: the same expressions are accepted on PostgreSQL", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  try {
    for (const testCase of CASES) {
      const work = project(testCase);
      const namespace = uniqueNamespace("pgx_ctl");
      try {
        const linted = run(work, "lint", ["--dialect", "postgres"]);
        assert.equal(
          linted.code,
          0,
          `${testCase.what}: PostgreSQL must ACCEPT what the others refuse, or the ` +
            `refusals prove nothing about dialect gating; ${linted.text}`,
        );

        if (!testCase.appliesOnPg) continue;
        await client.query(`CREATE SCHEMA "${namespace}"`);
        const applied = run(work, "apply", ["--approve"], pgUrl(), namespace);
        assert.equal(
          applied.code,
          0,
          `${testCase.what}: and it must really run there; ${applied.text}`,
        );
      } finally {
        await client
          .query(
            `DROP SCHEMA IF EXISTS "${namespace}" CASCADE;
             DROP SCHEMA IF EXISTS "${namespace}_migrations" CASCADE`,
          )
          .catch(() => {});
        rmSync(work, { recursive: true, force: true });
      }
    }
  } finally {
    await client.end().catch(() => {});
  }
});
