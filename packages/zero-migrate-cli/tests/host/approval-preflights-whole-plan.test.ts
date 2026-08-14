// `README.md`: "Approval is checked across the complete plan before any authored
// step runs, so a later unapproved data change cannot follow an already-committed
// earlier step from that plan."
//
// Under the schema/data protocol, each migration file is its own plan: the schema
// half can commit before the later data migration is considered. The ordering
// guarantee remains load-bearing WITHIN that data plan because its operations do
// not share a transaction. If approval were checked per operation, op 0 could land
// before op 1 was refused.
//
// `approval-gate-scope.test.ts` pins WHICH operations the gate covers, in both
// directions. It says nothing about WHEN the check happens, and a per-step gate
// would satisfy every assertion in it.
//
// THE DATA PLAN UNDER TEST is deliberately ordered against the guarantee:
//
//   step 1  insert into `fresh`   needs NO approval
//   step 2  delete from `seeded`  approval-gated
//
// A separate preceding schema migration creates `fresh`; that is the intentional
// protocol boundary and it remains after refusal. Run without `--approve`, the
// README requires that `fresh` contain no row afterwards. Its emptiness is the
// assertion: a per-operation gate inserts the sentinel before refusing the delete.
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

const SEED_SCHEMA = `import { table, t } from "zero-migrate";
export const name = "create_seeded";
export default {
  schema() {
    table("seeded").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`;

const SEED_DATA = `import { table } from "zero-migrate";
export const name = "seed";
export default {
  data() {
    table("seeded").insert({ rows: [{ id: 1 }, { id: 2 }] });
  },
  inverse() {
    table("seeded").delete({ where: (col) => col("id").in([1, 2]) });
  },
};
`;

/** Approval-free step FIRST, approval-gated step SECOND. The order is the point. */
const FRESH_SCHEMA = `import { table, t } from "zero-migrate";
export const name = "create_fresh";
export default {
  schema() {
    table("fresh").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`;

const DELETE_SEEDED = `import { table } from "zero-migrate";
export const name = "delete_seeded";
export default {
  data() {
    table("fresh").insert({ rows: [{ id: 1 }] });
    table("seeded").delete({ where: (col) => col("id").in([1, 2]) });
  },
  inverse() {
    table("seeded").insert({ rows: [{ id: 1 }, { id: 2 }] });
    table("fresh").delete({ where: (col) => col("id").eq(1) });
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
  writeFileSync(join(work, "migrations", "20260101000000_create_seeded.ts"), SEED_SCHEMA);
  writeFileSync(join(work, "migrations", "20260101000001_seed.ts"), SEED_DATA);
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

test("an unapproved later operation stops the whole data plan, including its approval-free first operation", async (ctx) => {
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
  const freshRows = async (): Promise<number> => {
    const { rows } = await client.query(`SELECT count(*)::int AS n FROM "${schema}".fresh`);
    return rows[0].n as number;
  };

  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const seeded = await apply(work, schema, true);
    assert.equal(seeded.code, 0, `the seed migration must apply; ${seeded.text}`);
    assert.equal(await seededRows(), 2, "the seed rows must exist for the delete to be meaningful");

    writeFileSync(join(work, "migrations", "20260102000000_create_fresh.ts"), FRESH_SCHEMA);
    writeFileSync(join(work, "migrations", "20260102000001_delete_seeded.ts"), DELETE_SEEDED);

    // No --approve: the separate schema migration may land, but no operation from
    // the following data migration may reach the database.
    const refused = await apply(work, schema, false);
    assert.equal(refused.code, 1, `the unapproved plan must be refused; ${refused.text}`);

    assert.ok(
      (await tables()).includes("fresh"),
      "the preceding schema migration is a separate plan and must remain applied",
    );
    assert.equal(
      await freshRows(),
      0,
      "the insert is the data plan's FIRST operation and needs no approval of its " +
        "own. A row here would mean approval is checked per operation rather than " +
        "across the data plan",
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
      "with approval the separately applied schema must remain",
    );
    assert.equal(
      await freshRows(),
      1,
      "with approval the data plan's first operation must land - otherwise the " +
        "refusal above was not the approval gate",
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
