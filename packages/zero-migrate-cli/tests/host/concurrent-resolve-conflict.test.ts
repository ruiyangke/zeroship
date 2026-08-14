// Two operators resolving the same rename in OPPOSITE directions, at once.
//
// This is the highest-consequence race the tool has. Committing a PostgreSQL
// online rename drops the SOURCE column; aborting it drops the DESTINATION. Both
// succeeding means both columns are gone and the data with them — and unlike a
// failed apply, there is nothing left to roll forward from.
//
// The serial form is covered: `pg_scenarios.rs` proves a contract whose apply has
// started refuses to switch to abort (`PendingContractResolutionConflict`). The
// concurrent form was not. Those are different claims — the serial test hands the
// engine an already-settled state, while this hands it two processes that both
// read "outstanding" before either commits.
//
// F472 raced "the lock-contention behaviour of every verb" — whether each waits or
// refuses while the lock is held. That is not this. This is a pair with CONFLICTING
// INTENT, which the lock serialises but does not adjudicate: after the winner
// commits, the loser must be refused by the resolution-conflict gate rather than
// proceeding on the state it read before blocking.
//
// THE COLUMN COUNT IS THE ASSERTION. Exactly one of `email` / `email_address` must
// survive. Zero is the data-loss outcome this exists to catch; two would mean
// neither resolution took effect. Which one survives depends on who won, so the
// test asserts the COUNT and the agreement with the journal, not the identity.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`. PostgreSQL only — the online rename is
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

const OWNER_APP = "app_resolve_race";
const RENAME = "rename_email";

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

const RENAME_MIG = `import { table, t } from "zero-migrate";
export const name = "${RENAME}";
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
  const work = mkdtempSync(join(HERE, "resolverace-"));
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
  writeFileSync(join(work, "migrations", "20260102000000_rename_email.ts"), RENAME_MIG);
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

test("commit racing abort on the same rename leaves exactly one column", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("resolverace");
  const work = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);

    const applied = await start(work, schema, ["apply", "--approve"]);
    assert.equal(applied.code, 0, `the rename must open its window; ${applied.err}`);

    // The premise: both columns coexist, so both resolutions have something to
    // drop and the data-loss outcome is reachable.
    const { rows: open } = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'people'
          AND column_name IN ('email', 'email_address')`,
      [schema],
    );
    assert.equal(open.length, 2, "the window must be open before the race");

    const [commit, abort] = await Promise.all([
      start(work, schema, ["resolve", RENAME, "--commit", "--approve"]),
      start(work, schema, ["resolve", RENAME, "--rollback", "--approve"]),
    ]);

    // Exactly one may win. Both succeeding is the data-loss outcome.
    const winners = [commit, abort].filter((r) => r.code === 0).length;
    assert.equal(
      winners,
      1,
      `exactly one resolution may succeed, got ${winners}; ` +
        `commit=${commit.code} (${commit.err.slice(0, 160)}) ` +
        `abort=${abort.code} (${abort.err.slice(0, 160)})`,
    );

    // THE COLUMN COUNT. Zero means both dropped and the value is gone.
    const { rows: after } = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'people'
          AND column_name IN ('email', 'email_address')`,
      [schema],
    );
    assert.equal(
      after.length,
      1,
      `exactly one of email/email_address must survive, got [${after
        .map((r) => r.column_name)
        .join(",")}]`,
    );

    // And the row's value survived into whichever column won.
    const survivor = after[0].column_name as string;
    const { rows: value } = await client.query(
      `SELECT "${survivor}" AS v FROM "${schema}".people WHERE id = 1`,
    );
    assert.equal(
      value[0].v,
      "ada@example.test",
      `the value must survive into the surviving column ${survivor}`,
    );

    // The loser must say why, per the F470 standard, rather than failing blank.
    const loser = commit.code === 0 ? abort : commit;
    assert.ok(
      loser.err.length > 0,
      "the losing resolution must explain itself rather than failing silently",
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
