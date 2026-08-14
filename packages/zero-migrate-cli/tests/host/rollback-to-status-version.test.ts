// A version `status` reports must be a version `rollback --to` accepts.
//
// The engine carries two identifier spaces. A logical PLAN has a version, and the
// journal records the versions of the STEPS that plan lowered to. `status` reports
// plan versions in `applied` and `pending`; the journal, `apply`'s output, and
// `rollback` all speak step versions. They are different `mig_…` values for the
// same migration.
//
// That made the obvious operator workflow impossible: read a version from
// `status`, roll back to it. Every version `status` reported as applied was
// rejected with `is not currently applied`, while `--steps` and `--all` worked
// fine -- so the verb looked healthy right up until someone used the one targeting
// flag that names a version.
//
// Worse, one status reply mixed both spaces: `rolledBack` carried step versions
// while `applied` and `pending` carried plan versions, so a consumer could not
// correlate its own reply's fields. That is asserted here too.
//
// `--to` now accepts EITHER spelling. The resolution is unambiguous because
// rollback only handles plans that lower to exactly one journaled step -- a
// multi-step plan is refused as unrepresentable before any of this -- so plan and
// step stand 1:1 for everything reachable.
//
// BOTH SPELLINGS ARE ASSERTED. Accepting the plan version while breaking the step
// version would trade one broken workflow for another, and the step version is the
// one `apply`'s output gives, which is what the docs told operators to use.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`.

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
const OWNER_APP = "app_rb_version";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(): string {
  const work = mkdtempSync(join(HERE, "rbver-"));
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
  writeFileSync(
    join(work, "registry.json"),
    JSON.stringify({ rbv_one: OWNER_APP, rbv_two: OWNER_APP }),
  );
  writeFileSync(
    join(work, "migrations", "20260101000000_one.ts"),
    `import { table, t } from "zero-migrate";
export const name = "one";
export default { schema() {
  table("rbv_one").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
} };
`,
  );
  writeFileSync(
    join(work, "migrations", "20260102000000_two.ts"),
    `import { table, t } from "zero-migrate";
export const name = "two";
export default { schema() {
  table("rbv_two").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
} };
`,
  );
  return work;
}

function cli(
  work: string,
  namespace: string,
  verb: string,
  extra: readonly string[],
): Promise<{ code: number | null; text: string }> {
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, verb,
        "--dir", join(work, "migrations"),
        "--database-url", pgUrl(),
        "--policy", join(work, "policy.toml"),
        "--registry", join(work, "registry.json"),
        "--schema", namespace,
        "--owner-app", OWNER_APP,
        ...extra,
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

/** The step versions the journal recorded, in authored order, as `apply` printed
 *  them. This is the spelling the docs tell operators to use. */
function stepVersionsFrom(applyOutput: string): string[] {
  const versions: string[] = [];
  for (const line of applyOutput.split("\n")) {
    const match = /"applied":\["(mig_[^"]+)"\]/.exec(line);
    if (match) versions.push(match[1]);
  }
  return versions;
}

async function setUp(namespace: string): Promise<{
  work: string;
  planVersions: string[];
  stepVersions: string[];
}> {
  const work = project();
  const applied = await cli(work, namespace, "apply", ["--approve"]);
  assert.equal(applied.code, 0, `setup apply must succeed; ${applied.text}`);
  const stepVersions = stepVersionsFrom(applied.text);
  assert.equal(stepVersions.length, 2, `setup must report two step versions; ${applied.text}`);

  const status = await cli(work, namespace, "status", ["--json"]);
  assert.equal(status.code, 0, `setup status must succeed; ${status.text}`);
  const planVersions = (JSON.parse(status.text) as { applied: string[] }).applied;
  assert.equal(planVersions.length, 2, `setup status must list two applied plans; ${status.text}`);
  return { work, planVersions, stepVersions };
}

async function tables(client: import("pg").Client, namespace: string): Promise<string[]> {
  const { rows } = await client.query(
    `SELECT table_name FROM information_schema.tables
      WHERE table_schema = $1 AND table_name LIKE 'rbv_%' ORDER BY 1`,
    [namespace],
  );
  return rows.map((row: { table_name: string }) => row.table_name);
}

test("rollback --to accepts the version status reports as applied", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("rbver_plan");
  let work = "";
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const setup = await setUp(namespace);
    work = setup.work;

    // The premise: status genuinely reports something other than the journal.
    // If these ever coincide the test still passes, but it stops being a
    // regression test for the two-space problem, so it is asserted rather than
    // assumed.
    assert.notDeepEqual(
      setup.planVersions,
      setup.stepVersions,
      "status and the journal are expected to use different version spellings; " +
        "if they have been unified, this test's premise is gone and it should be revisited",
    );
    assert.deepEqual(await tables(client, namespace), ["rbv_one", "rbv_two"]);

    // Roll back TO the first plan: keep it, unwind everything after.
    const result = await cli(setup.work, namespace, "rollback", [
      "--approve",
      "--to",
      setup.planVersions[0],
    ]);
    assert.equal(
      result.code,
      0,
      `a version status reported as applied must be a usable rollback target; ${result.text}`,
    );
    assert.deepEqual(
      await tables(client, namespace),
      ["rbv_one"],
      "the anchor plan must be kept and only what came after it unwound",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${namespace}" CASCADE;
         DROP SCHEMA IF EXISTS "${namespace}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    if (work) rmSync(work, { recursive: true, force: true });
  }
});

test("rollback --to still accepts the journal version apply printed", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("rbver_step");
  let work = "";
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const setup = await setUp(namespace);
    work = setup.work;

    // The spelling that already worked, and the one the reference documents.
    // Accepting plan versions must not cost this.
    const result = await cli(setup.work, namespace, "rollback", [
      "--approve",
      "--to",
      setup.stepVersions[0],
    ]);
    assert.equal(result.code, 0, `the journal spelling must keep working; ${result.text}`);
    assert.deepEqual(await tables(client, namespace), ["rbv_one"]);
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${namespace}" CASCADE;
         DROP SCHEMA IF EXISTS "${namespace}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    if (work) rmSync(work, { recursive: true, force: true });
  }
});

test("a version in neither space is still refused", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("rbver_bogus");
  let work = "";
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const setup = await setUp(namespace);
    work = setup.work;

    // Widening what `--to` accepts must not turn it into a flag that accepts
    // anything: an unknown version is still an error, and nothing is unwound.
    const result = await cli(setup.work, namespace, "rollback", [
      "--approve",
      "--to",
      "mig_0000000000000000000000",
    ]);
    assert.equal(result.code, 1, `an unknown version must still be refused; ${result.text}`);
    assert.match(result.text, /not currently applied/i, result.text);
    assert.deepEqual(
      await tables(client, namespace),
      ["rbv_one", "rbv_two"],
      "a refused target must unwind nothing",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${namespace}" CASCADE;
         DROP SCHEMA IF EXISTS "${namespace}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    if (work) rmSync(work, { recursive: true, force: true });
  }
});
