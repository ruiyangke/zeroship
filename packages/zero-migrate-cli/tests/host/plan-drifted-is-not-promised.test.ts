// `plan` must not promise, or render SQL for, a migration `apply` aborts on.
//
// F663, the drift sibling of F662. Editing an APPLIED migration's source makes
// apply abort on checksum drift and makes status print a checksum-mismatch line.
// `plan` did neither: it counted the drifted migration in "would apply" and
// printed the EDITED statement as the SQL it would run.
//
// Measured before the fix, on live PostgreSQL:
//
//   PLAN    would apply 2 migrations
//           CREATE TABLE "..."."dr_t" ("id" integer PRIMARY KEY NOT NULL,
//                                      "drifted" text);
//   STATUS  checksum mismatch: dr_a, step create_table_dr_t (mig_...)
//   APPLY   checksum drift on mig_...: journal has 748dc0406a...
//
// TWO THINGS ARE WRONG, and the second is the worse one. Over-promising is what
// F662 was. Here `plan` also prints a statement that will never execute,
// describing a table that already exists in a DIFFERENT shape — so an operator
// reading it is misled about the database, not merely about the schedule.
//
// The information is in the reply `plan` already holds: `formatStatusHuman`
// walks the same `state === "drifted"` two functions away.
//
// THE CONTROL RUNS FIRST, in the same schema, so the source edit is the only
// variable. Without it, a fix that dropped migrations from `plan` in general
// would satisfy the drift arms while breaking every ordinary preview.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { connectLivePg, pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_plan_drift";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** The first migration, whose source is later edited to create the drift. */
function firstMigration(extraColumn: boolean): string {
  const columns = extraColumn
    ? `{ id: t.int().notNull(), drifted: t.text() }`
    : `{ id: t.int().notNull() }`;
  return `import { table, t } from "zero-migrate";
export const name = "pd_a";
export default {
  schema() {
    table("pd_t").create({ columns: ${columns}, primaryKey: ["id"] });
  },
};
`;
}

const SECOND_MIGRATION = `import { table, t } from "zero-migrate";
export const name = "pd_b";
export default {
  schema() {
    table("pd_u").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`;

function project(): string {
  const work = mkdtempSync(join(HERE, "planchk-"));
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
    JSON.stringify({ pd_t: OWNER_APP, pd_u: OWNER_APP }),
  );
  writeFileSync(join(work, "migrations", "20260101000000_a.ts"), firstMigration(false));
  return work;
}

function cli(
  work: string,
  schema: string,
  argv: readonly string[],
): { code: number | null; text: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, ...argv,
      "--dir", join(work, "migrations"),
      "--database-url", pgUrl(),
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      "--schema", schema,
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

test("plan does not promise or render SQL for a drifted migration", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("planchk");
  const work = project();
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    assert.equal(cli(work, schema, ["apply", "--approve"]).code, 0, "the first apply lands");

    // CONTROL FIRST: a second migration with NO drift previews normally. Run in
    // the same schema so the source edit below is the only variable.
    writeFileSync(join(work, "migrations", "20260101000001_b.ts"), SECOND_MIGRATION);
    const clean = cli(work, schema, ["plan"]);
    assert.equal(clean.code, 0, `an ordinary plan must succeed; ${clean.text}`);
    assert.match(clean.text, /would apply 1 migration\b/, `${clean.text}`);
    assert.doesNotMatch(clean.text, /drift/i, );

    // Now edit the APPLIED migration's source.
    writeFileSync(join(work, "migrations", "20260101000000_a.ts"), firstMigration(true));

    const drifted = cli(work, schema, ["plan"]);
    assert.equal(drifted.code, 0, `plan stays a describing read; ${drifted.text}`);

    // 1. The drifted migration is not work plan would do.
    assert.doesNotMatch(
      drifted.text,
      /would apply 2 migrations\b/,
      `a migration apply ABORTS on must not be counted as work plan would do; ` +
        `${drifted.text}`,
    );

    // 2. Its edited SQL must not be presented as a statement that will run. The
    //    table already exists WITHOUT this column, so printing it unlabelled
    //    misleads about the database itself, not merely about the schedule.
    const sqlLines = drifted.text
      .split(/\r?\n/)
      .filter((line) => /CREATE TABLE .*"pd_t"/.test(line));
    for (const line of sqlLines) {
      assert.doesNotMatch(
        line,
        /"drifted"/,
        `the edited statement must not be shown as runnable SQL; ${line}`,
      );
    }

    // 3. The drift is reported, in the wording status already uses.
    assert.match(
      drifted.text,
      /drift|mismatch/i,
      `plan must SAY the migration drifted rather than silently omitting it - a ` +
        `migration that vanishes from plan is a different lie; ${drifted.text}`,
    );

    // 4. And apply genuinely aborts, so plan is describing a real refusal.
    const applied = cli(work, schema, ["apply", "--approve"]);
    assert.equal(applied.code, 1, `apply must abort on drift; ${applied.text}`);
    assert.match(applied.text, /drift/i, `${applied.text}`);
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
