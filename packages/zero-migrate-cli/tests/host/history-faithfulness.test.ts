// `history` against the journal it reports, and the two boundaries around it.
//
// The journal is append-only: a rollback does not delete the `applied` row, it
// appends a `rolled_back` one, and a re-apply appends a further `applied` row for
// the same version. So the same migration can hold three events, and the ORDER of
// those events is the only thing that says what its current state is. A history
// that dropped, reordered, or collapsed them would still look plausible - one row
// per migration reads like a clean report rather than a bug.
//
// This asserts history against the RAW TABLE rather than against a hand-written
// expectation. A hand-written list would have to be kept in step with whatever
// the engine happens to journal, and would quietly stop testing faithfulness the
// first time it drifted. Reading both sides means the test says "these agree",
// which is the actual property.
//
// TWO DOCUMENTED BOUNDARIES ride along, both easy to get wrong in the direction
// that looks fine:
//
//   - `history` is PostgreSQL-only (`docs/cli.md`). A SQLite or MySQL target must
//     be refused by name, not silently return an empty stream, which is what an
//     operator would read as "nothing has been deployed".
//
//   - `status` and `history` are NOT strictly read-only. Both bootstrap the
//     journal, so a first call against a project that has never deployed CREATES
//     the meta schema. `docs/operations.md` and `docs/node-api.md` both say so,
//     and it is worth pinning because it decides whether these verbs can be run
//     by a role without DDL rights. `plan` and `lint` create nothing, which is
//     the contrast that makes the statement meaningful rather than a blanket
//     "everything touches the database".
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`; the MySQL arm additionally needs
// `ZERO_MIGRATE_MYSQL_URL`. SQLite always runs.

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

const OWNER_APP = "app_history_faithfulness";
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;

const migrationSource = (name: string, table: string): string =>
  `import { table, t } from "zero-migrate";
export const name = ${JSON.stringify(name)};
export default {
  schema() {
    table(${JSON.stringify(table)}).create({
      columns: { id: t.int().notNull() },
      primaryKey: ["id"],
    });
  },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** A project with two migrations, and destructive ops allowed so rollback can run. */
function project(scope: string, files: Array<[string, string]>): string {
  const work = mkdtempSync(join(HERE, "history-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = [${JSON.stringify(scope)}] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = [${JSON.stringify(scope)}] }

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`,
  );
  for (const [fileName, body] of files) {
    writeFileSync(join(work, "migrations", fileName), body);
  }
  return work;
}

const TWO: Array<[string, string]> = [
  ["20260101000000_m1.ts", migrationSource("m1", "t1")],
  ["20260102000000_m2.ts", migrationSource("m2", "t2")],
];

interface Outcome {
  readonly code: number | null;
  readonly out: string;
  readonly err: string;
}

function runCli(work: string, argv: string[], extra: string[] = []): Promise<Outcome> {
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, ...argv,
        "--dir", join(work, "migrations"),
        "--policy", join(work, "policy.toml"),
        "--owner-app", OWNER_APP,
        ...extra,
      ],
      { cwd: work, env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" } },
    );
    let out = "";
    let err = "";
    child.stdout.on("data", (chunk) => (out += chunk));
    child.stderr.on("data", (chunk) => (err += chunk));
    child.on("close", (code) =>
      resolvePromise({ code, out: out.trim(), err: err.replace(/^WARNING.*$/gm, "").trim() }),
    );
  });
}

const pgArgs = (schema: string): string[] => [
  "--database-url", pgUrl(),
  "--schema", schema,
];

interface HistoryEvent {
  readonly eventSeq: string;
  readonly version: string;
  readonly name: string;
  readonly kind: string;
}

test("history reports every journal event, in order, across a rollback and a re-apply", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("histfaith");
  const meta = `${schema}_migrations`;
  const work = project(schema, TWO);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);

    // Three deploys, so one version accumulates applied -> rolled_back -> applied.
    for (const argv of [
      ["apply", "--approve"],
      ["rollback", "--steps", "1", "--approve"],
      ["apply", "--approve"],
    ]) {
      const result = await runCli(work, argv, pgArgs(schema));
      assert.equal(result.code, 0, `${argv[0]} must succeed; ${result.err}`);
    }

    const { rows: raw } = await client.query(
      `SELECT event_seq::text AS seq, version, name, event_kind
         FROM "${meta}".schema_migrations ORDER BY event_seq`,
    );
    // The premise: without a rollback in the middle there is nothing to be
    // faithful ABOUT, and a one-row-per-migration history would pass.
    assert.equal(raw.length, 4, "the journal must hold four events for this sequence");
    assert.deepEqual(
      raw.map((row) => row.event_kind),
      ["applied", "applied", "rolled_back", "applied"],
      "the journal is append-only: the rollback appends rather than deleting",
    );

    const reported = await runCli(work, ["history", "--json"], pgArgs(schema));
    assert.equal(reported.code, 0, `history must succeed; ${reported.err}`);
    const events = (JSON.parse(reported.out) as { events: HistoryEvent[] }).events;

    // Compared field by field against the table, so this cannot drift into
    // asserting a hand-written list that stopped matching the engine.
    assert.deepEqual(
      events.map((event) => [event.eventSeq, event.version, event.name, event.kind]),
      raw.map((row) => [row.seq, row.version, row.name, row.event_kind]),
      "history must report the journal exactly: same events, same order, same kinds",
    );

    // The human rendering carries the rollback too - a reader of the default
    // output must not have to ask for `--json` to see that something came back.
    const human = await runCli(work, ["history"], pgArgs(schema));
    assert.equal(human.code, 0, `history must succeed; ${human.err}`);
    assert.match(human.out, /rolled_back/, "the human stream must show the rollback event");
    assert.equal(
      human.out.split("\n").filter((line) => line.trim()).length,
      4,
      "one line per journal event",
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

test("history refuses a SQLite target by name rather than reporting an empty stream", async () => {
  const work = project("main", TWO);
  try {
    const dbPath = join(work, "app.db");
    const applied = await runCli(work, ["apply", "--approve"], [
      "--database-url", `sqlite:${dbPath}`,
      "--schema", "main",
    ]);
    // The control: SQLite really did deploy, so an empty history would be a lie
    // rather than an accurate report of an untouched database.
    assert.equal(applied.code, 0, `the SQLite apply must succeed; ${applied.err}`);

    const reported = await runCli(work, ["history"], [
      "--database-url", `sqlite:${dbPath}`,
      "--schema", "main",
    ]);
    assert.equal(reported.code, 1, "history must fail on SQLite");
    assert.match(
      reported.err,
      /history supports only PostgreSQL/,
      `the refusal must name the dialect boundary; got: ${reported.err}`,
    );
    assert.equal(reported.out, "", "a refused history must print no event stream");
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("history refuses a MySQL target by name rather than reporting an empty stream", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL history boundary skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const database = uniqueNamespace("histmy");
  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  const work = project(database, TWO);
  try {
    await admin.query(`CREATE DATABASE \`${database}\``);
    const applied = await runCli(work, ["apply", "--approve"], [
      "--database-url", MYSQL_URL,
      "--schema", database,
    ]);
    assert.equal(applied.code, 0, `the MySQL apply must succeed; ${applied.err}`);

    const reported = await runCli(work, ["history"], [
      "--database-url", MYSQL_URL,
      "--schema", database,
    ]);
    assert.equal(reported.code, 1, "history must fail on MySQL");
    assert.match(
      reported.err,
      /history supports only PostgreSQL/,
      `the refusal must name the dialect boundary; got: ${reported.err}`,
    );
    assert.equal(reported.out, "", "a refused history must print no event stream");
  } finally {
    await admin
      .query(
        `DROP DATABASE IF EXISTS \`${database}\`; DROP DATABASE IF EXISTS \`${database}_migrations\``,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});

test("status and history bootstrap the journal on a fresh project; plan and lint do not", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  // `docs/operations.md`: "do not assume the first call is physically
  // read-only". This pins which verbs that covers, because it decides whether
  // they can be run by a role without DDL rights.
  const bootstraps = async (argv: string[]): Promise<number> => {
    const schema = uniqueNamespace("histboot");
    const meta = `${schema}_migrations`;
    const work = project(schema, TWO);
    try {
      await client.query(`CREATE SCHEMA "${schema}"`);
      const { rows: before } = await client.query(
        `SELECT count(*)::int AS n FROM information_schema.schemata WHERE schema_name = $1`,
        [meta],
      );
      assert.equal(before[0].n, 0, "the meta schema must not exist before the verb runs");

      const result = await runCli(work, argv, pgArgs(schema));
      assert.equal(result.code, 0, `${argv[0]} must succeed; ${result.err}`);

      const { rows: after } = await client.query(
        `SELECT count(*)::int AS n FROM information_schema.tables WHERE table_schema = $1`,
        [meta],
      );
      return after[0].n as number;
    } finally {
      await client
        .query(
          `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
           DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
        )
        .catch(() => {});
      rmSync(work, { recursive: true, force: true });
    }
  };

  try {
    assert.ok(
      (await bootstraps(["status"])) > 0,
      "status creates the journal on a project that has never deployed",
    );
    assert.ok(
      (await bootstraps(["history"])) > 0,
      "history creates the journal on a project that has never deployed",
    );
    // The contrast. Without these two the statement above would be a blanket
    // "the CLI touches the database", which says nothing about which verbs an
    // operator can hand to a role without DDL rights.
    assert.equal(
      await bootstraps(["plan"]),
      0,
      "plan must create nothing",
    );
    assert.equal(
      await bootstraps(["lint", "--dialect", "postgres"]),
      0,
      "lint must create nothing",
    );
  } finally {
    await client.end().catch(() => {});
  }
});
