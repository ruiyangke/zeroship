// An empty project schema must be refused BEFORE anything touches the database.
//
// The CLI already validates emptiness for every other required setting -- the
// database URL, `--registry`, `--policy`, `--journal` all refuse a zero-length
// value by name. `schema` was the one that did not, from any source: the flag,
// `ZERO_MIGRATE_SCHEMA`, or a config `schema` field.
//
// The failure was not a clean error, and it was not the same failure everywhere:
//
//   SQLite       APPLIED, exit 0 -- schema is inert there, so nothing objected
//   PostgreSQL   bootstrapped a journal schema literally named `_migrations`
//                (that is "" + "_migrations", five tables) and THEN died on
//                `zero-length delimited identifier` from the SQL guard
//   MySQL        `Incorrect database name ''` straight from the server
//
// So one invalid configuration was accepted on one target, and on another left
// persistent state behind under a name no operator chose. The realistic way in is
// an unset CI variable: `ZERO_MIGRATE_SCHEMA="$DEPLOY_SCHEMA"` with `DEPLOY_SCHEMA`
// never exported is an empty string, not an absent one, so it does NOT fall back to
// the `public` default -- it overrides it.
//
// THE ASSERTIONS ARE THEREFORE (a) refusal naming the setting, and (b) on
// PostgreSQL, that NO `_migrations` schema exists afterwards. (b) is the half that
// matters: an error that still leaves a bootstrapped journal behind is not a
// refusal, it is a partial deploy that reported failure.
//
// Every arm carries a control with a real schema name, because a build where the
// schema setting was broken outright would satisfy every refusal above.
//
// GATE: PG needs `ZERO_MIGRATE_TEST_PG_URL`. SQLite always runs.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
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
const OWNER_APP = "app_empty_schema";
const TABLE = "empty_schema_rows";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(): string {
  const work = mkdtempSync(join(HERE, "emptyschema-"));
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
    `import { table, t } from "zero-migrate";
export const name = "base";
export default {
  schema() {
    table("${TABLE}").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`,
  );
  return work;
}

/** `schema` is passed through whichever source the caller names, so the refusal is
 *  proven for the flag AND the environment variable rather than for one of them. */
function apply(
  work: string,
  databaseUrl: string,
  schema: { via: "flag" | "env"; value: string },
): Promise<{ code: number | null; text: string }> {
  const viaFlag = schema.via === "flag";
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, "apply", "--approve",
        "--dir", join(work, "migrations"),
        "--database-url", databaseUrl,
        "--policy", join(work, "policy.toml"),
        "--registry", join(work, "registry.json"),
        ...(viaFlag ? ["--schema", schema.value] : []),
        "--owner-app", OWNER_APP,
      ],
      {
        cwd: work,
        env: {
          ...process.env,
          ZERO_MIGRATE_ADDON_PATH: ADDON_PATH,
          DATABASE_URL: "",
          ...(viaFlag ? {} : { ZERO_MIGRATE_SCHEMA: schema.value }),
        },
      },
    );
    let out = "";
    let err = "";
    child.stdout.on("data", (chunk) => (out += chunk));
    child.stderr.on("data", (chunk) => (err += chunk));
    child.on("close", (code) => resolvePromise({ code, text: `${out}\n${err}`.trim() }));
  });
}

/** By MESSAGE, not by exit code alone. Every dialect already failed this config
 *  somehow -- a parse error, a server error -- so "it exited non-zero" was true
 *  before the fix and proves nothing about whether the setting was validated. */
function assertRefused(result: { code: number | null; text: string }, where: string): void {
  assert.equal(result.code, 1, `${where}: an empty schema must be refused; ${result.text}`);
  assert.match(
    result.text,
    /schema/i,
    `${where}: the refusal must name the setting at fault; ${result.text}`,
  );
  assert.doesNotMatch(
    result.text,
    /zero-length delimited identifier|Incorrect database name/i,
    `${where}: the operator must not be shown the raw SQL-layer symptom; ${result.text}`,
  );
}

test("an empty schema is refused on SQLite, from the flag and the environment", async () => {
  for (const via of ["flag", "env"] as const) {
    const work = project();
    try {
      // SQLite is where this used to APPLY cleanly: schema is inert, so nothing
      // downstream had any reason to object.
      assertRefused(
        await apply(work, `sqlite:${join(work, "app.db")}`, { via, value: "" }),
        `SQLite via ${via}`,
      );
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  }
});

test("a real schema still applies on SQLite", async () => {
  const work = project();
  try {
    const applied = await apply(work, `sqlite:${join(work, "app.db")}`, {
      via: "flag",
      value: "main",
    });
    assert.equal(applied.code, 0, `the control must still apply; ${applied.text}`);
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("an empty schema is refused on PostgreSQL before any journal is created", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const strayExists = async (): Promise<boolean> =>
    (await client.query(`SELECT 1 FROM pg_namespace WHERE nspname = '_migrations'`)).rows.length > 0;
  try {
    // Start from a known-clean state, or "it exists afterwards" is unattributable.
    await client.query(`DROP SCHEMA IF EXISTS "_migrations" CASCADE`);
    assert.equal(await strayExists(), false, "the probe must start with no stray schema");

    for (const via of ["flag", "env"] as const) {
      const work = project();
      try {
        assertRefused(await apply(work, pgUrl(), { via, value: "" }), `PostgreSQL via ${via}`);
        // The half that matters. `"" + "_migrations"` is a name no operator chose,
        // and bootstrapping it before failing is a partial deploy, not a refusal.
        assert.equal(
          await strayExists(),
          false,
          "a refused deploy must not leave a bootstrapped `_migrations` journal schema behind",
        );
      } finally {
        rmSync(work, { recursive: true, force: true });
      }
    }
  } finally {
    await client.query(`DROP SCHEMA IF EXISTS "_migrations" CASCADE`).catch(() => {});
    await client.end().catch(() => {});
  }
});

test("a real schema still applies on PostgreSQL", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("emptyschema_pg");
  const work = project();
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const applied = await apply(work, pgUrl(), { via: "flag", value: namespace });
    assert.equal(applied.code, 0, `the control must still apply; ${applied.text}`);
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
