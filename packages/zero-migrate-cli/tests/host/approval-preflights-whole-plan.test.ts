// `README.md`: "Approval is checked across the complete plan before any authored
// step runs, so a later unapproved data change cannot follow an already-committed
// earlier step from that plan."
//
// That is an ORDERING guarantee, and it is load-bearing precisely because steps do
// NOT share a transaction. `partial-deploy-resumes.test.ts` established that a
// file's ops commit separately: op 0 can land while op 1 fails. So if approval were
// checked per step rather than per plan, a plan whose first step needs no approval
// and whose second step does would leave the first step COMMITTED and the second
// refused - a half-applied deploy that the operator never approved any part of.
//
// `approval-gate-scope.test.ts` pins WHICH operations the gate covers, in both
// directions. It says nothing about WHEN the check happens, and a per-step gate
// would satisfy every assertion in it.
//
// THE PLAN UNDER TEST is deliberately ordered against the guarantee:
//
//   step 1  createTable `fresh`   needs NO approval
//   step 2  delete from `seeded`  approval-gated
//
// Run without `--approve`, the README requires that `fresh` does not exist
// afterwards. Its absence is the whole assertion: a per-step gate creates it.
//
// THE APPROVED ARM IS NOT OPTIONAL. Without it, this file passes for a plan that
// fails for any reason at all - a bad policy, a typo, an unregistered table - and
// would keep passing if the approval gate were deleted and replaced by an
// unconditional refusal. The control proves the plan is otherwise valid and that
// approval is the only thing standing between it and the database.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`.

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

const OWNER_APP = "app_preflight";

const SEED = `import { table, t } from "zero-migrate";
export const name = "seed";
export default {
  up() {
    table("seeded").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
    table("seeded").insert({ rows: [{ id: 1 }, { id: 2 }] });
  },
};
`;

/** Approval-free step FIRST, approval-gated step SECOND. The order is the point. */
const MIXED = `import { table, t } from "zero-migrate";
export const name = "mixed";
export default {
  up() {
    table("fresh").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
    table("seeded").delete({ where: (c) => c("id").gt(0) });
  },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "preflight-"));
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
  writeFileSync(
    join(work, "registry.json"),
    JSON.stringify({ seeded: OWNER_APP, fresh: OWNER_APP }),
  );
  writeFileSync(join(work, "migrations", "20260101000000_seed.ts"), SEED);
  return work;
}

function apply(work: string, schema: string, approve: boolean) {
  const child = spawn(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "apply",
      ...(approve ? ["--approve"] : []),
      "--dir", join(work, "migrations"),
      "--database-url", pgUrl(),
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      "--schema", schema,
      "--owner-app", OWNER_APP,
    ],
    { cwd: work, env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" } },
  );
  let out = "";
  let err = "";
  child.stdout.on("data", (chunk) => (out += chunk));
  child.stderr.on("data", (chunk) => (err += chunk));
  return new Promise<{ code: number | null; text: string }>((done) =>
    child.on("close", (code) =>
      done({ code, text: `${out}\n${err}`.replace(/^WARNING.*$/gm, "").trim() }),
    ),
  );
}

test("an unapproved later step stops the whole plan, including its approval-free first step", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("preflight");
  const meta = `${schema}_migrations`;
  const work = project(schema);

  const tables = async (): Promise<string[]> => {
    const { rows } = await client.query(
      `SELECT table_name FROM information_schema.tables
        WHERE table_schema = $1 ORDER BY table_name`,
      [schema],
    );
    return rows.map((row) => row.table_name as string);
  };
  const seededRows = async (): Promise<number> => {
    const { rows } = await client.query(`SELECT count(*)::int AS n FROM "${schema}".seeded`);
    return rows[0].n as number;
  };

  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const seeded = await apply(work, schema, true);
    assert.equal(seeded.code, 0, `the seed migration must apply; ${seeded.text}`);
    assert.equal(await seededRows(), 2, "the seed rows must exist for the delete to be meaningful");

    writeFileSync(join(work, "migrations", "20260102000000_mixed.ts"), MIXED);

    // THE ASSERTION: no --approve, so nothing from this plan may reach the database.
    const refused = await apply(work, schema, false);
    assert.equal(refused.code, 1, `the unapproved plan must be refused; ${refused.text}`);

    assert.ok(
      !(await tables()).includes("fresh"),
      "`fresh` is the plan's FIRST step and needs no approval of its own. Its " +
        "presence would mean approval is checked per step rather than across the " +
        "plan, leaving a half-applied deploy the operator approved no part of",
    );
    assert.equal(
      await seededRows(),
      2,
      "the gated delete must not have run either",
    );

    // CONTROL: the same plan with approval applies both steps. Without this, the
    // test above passes for a plan that fails for any reason, and would keep
    // passing if the gate became an unconditional refusal.
    const approved = await apply(work, schema, true);
    assert.equal(approved.code, 0, `the approved plan must apply; ${approved.text}`);
    assert.ok(
      (await tables()).includes("fresh"),
      "with approval the first step must land - otherwise the refusal above was " +
        "not the approval gate",
    );
    assert.equal(
      await seededRows(),
      0,
      "with approval the gated delete must run too",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
