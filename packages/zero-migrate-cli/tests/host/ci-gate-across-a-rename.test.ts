// The documented CI gate, through the full life of an online rename.
//
// `docs/operations.md` puts `status --strict` in the pipeline:
//
//   zero-migrate status --env production --strict --json > status.json || exit 1
//
// `documented-ci-gate.test.ts` covers it idle and under lock contention. Neither
// it nor anything else follows the gate across a RENAME, and a rename is the one
// ordinary operation that deliberately leaves the project in a non-clean state for
// a whole deploy cycle.
//
// That makes the third arm the one worth having. If `--strict` did not return to 0
// once the contract is committed, every pipeline in the org goes red permanently
// the first time a team ships an online rename — and it would stay green through
// every test this repo has, because no other test resolves a contract and then
// asks the gate.
//
// `resolve-lifecycle.test.ts` says "a resolved contract leaves no trace in the
// status reply", but that is about the wording of the resolve MESSAGE. This is
// about the EXIT CODE, which is the thing CI actually reads.
//
// The middle arm is the safety half and belongs here too: while the window is open
// the gate MUST fail, because further changes to that table are refused anyway and
// a pipeline that sailed past would deploy code against a half-renamed column.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`. PostgreSQL only - the online rename is
// PostgreSQL's.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
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

const OWNER_APP = "app_ci_rename";
const RENAME_MIGRATION = "rename_email";

const CREATE = `import { table, t } from "zero-migrate";
export const name = "create_people";
export default {
  schema() {
    table("people").create({
      columns: { id: t.int().notNull(), email: t.text() },
      primaryKey: ["id"],
    });
  },
};
`;

const SEED = `import { table } from "zero-migrate";
export const name = "seed_people";
export default {
  data() {
    table("people").insert({ rows: { id: 1, email: "ada@example.test" } });
  },
  inverse() {
    table("people").delete({ where: (col) => col("id").eq(1) });
  },
};
`;

const RENAME = `import { table, t } from "zero-migrate";
export const name = "${RENAME_MIGRATION}";
export default {
  schema() {
    table("people").column("email").rename({ to: "email_address", type: t.text() });
  },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "cirename-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

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
`,
  );
  writeFileSync(join(work, "registry.json"), JSON.stringify({ people: OWNER_APP }));
  writeFileSync(join(work, "migrations", "20260101000000_create_people.ts"), CREATE);
  writeFileSync(join(work, "migrations", "20260101000001_seed_people.ts"), SEED);
  return work;
}

function runCli(
  work: string,
  schema: string,
  argv: string[],
): Promise<{ code: number | null; out: string; err: string }> {
  return new Promise((resolvePromise) => {
    const child = spawn(
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
      { cwd: work, env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" } },
    );
    let err = "";
    let out = "";
    child.stderr.on("data", (chunk) => (err += chunk));
    child.stdout.on("data", (chunk) => (out += chunk));
    child.on("close", (code) =>
      resolvePromise({ code, out, err: err.replace(/^WARNING.*$/gm, "").trim() }),
    );
  });
}

test("the CI gate goes red for an open rename window and green again once it is committed", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("cirename");
  const work = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);

    // 1. A settled project. The gate must be green, or nothing below means
    //    anything - a gate that is always red is not a gate.
    const created = await runCli(work, schema, ["apply", "--approve"]);
    assert.equal(created.code, 0, `apply must succeed; ${created.err}`);
    const settled = await runCli(work, schema, ["status", "--strict", "--json"]);
    assert.equal(
      settled.code,
      0,
      `the gate must pass on a settled project; ${settled.out}${settled.err}`,
    );

    // 2. The window is open. The gate MUST fail: further changes to this table are
    //    refused anyway, and a pipeline that sailed past would ship code against a
    //    half-renamed column.
    writeFileSync(join(work, "migrations", "20260102000000_rename_email.ts"), RENAME);
    const expanded = await runCli(work, schema, ["apply", "--approve"]);
    assert.equal(expanded.code, 0, `the expand must apply; ${expanded.err}`);
    const duringWindow = await runCli(work, schema, ["status", "--strict", "--json"]);
    assert.notEqual(
      duringWindow.code,
      0,
      "an outstanding rename obligation must fail the CI gate",
    );

    // The premise for arm 3: the window really is open at the database.
    const { rows: open } = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'people' ORDER BY column_name`,
      [schema],
    );
    assert.deepEqual(
      open.map((row) => row.column_name),
      ["email", "email_address", "id"],
      "both columns coexist while the contract is outstanding",
    );

    // 3. THE ONE THAT MATTERS. Commit the contract and the gate must go green
    //    again. If it did not, a team's first online rename would leave every
    //    pipeline permanently red.
    const committed = await runCli(work, schema, [
      "resolve", RENAME_MIGRATION, "--commit", "--approve",
    ]);
    assert.equal(committed.code, 0, `the commit must succeed; ${committed.err}`);

    const after = await runCli(work, schema, ["status", "--strict", "--json"]);
    assert.equal(
      after.code,
      0,
      `the gate must go green once the contract is committed; ${after.out}${after.err}`,
    );

    // And the deploy that follows is an honest no-op rather than a refusal: the
    // rename migration is still a file on disk and every later deploy re-supplies
    // it.
    const redeploy = await runCli(work, schema, ["apply", "--approve"]);
    assert.equal(
      redeploy.code,
      0,
      `re-applying the settled directory must succeed; ${redeploy.err}`,
    );
    const { rows: final } = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'people' ORDER BY column_name`,
      [schema],
    );
    assert.deepEqual(
      final.map((row) => row.column_name),
      ["email_address", "id"],
      "and the re-deploy must not reopen the window it just closed",
    );
    const { rows: value } = await client.query(
      `SELECT email_address FROM "${schema}".people WHERE id = 1`,
    );
    assert.equal(value[0].email_address, "ada@example.test", "the value survived");
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
