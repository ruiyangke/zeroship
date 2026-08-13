// The one concurrency pair the sweep never raced: `apply` against `rollback`.
//
// F470 raced `apply || apply`. F471 raced `rollback || rollback` and
// `rollback || status`. F472 raced two applies of DIFFERENT sets and the
// lock-contention behaviour of every verb. Nothing raced a process that ADDS
// against a process that REMOVES, which is the pair where the two racers want the
// journal to move in opposite directions.
//
// Both take the project lock, so they serialise — the question is whether the
// SERIALISED OUTCOME IS COHERENT, not whether they overlap. Two orderings are
// legal and they end in different states:
//
//   apply first     b applies, then `rollback --steps 1` unwinds the newest
//                   applied version, which is now b
//   rollback first  a is unwound, then apply finds BOTH a and b pending and
//                   re-applies them
//
// So this cannot assert one final schema. It asserts the INVARIANTS that hold
// under either ordering, which is what a concurrency test is for:
//
//   * every version appears in the journal a coherent number of times — an
//     `applied` count that exceeds its `rolled_back` count by at most one;
//   * the live tables agree with the journal's net state, so nothing was created
//     without a record or dropped while the record still claims it applied;
//   * neither process reports success while having left the other's object
//     half-removed.
//
// BOTH ORDERINGS WERE OBSERVED while writing this, over repeated runs:
//
//   apply first     journal and schema both carry alpha and beta
//   rollback first  alpha is unwound, and apply then REFUSES with
//                   `authored prior migration "create_alpha" (mig_...) is not
//                   fully applied (state: pending)` - the require_applied_prefix
//                   gate, naming the migration, its version and its state
//
// so the refusal arm below is not hypothetical. Journal and live schema agreed in
// every observed run under both orderings.
//
// BOTH ORDERINGS WERE OBSERVED while writing this, over repeated runs:
//
//   apply first     journal and schema both carry alpha and beta
//   rollback first  alpha is unwound, and apply then REFUSES with
//                   `authored prior migration "create_alpha" (mig_...) is not
//                   fully applied (state: pending)` — the require_applied_prefix
//                   gate, naming the migration, its version and its state
//
// so the refusal arm below is not hypothetical. Journal and live schema agreed in
// every observed run, under both orderings.
//
// The failure this is shaped to catch is a journal that records a rollback whose
// DDL the winner had already undone, or an apply that re-creates a table the
// journal still calls rolled back — either of which makes the next deploy's
// pending computation wrong.
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

const OWNER_APP = "app_race";

const FIRST = `import { table, t } from "zero-migrate";
export const name = "create_alpha";
export default {
  up() { table("alpha").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] }); },
};
`;

const SECOND = `import { table, t } from "zero-migrate";
export const name = "create_beta";
export default {
  up() { table("beta").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] }); },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "raceroll-"));
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
  writeFileSync(join(work, "registry.json"), JSON.stringify({}));
  writeFileSync(join(work, "migrations", "20260101000000_create_alpha.ts"), FIRST);
  return work;
}

function start(work: string, schema: string, argv: string[]) {
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
  child.stderr.on("data", (chunk) => (err += chunk));
  return new Promise<{ code: number | null; err: string }>((done) =>
    child.on("close", (code) =>
      done({ code, err: err.replace(/^WARNING.*$/gm, "").trim() }),
    ),
  );
}

test("apply racing rollback leaves a journal and a schema that agree", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("raceroll");
  const meta = `${schema}_migrations`;
  const work = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);

    // `alpha` is applied and reversible, so a rollback has something real to undo.
    const seeded = await start(work, schema, ["apply", "--approve"]);
    assert.equal(seeded.code, 0, `the first migration must apply; ${seeded.err}`);

    // Now the racers want opposite things: one adds `beta`, one unwinds the
    // newest applied version.
    writeFileSync(join(work, "migrations", "20260102000000_create_beta.ts"), SECOND);
    const [applied, rolled] = await Promise.all([
      start(work, schema, ["apply", "--approve"]),
      start(work, schema, ["rollback", "--steps", "1", "--approve"]),
    ]);

    // Neither may crash with an unhandled error. A refusal is legal - one racer
    // can legitimately find nothing to do - but it must be a clean exit path.
    for (const [label, r] of [["apply", applied], ["rollback", rolled]] as const) {
      assert.ok(
        r.code === 0 || r.code === 1,
        `${label} must exit cleanly, got ${r.code}: ${r.err}`,
      );
      assert.doesNotMatch(
        r.err,
        /panic|unwrap|RUST_BACKTRACE/i,
        `${label} must not surface a panic: ${r.err}`,
      );
      // F470 set the standard: a refusal an operator cannot act on is a defect
      // even when the exit code is clean. The one refusal this race produces is
      // apply losing to rollback, and it must name what it is waiting on.
      if (r.code !== 0) {
        assert.match(
          r.err,
          /not fully applied|pending|create_alpha/,
          `${label} refused without naming a cause an operator can act on: ${r.err}`,
        );
      }
    }

    // INVARIANT 1: the journal is coherent. Each version's `applied` count may
    // exceed its `rolled_back` count by at most one, and never trail it.
    const { rows: events } = await client.query(
      `SELECT version, event_kind, count(*)::int AS n
         FROM "${meta}".schema_migrations
        WHERE phase IS NULL OR phase = 'completed'
        GROUP BY version, event_kind`,
      [],
    );
    const net = new Map<string, number>();
    for (const row of events) {
      const delta = row.event_kind === "applied" ? row.n : -row.n;
      net.set(row.version, (net.get(row.version) ?? 0) + delta);
    }
    for (const [version, value] of net) {
      assert.ok(
        value === 0 || value === 1,
        `version ${version} has a net journal state of ${value}; ` +
          `only 0 (rolled back) or 1 (applied) is coherent`,
      );
    }

    // INVARIANT 2: the live schema agrees with that net state. A table present
    // with a net-zero version, or absent with a net-one version, is the
    // divergence this pair could produce.
    const { rows: live } = await client.query(
      `SELECT table_name FROM information_schema.tables
        WHERE table_schema = $1 ORDER BY table_name`,
      [schema],
    );
    const present = new Set(live.map((row) => row.table_name as string));

    const { rows: named } = await client.query(
      `SELECT version, name FROM "${meta}".schema_migrations
        WHERE event_kind = 'applied' AND phase = 'completed'`,
      [],
    );
    const tableFor = new Map<string, string>();
    for (const row of named) {
      if (String(row.name).includes("alpha")) tableFor.set(row.version, "alpha");
      if (String(row.name).includes("beta")) tableFor.set(row.version, "beta");
    }

    for (const [version, table] of tableFor) {
      const applied1 = (net.get(version) ?? 0) === 1;
      assert.equal(
        present.has(table),
        applied1,
        `journal says ${table} is ${applied1 ? "applied" : "rolled back"}, ` +
          `but the table is ${present.has(table) ? "present" : "absent"}`,
      );
    }

    // Non-vacuity: the race must have left SOMETHING to check, or the invariants
    // above are statements about an empty set.
    assert.ok(tableFor.size > 0, "the race must leave at least one journaled version");
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
