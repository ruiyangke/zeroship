// F654: rollback replays the reverse captured when the forward migration ran.
//
// These are deliberately CLI + live-PostgreSQL tests. In particular, the first
// two assertions read the journal column with SQL instead of trusting a host API
// that could substitute a freshly-derived reverse for a missing stored value.
//
// The compatibility arm temporarily disables the journal's immutability trigger
// solely to manufacture the state an upgraded installation already has: an
// applied event whose new nullable `down` column is NULL. The rollback itself is
// still exercised through the shipped CLI and with the trigger enabled.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { connectLivePg, pgUrl } from "./live-db.js";
import { noInjectPolicy } from "./policy.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_f654_stored_reverse";
const TABLE = "f654_notes";
const ACCOUNT_TABLE = "f654_accounts";
const UNIQUE_NAME = "f654_accounts_email_key";

type CliResult = ReturnType<typeof spawnSync>;

function ident(value: string): string {
  return `"${value.replaceAll('"', '""')}"`;
}

function uniqueSchema(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function scaffold(schema: string, name: string, body: string): string {
  const work = mkdtempSync(join(HERE, "rollback-stored-reverse-"));
  const migrations = join(work, "migrations");
  mkdirSync(migrations);
  writeFileSync(join(work, "policy.toml"), noInjectPolicy(schema));
  writeFileSync(
    join(work, "registry.json"),
    JSON.stringify({ [TABLE]: OWNER_APP, [ACCOUNT_TABLE]: OWNER_APP }),
  );
  writeFileSync(
    join(migrations, `20260101000000_${name}.ts`),
    `import { table, t } from "zero-migrate";
export const name = ${JSON.stringify(name)};
export default { schema() { ${body} } };
`,
  );
  return work;
}

function cli(work: string, schema: string, args: readonly string[]): CliResult {
  return spawnSync(
    process.execPath,
    [
      "--import",
      "tsx",
      CLI_BIN,
      ...args,
      "--dir",
      join(work, "migrations"),
      "--database-url",
      pgUrl(),
      "--schema",
      schema,
      "--policy",
      join(work, "policy.toml"),
      "--registry",
      join(work, "registry.json"),
      "--owner-app",
      OWNER_APP,
    ],
    {
      cwd: work,
      encoding: "utf8",
      env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    },
  );
}

function resultText(result: CliResult): string {
  return `${result.stdout ?? ""}\n${result.stderr ?? ""}`;
}

function assertCliOk(result: CliResult, action: string): void {
  assert.equal(result.status, 0, `${action} failed:\n${resultText(result)}`);
}

function createTableBody(): string {
  return `table(${JSON.stringify(TABLE)}).create({
    columns: { id: t.int().notNull(), title: t.text() },
    primaryKey: ["id"],
  });`;
}

async function withProject(
  t: Parameters<typeof connectLivePg>[0],
  prefix: string,
  body: (client: NonNullable<Awaited<ReturnType<typeof connectLivePg>>>, schema: string) => Promise<void>,
): Promise<void> {
  const client = await connectLivePg(t);
  if (!client) return;

  const schema = uniqueSchema(prefix);
  const meta = `${schema}_migrations`;
  try {
    await client.query(`CREATE SCHEMA ${ident(schema)}`);
    await body(client, schema);
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS ${ident(schema)} CASCADE;
         DROP SCHEMA IF EXISTS ${ident(meta)} CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
}

async function storedDown(
  client: NonNullable<Awaited<ReturnType<typeof connectLivePg>>>,
  schema: string,
): Promise<string | null> {
  const result = await client.query(
    `SELECT down
       FROM ${ident(`${schema}_migrations`)}.schema_migrations
      WHERE event_kind = 'applied'
      ORDER BY event_seq DESC
      LIMIT 1`,
  );
  assert.equal(result.rows.length, 1, "the direct journal query must find the applied row");
  return (result.rows[0] as { down: string | null }).down;
}

async function tableExists(
  client: NonNullable<Awaited<ReturnType<typeof connectLivePg>>>,
  schema: string,
  table: string,
): Promise<boolean> {
  const result = await client.query(
    `SELECT 1 FROM information_schema.tables WHERE table_schema = $1 AND table_name = $2`,
    [schema, table],
  );
  return result.rows.length === 1;
}

test("F654 a: apply stores the exact reverse in the journal", async (t) => {
  await withProject(t, "f654_store", async (client, schema) => {
    const work = scaffold(schema, "create_f654_notes", createTableBody());
    try {
      const applied = cli(work, schema, ["apply", "--approve"]);
      assertCliOk(applied, "apply");

      const down = await storedDown(client, schema);
      assert.equal(typeof down, "string", "a newly-applied reversible row stores its down SQL");
      assert.ok(down!.length > 0, "the stored down SQL is not an empty placeholder");
      assert.match(down!, /DROP TABLE/i);
      assert.match(down!, new RegExp(TABLE));
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  });
});

test("F654 b: a stored reverse is replayed, and says nothing about reconstructing", async (t) => {
  await withProject(t, "f654_replay", async (client, schema) => {
    // THE ARM THAT PROVES WHICH PATH RAN. Arms (a) and (c) show a reverse being
    // written and a legacy row being handled; this one shows that a row WITH a
    // stored reverse takes the replay branch rather than the derivation branch.
    //
    // The signal is the reconstruction advisory. Arm (c) requires a NULL row to
    // announce that its reverse was rebuilt; this arm requires a stored row to
    // stay silent. Neither assertion alone distinguishes the branches - together
    // they pin both directions, and a fix that always derived or always replayed
    // would fail one of them.
    //
    // TWO EARLIER VEHICLES FAILED, and both are worth recording rather than
    // quietly discarding:
    //   1. dropConstraint, on the theory that its UNIQUE definition vanishes
    //      after the forward runs so only apply could capture it. Wrong:
    //      lower_drop_constraint emits down: None, so a dropped constraint has NO
    //      synthesized reverse at all. It is irreversible today, and storing
    //      reverses does not change that.
    //   2. rewriting the journal's stored reverse to a distinguishable statement
    //      and watching which SQL ran. Refused by the engine: "migration journal
    //      is append-only (no UPDATE/DELETE)". That refusal is a property worth
    //      having, and this test measuring it was an accident worth keeping.
    const work = scaffold(
      schema,
      "create_f654_replay",
      `table(${JSON.stringify(TABLE)}).create({
         columns: { id: t.int().notNull() },
         primaryKey: ["id"],
       });`,
    );
    try {
      assertCliOk(cli(work, schema, ["apply", "--approve"]), "apply");
      const down = await storedDown(client, schema);
      assert.equal(typeof down, "string", "the forward migration stored a reverse");
      assert.match(down!, /DROP TABLE/i, "and it is the reverse of what ran");

      const rolledBack = cli(work, schema, ["rollback", "--steps", "1", "--approve"]);
      assertCliOk(rolledBack, "stored-reverse rollback");
      assert.equal(await tableExists(client, schema, TABLE), false, "the reverse ran");

      assert.doesNotMatch(
        resultText(rolledBack),
        /reconstruct(?:ed|ing)[^\n]*reverse|reverse[^\n]*reconstruct(?:ed|ing)/i,
        `a row that STORED its reverse must replay it silently. Announcing a ` +
          `reconstruction here would mean the derivation branch ran and the ` +
          `stored SQL was ignored, which is the defect this change closes`,
      );
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  });
});

test("F654 c: a legacy NULL reverse is reconstructed with a visible advisory", async (t) => {
  await withProject(t, "f654_legacy", async (client, schema) => {
    const work = scaffold(schema, "create_f654_notes", createTableBody());
    const journal = `${ident(`${schema}_migrations`)}.schema_migrations`;
    try {
      const applied = cli(work, schema, ["apply", "--approve"]);
      assertCliOk(applied, "apply before legacy-row simulation");

      // New installations cannot naturally create NULL here. Disable only user
      // triggers around this one fixture update to model an applied event written
      // by the pre-column journal shape; re-enable them before rollback starts.
      await client.query(`ALTER TABLE ${journal} DISABLE TRIGGER USER`);
      try {
        await client.query(
          `UPDATE ${journal} SET down = NULL WHERE event_kind = 'applied'`,
        );
      } finally {
        await client.query(`ALTER TABLE ${journal} ENABLE TRIGGER USER`);
      }
      assert.equal(await storedDown(client, schema), null, "the fixture is a legacy NULL row");

      const rolledBack = cli(work, schema, ["rollback", "--steps", "1", "--approve"]);
      assertCliOk(rolledBack, "legacy fallback rollback");
      assert.equal(await tableExists(client, schema, TABLE), false, "the reconstructed down ran");
      assert.match(
        resultText(rolledBack),
        /reconstruct(?:ed|ing)[^\n]*reverse|reverse[^\n]*reconstruct(?:ed|ing)/i,
        "the operator is told that this pre-upgrade reverse was reconstructed",
      );
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  });
});

test("F654 d CONTROL: ordinary DDL rollback appends an event and remains re-applicable", async (t) => {
  await withProject(t, "f654_control", async (client, schema) => {
    const work = scaffold(schema, "create_f654_notes", createTableBody());
    try {
      const applied = cli(work, schema, ["apply", "--approve"]);
      assertCliOk(applied, "initial apply");
      assert.equal(await tableExists(client, schema, TABLE), true, "apply creates the table");

      const rolledBack = cli(work, schema, ["rollback", "--steps", "1", "--approve"]);
      assertCliOk(rolledBack, "ordinary rollback");
      assert.equal(await tableExists(client, schema, TABLE), false, "rollback removes the table");

      const events = await client.query(
        `SELECT count(*)::int AS n
           FROM ${ident(`${schema}_migrations`)}.schema_migrations
          WHERE event_kind = 'rolled_back'`,
      );
      assert.equal(
        Number((events.rows[0] as { n: number }).n),
        1,
        "rollback appends one rolled_back journal event",
      );

      const reapplied = cli(work, schema, ["apply", "--approve"]);
      assertCliOk(reapplied, "re-apply after rollback");
      assert.equal(await tableExists(client, schema, TABLE), true, "pending migration re-applies");
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  });
});
