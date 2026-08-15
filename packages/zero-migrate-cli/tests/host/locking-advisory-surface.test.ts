// PostgreSQL locking advisories must reach the operator-facing CLI.
//
// The analyzer already identifies the ADD CONSTRAINT emitted for a unique
// addColumn as an ACCESS EXCLUSIVE hazard. These tests keep that information
// visible on both commands an operator reads before deploy: offline
// `lint --explain` and the live `plan` preview. The plain-column controls are
// essential: an unconditional warning would otherwise satisfy the hazard arms.
//
// GATES: none for lint; `ZERO_MIGRATE_TEST_PG_URL` for plan.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test, type TestContext } from "node:test";
import { fileURLToPath } from "node:url";

import { ADDON_PATH } from "./addon.js";
import { connectLivePg, pgUrl } from "./live-db.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const OWNER_APP = "app_locksurface";

type CliResult = { code: number | null; text: string };

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function baseProject(): string {
  const work = mkdtempSync(join(HERE, "locksurface-"));
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
  writeFileSync(join(work, "registry.json"), JSON.stringify({ lk_t: OWNER_APP }));
  writeFileSync(
    join(work, "migrations", "20260101000000_create.ts"),
    `import { table, t } from "zero-migrate";
export const name = "create_lk_t";
export default {
  schema() {
    table("lk_t").create({
      columns: { id: t.int().notNull() },
      primaryKey: ["id"],
    });
  },
};
`,
  );
  return work;
}

function addColumnMigration(work: string, unique: boolean): void {
  writeFileSync(
    join(work, "migrations", "20260101000001_add_e.ts"),
    `import { table, t } from "zero-migrate";
export const name = "add_e";
export default {
  schema() {
    table("lk_t").column("e").add({ type: t.text()${unique ? ".unique()" : ""} });
  },
};
`,
  );
}

function runCli(
  work: string,
  argv: string[],
  options: { schema: string; databaseUrl?: string },
): CliResult {
  const connection =
    options.databaseUrl === undefined ? [] : ["--database-url", options.databaseUrl];
  const result = spawnSync(
    process.execPath,
    [
      "--import",
      "tsx",
      CLI_BIN,
      ...argv,
      "--dir",
      join(work, "migrations"),
      ...connection,
      "--policy",
      join(work, "policy.toml"),
      "--registry",
      join(work, "registry.json"),
      "--schema",
      options.schema,
      "--owner-app",
      OWNER_APP,
    ],
    {
      cwd: work,
      encoding: "utf8",
      env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    },
  );
  return {
    code: result.status,
    text: `${result.stdout ?? ""}\n${result.stderr ?? ""}`.trim(),
  };
}

function assertLockAdvisory(result: CliResult, verb: string): void {
  assert.equal(result.code, 0, `${verb}: an advisory must not gate the command; ${result.text}`);
  assert.match(result.text, /advisory/i, `${verb}: the hazard must be identified as advisory`);

  const lockLines = result.text.split(/\r?\n/).filter((line) => /ACCESS\s+EXCLUSIVE/i.test(line));
  assert.ok(
    lockLines.length > 0,
    `${verb}: the advisory must name the ACCESS EXCLUSIVE lock; ${result.text}`,
  );
  assert.ok(
    lockLines.some((line) => /\blk_t\b/i.test(line) && /ADD\s+CONSTRAINT/i.test(line)),
    `${verb}: one advisory line must name table lk_t, statement kind ADD CONSTRAINT, ` +
      `and ACCESS EXCLUSIVE; got: ${lockLines.join(" | ")}`,
  );
}

function assertNoLockAdvisory(result: CliResult, verb: string): void {
  assert.equal(result.code, 0, `${verb}: a plain addColumn must still succeed; ${result.text}`);
  assert.doesNotMatch(
    result.text,
    /ACCESS\s+EXCLUSIVE|validating index/i,
    `${verb}: a plain addColumn must surface no locking advisory; ${result.text}`,
  );
}

function lint(unique: boolean): CliResult {
  const work = baseProject();
  try {
    addColumnMigration(work, unique);
    return runCli(work, ["lint", "--explain", "--dialect", "postgres"], {
      schema: "lint_locksurface",
    });
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}

async function plan(t: TestContext, unique: boolean): Promise<CliResult | null> {
  const client = await connectLivePg(t);
  if (!client) return null;

  const schema = uniqueNamespace(unique ? "lock_adv_unique" : "lock_adv_plain");
  const work = baseProject();
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const created = runCli(work, ["apply"], { schema, databaseUrl: pgUrl() });
    assert.equal(
      created.code,
      0,
      `the earlier table-creation migration must apply before plan; ${created.text}`,
    );

    addColumnMigration(work, unique);
    return runCli(work, ["plan"], { schema, databaseUrl: pgUrl() });
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
}

test("lint --explain surfaces the unique addColumn ACCESS EXCLUSIVE advisory without gating", () => {
  assertLockAdvisory(lint(true), "lint --explain");
});

test("lint --explain control: a plain addColumn surfaces no locking advisory", () => {
  assertNoLockAdvisory(lint(false), "lint --explain");
});

test("plan surfaces the unique addColumn ACCESS EXCLUSIVE advisory without gating", async (t) => {
  const result = await plan(t, true);
  if (result) assertLockAdvisory(result, "plan");
});

test("plan control: a plain addColumn surfaces no locking advisory", async (t) => {
  const result = await plan(t, false);
  if (result) assertNoLockAdvisory(result, "plan");
});
