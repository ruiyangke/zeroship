// The one pipeline recipe the documentation actually prints, run as written.
//
// docs/cli.md:
//
//   zero-migrate status --env production --strict --json > status.json || exit 1
//   jq -e '.busy | not' status.json    # fail this build if a deploy was running
//
// It only works because two separate behaviours agree, and neither is obvious:
//
//   1. `status --strict` exits 0 when a peer holds the project lock. Contention
//      is not a dirty migration set - a strict gate that failed there would fail
//      on every pipeline overlapping a deploy - so line 1 does NOT stop.
//   2. `busy` is present in the JSON whether or not anything is busy, so line 2
//      has something to test.
//
// If (1) changed, the recipe would exit at line 1 and never reach the busy check.
// If (2) changed, `jq -e '.busy | not'` on a missing field yields `true` and the
// gate would pass forever - silently stopping protecting, which is the direction
// that matters. Neither would fail any existing test.
//
// So this asserts the gate in BOTH directions: it must pass when idle and FAIL
// when a deploy is running. A gate only verified in the passing direction is not
// verified at all.
//
// The real `jq` runs when it is on PATH, so the documented command itself is
// exercised rather than a re-implementation of it. When it is absent the same
// predicate is evaluated in JavaScript, because the property belongs to the JSON
// rather than to jq.
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
const OWNER_APP = "app_documented_gate";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "cigate-"));
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
`,
  );
  writeFileSync(
    join(work, "migrations", "20260101000000_create.ts"),
    `import { table, t } from "zero-migrate";
export const name = "create_t1";
export default {
  schema() {
    table("t1").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`,
  );
  return work;
}

function run(work: string, schema: string, argv: string[]): { code: number | null; out: string; err: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, ...argv,
      "--dir", join(work, "migrations"),
      "--database-url", pgUrl(),
      "--policy", join(work, "policy.toml"),
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
    out: result.stdout ?? "",
    err: (result.stderr ?? "").replace(/^WARNING.*$/gm, "").trim(),
  };
}

/** `jq -e '.busy | not'` when jq is on PATH; the same predicate otherwise. */
function busyGatePasses(document: string): boolean {
  const jq = spawnSync("jq", ["-e", ".busy | not"], { input: document, encoding: "utf8" });
  if (jq.error === undefined && jq.status !== null) return jq.status === 0;
  return !(JSON.parse(document) as { busy: boolean }).busy;
}

test("the documented CI gate passes when idle and fails while a deploy runs", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;
  const holder = await connectLivePg(ctx);
  if (!holder) return;

  const schema = uniqueNamespace("cigate");
  const work = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    assert.equal(run(work, schema, ["apply", "--approve"]).code, 0, "the project must be clean");

    // Line 1 of the recipe, idle.
    const idle = run(work, schema, ["status", "--strict", "--json"]);
    assert.equal(idle.code, 0, `a clean, idle project must not stop the pipeline; ${idle.err}`);
    const idleDoc = JSON.parse(idle.out) as { busy: unknown };
    assert.equal(typeof idleDoc.busy, "boolean", "`busy` must be present even when nothing is busy");
    assert.equal(busyGatePasses(idle.out), true, "the gate must pass on an idle project");

    // Now a peer holds the project lock, which is what the gate exists to catch.
    await holder.query(`SELECT pg_advisory_lock(hashtext($1)::bigint)`, [schema]);
    try {
      const busy = run(work, schema, ["status", "--strict", "--json"]);
      // Load-bearing: if contention made `--strict` exit non-zero, the recipe
      // would stop at line 1 and the busy check would never run.
      assert.equal(
        busy.code,
        0,
        `contention must not fail --strict, or the recipe never reaches jq; ${busy.err}`,
      );
      const busyDoc = JSON.parse(busy.out) as { busy: unknown; lockHolders: unknown };
      assert.equal(busyDoc.busy, true, "`busy` must be true while a peer holds the lock");
      assert.ok(
        Array.isArray(busyDoc.lockHolders),
        "`lockHolders` must be an array, as the passage promises",
      );
      // The direction that matters: the gate must FAIL here.
      assert.equal(
        busyGatePasses(busy.out),
        false,
        "the documented gate must fail the build while a deploy is running",
      );
    } finally {
      await holder.query(`SELECT pg_advisory_unlock(hashtext($1)::bigint)`, [schema]);
    }
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
      )
      .catch(() => {});
    await holder.end().catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
