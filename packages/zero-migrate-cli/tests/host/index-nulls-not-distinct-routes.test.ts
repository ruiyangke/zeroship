// `nullsNotDistinct` works whichever way the index is authored.
//
// An index can be authored two ways, and the DSL type offers the SAME option set
// to both:
//
//   inline   table(x).create({ indexes: [{ name, on, unique, nullsNotDistinct }] })
//   add op   table(x).index(name).add({ on, unique, nullsNotDistinct })
//
// F647. Every option held on both routes -- `unique`, `where`, `include`, `with`,
// `only`, `using` -- except `nullsNotDistinct`, which the add op honoured and the
// inline form REFUSED outright:
//
//   malformed IR envelope: unknown field `nullsNotDistinct`, expected one of
//   `name`, `columns`, `unique`, `using`, `where`, `include`, `with`, `only`,
//   `nulls_not_distinct`
//
// The last name in that list is the whole bug. `IrIndex` -- the index inside a
// createTable op -- carried the field but no `rename_all = "camelCase"`, while the
// add op's `Op::CreateIndex` inherits one from the enclosing enum. So the wire name
// was snake_case on one route and camelCase on the other, and the DSL emits
// camelCase. `nulls_not_distinct` is the ONLY multi-word field on that struct,
// which is exactly why it was the only option that broke: for every sibling field
// the two spellings coincide, so nothing else could expose the missing attribute.
//
// `types.ts` offered the option on both routes and documented it as PG-supported,
// so this typechecked clean and failed at the wire.
//
// THE ASSERTION IS UNIQUENESS BEHAVIOUR, not the DDL text. `NULLS NOT DISTINCT`
// exists to make NULLs collide, so the test inserts TWO NULL rows and requires the
// second to be rejected. Reading `indexdef` back would confirm a string was
// rendered; only the insert confirms the index treats NULLs the way the author
// asked. The control is the same table WITHOUT the option, where both NULL rows
// must be ACCEPTED -- otherwise a plain unique index would satisfy the assertion
// just as well and the option would be proving nothing.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`. `NULLS NOT DISTINCT` is PG 15+.

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

const OWNER_APP = "app_ixnd";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** The same index, authored inline and via the add op, plus a no-option control. */
const SOURCE = `import { table, t } from "zero-migrate";
export const name = "a";
const cols = { id: t.int().notNull(), c: t.text() };
export default {
  up() {
    table("nd_inline").create({
      columns: cols,
      primaryKey: ["id"],
      indexes: [{ name: "ix_inline", on: ["c"], unique: true, nullsNotDistinct: true }],
    });
    table("nd_addop").create({ columns: cols, primaryKey: ["id"] });
    table("nd_addop").index("ix_addop").add({
      on: ["c"], unique: true, nullsNotDistinct: true,
    });
    // Control: a plain UNIQUE index, where SQL's default NULL semantics apply.
    table("nd_plain").create({
      columns: cols,
      primaryKey: ["id"],
      indexes: [{ name: "ix_plain", on: ["c"], unique: true }],
    });
  },
};
`;

function project(): string {
  const work = mkdtempSync(join(HERE, "ixnd-"));
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
  writeFileSync(
    join(work, "registry.json"),
    JSON.stringify({ nd_inline: OWNER_APP, nd_addop: OWNER_APP, nd_plain: OWNER_APP }),
  );
  writeFileSync(join(work, "migrations", "20260101000000_a.ts"), SOURCE);
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

test("lint accepts nullsNotDistinct on BOTH authoring routes", () => {
  const work = project();
  try {
    const linted = run(work, "lint", ["--dialect", "postgres"]);
    assert.equal(
      linted.code,
      0,
      `the inline route must accept the same option the add op does -- the DSL type ` +
        `offers it on both, so a wire-name mismatch here typechecks clean and fails ` +
        `at the envelope; ${linted.text}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("both routes make NULLs collide, and the control proves the option did it", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("ixnd");
  const work = project();
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const applied = run(work, "apply", ["--approve"], pgUrl(), namespace);
    assert.equal(applied.code, 0, `both routes must apply; ${applied.text}`);

    for (const table of ["nd_inline", "nd_addop"]) {
      await client.query(
        `INSERT INTO "${namespace}"."${table}" (id, c) VALUES (1, NULL)`,
      );
      await assert.rejects(
        () =>
          client.query(
            `INSERT INTO "${namespace}"."${table}" (id, c) VALUES (2, NULL)`,
          ),
        /duplicate key|unique/i,
        `${table}: NULLS NOT DISTINCT means a second NULL must COLLIDE. This is the ` +
          `behaviour the option exists for, and DDL text alone would not show it`,
      );
    }

    // Without the option, SQL's default applies and NULLs never collide. If this
    // row were rejected, the assertions above would be satisfied by any plain
    // unique index and would say nothing about nullsNotDistinct.
    await client.query(`INSERT INTO "${namespace}"."nd_plain" (id, c) VALUES (1, NULL)`);
    await client.query(`INSERT INTO "${namespace}"."nd_plain" (id, c) VALUES (2, NULL)`);

    // And the two routes must agree on the DDL they produced.
    const { rows } = await client.query(
      `SELECT indexname, indexdef FROM pg_indexes
        WHERE schemaname = $1 AND indexname IN ('ix_inline', 'ix_addop')
        ORDER BY indexname`,
      [namespace],
    );
    const defs = (rows as Array<{ indexname: string; indexdef: string }>).map((row) =>
      row.indexdef.replace(/ ON [^ ]+/, "").replace(/INDEX \w+/, "INDEX x"),
    );
    assert.equal(defs.length, 2, "both indexes must exist");
    assert.equal(
      new Set(defs).size,
      1,
      `the two routes must render one index, not two dialects of one; got ${JSON.stringify(defs)}`,
    );
    assert.match(defs[0], /NULLS NOT DISTINCT/);
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
