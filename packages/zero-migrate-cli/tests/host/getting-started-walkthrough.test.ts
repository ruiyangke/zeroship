// The getting-started walkthrough, run as a SEQUENCE.
//
// Every verb here is covered individually elsewhere in this suite. What is not
// covered is the walkthrough itself: the documented migrations, the documented
// `policy.toml` grants, the documented `table-owners.json`, and the documented
// flag combinations, in the documented order. That is what a new user actually
// executes, and it can break without any single-verb test noticing.
//
// Three ways it could, none of which the existing tests would catch:
//
//   - the policy grants the doc prints stop being sufficient (a new default-deny
//     knob), so step 5 refuses;
//   - the rename's type-identity rule changes, so the documented
//     `t.string({ length: 255 })` no longer matches the column step 2 declared -
//     the doc has a comment warning about exactly this;
//   - `--registry ./table-owners.json` stops being enough to attribute `users`,
//     so the rename is refused for ownership.
//
// The migrations are the doc's own, near-verbatim. The comments in the original
// are dropped and nothing else, so a drift between this file and the page is a
// drift in the API rather than in prose.
//
// The last assertion is the data. A walkthrough that ended with the right column
// name and an empty row would satisfy every exit code and be the worst possible
// first experience.
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

const OWNER_APP = "app_demo";
const SEEDED_ID = "user_01h455vb4pex5vsknk084sn02q";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** The doc's `policy.toml`, including the destructive-ops grant it adds later. */
function policy(schema: string): string {
  return `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = [${JSON.stringify(schema)}] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = [${JSON.stringify(schema)}] }

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`;
}

/** The doc's step-2 migration. */
const CREATE_USERS = `import { ids, now, table, t } from "zero-migrate";

export const name = "create_users";

export default {
  up() {
    table("users").create({
      columns: {
        id: ids.typeId({ prefix: "user" }).primaryKey(),
        email: t.string({ length: 254 }).notNull(),
        display_name: t.string({ length: 255 }),
        state: t.string({ length: 32 }).notNull().default("invited"),
        created_at: t.timestamp().notNull().default(now()),
      },
    });

    table("users").index("users_email_uq").add({ on: ["email"], unique: true });

    table("users").insert({
      rows: {
        id: ${JSON.stringify(SEEDED_ID)},
        email: "first@example.com",
        display_name: "First user",
      },
    });
  },
};
`;

/** The doc's rename migration, including its type-identity requirement. */
const RENAME = `import { table, t } from "zero-migrate";

export const name = "rename_user_display_name";

export default {
  up() {
    table("users").column("display_name").rename({
      to: "full_name",
      type: t.string({ length: 255 }),
    });
  },
};
`;

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "walkthrough-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(join(work, "policy.toml"), policy(schema));
  // The doc's `table-owners.json`.
  writeFileSync(join(work, "table-owners.json"), JSON.stringify({ users: OWNER_APP }, null, 2));
  writeFileSync(join(work, "migrations", "20260101000000_create_users.ts"), CREATE_USERS);
  return work;
}

function run(work: string, argv: string[]): { code: number | null; out: string; err: string } {
  const result = spawnSync(process.execPath, ["--import", "tsx", CLI_BIN, ...argv], {
    cwd: work,
    encoding: "utf8",
    env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
  });
  return {
    code: result.status,
    out: result.stdout ?? "",
    err: (result.stderr ?? "").replace(/^WARNING.*$/gm, "").trim(),
  };
}

test("the documented walkthrough runs end to end, in order, with the documented files", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("walkthrough");
  const work = project(schema);
  const base = [
    "--dir", "./migrations",
    "--database-url", pgUrl(),
    "--policy", "./policy.toml",
    "--schema", schema,
    "--owner-app", OWNER_APP,
  ];
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);

    // Step 5: the first apply, under the grants the page prints.
    const applied = run(work, ["apply", ...base]);
    assert.equal(applied.code, 0, `the documented policy must permit step 5; ${applied.err}`);

    // Step 6: machine-readable status.
    const status = run(work, ["status", ...base, "--json"]);
    assert.equal(status.code, 0, `status must succeed; ${status.err}`);
    const reply = JSON.parse(status.out) as { applied: string[]; pending: string[] };
    assert.equal(reply.pending.length, 0, "nothing may be pending after the first apply");
    assert.equal(reply.applied.length, 1, "and the first migration must be applied");

    // Step 7: author the rename, preview it with the documented registry.
    writeFileSync(join(work, "migrations", "20260102000000_rename.ts"), RENAME);
    const linted = run(work, [
      "lint", "--dir", "./migrations", "--explain", "--dialect", "postgres",
      "--registry", "./table-owners.json", "--schema", schema, "--owner-app", OWNER_APP,
    ]);
    assert.equal(linted.code, 0, `the documented registry must attribute users; ${linted.err}`);

    // Step 8: apply it. The window opens, both columns present.
    const renamed = run(work, [
      "apply", ...base, "--registry", "./table-owners.json", "--approve",
    ]);
    assert.equal(renamed.code, 0, `the rename must apply; ${renamed.err}`);
    const { rows: during } = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'users' ORDER BY column_name`,
      [schema],
    );
    const names = during.map((row) => row.column_name);
    assert.ok(
      names.includes("display_name") && names.includes("full_name"),
      `the coexistence window must hold both columns; got ${names.join(",")}`,
    );

    // Step 9: resolve it.
    const resolved = run(work, [
      "resolve", "rename_user_display_name", "--commit", "--approve",
      ...base, "--registry", "./table-owners.json",
    ]);
    assert.equal(resolved.code, 0, `the documented resolve must succeed; ${resolved.err}`);

    const { rows: after } = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'users' ORDER BY column_name`,
      [schema],
    );
    assert.deepEqual(
      after.map((row) => row.column_name),
      ["created_at", "email", "full_name", "id", "state"],
      "the commit drops the old column and keeps the rest",
    );

    // The row the walkthrough seeded, carried the whole way. A run that ended
    // with the right column and no data would satisfy every exit code above.
    const { rows } = await client.query(
      `SELECT id, email, full_name FROM "${schema}".users`,
    );
    assert.deepEqual(
      rows,
      [{ id: SEEDED_ID, email: "first@example.com", full_name: "First user" }],
      "the seeded row must survive the rename with its value under the new name",
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
