// The remediation an operator is handed when a pending rename blocks a deploy
// has to be a command that exists.
//
// Touching a table that still has an outstanding online-rename contract is
// fail-closed refused, and the refusal ends by naming the command that unblocks
// it. That command was `migrate resolve-pending --apply <version>`, of which
// every part was wrong: there is no `migrate` binary, `resolve-pending` is a
// verb this project REMOVED - `cli.test.ts` has a test called "removed CLI verbs
// are unknown and absent from help" asserting exactly that - and the CLI's
// `resolve` takes an authored migration NAME, not a version, which `docs/cli.md`
// calls out specifically.
//
// So the operator most in need of a way forward, at the one moment the tool has
// deliberately stopped them, was told to run something that does not exist.
//
// THIS TEST EXECUTES THE SUGGESTION RATHER THAN SPELL-CHECKING IT. It lifts the
// command out of the refusal text, substitutes the migration name for the
// placeholder, runs it, and requires the rename to actually complete. A test that
// only matched the new string would pass just as happily on the next plausible-
// looking wrong command, which is how the original defect survived: the string
// was pinned by a unit test, and the unit test pinned it to the wrong value.
//
// The CONTROL runs the command the refusal used to name and requires it to be
// rejected as unknown. Without it, the assertion that the message no longer says
// `resolve-pending` would also pass in a world where every verb was accepted.
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

const OWNER_APP = "app_pending_remediation";
const RENAME_MIGRATION = "rename_display_name";

const CREATE = `import { table, t } from "zero-migrate";
export const name = "create_users";
export default {
  schema() {
    table("users").create({
      columns: { id: t.int().notNull(), display_name: t.text() },
      primaryKey: ["id"],
    });
  },
};
`;

const RENAME = `import { table, t } from "zero-migrate";
export const name = "${RENAME_MIGRATION}";
export default {
  schema() {
    table("users").column("display_name").rename({ to: "full_name", type: t.text() });
  },
};
`;

/** A later deploy touching the same table, which the pending contract must block. */
const TOUCH = `import { table, t } from "zero-migrate";
export const name = "add_nickname";
export default {
  schema() {
    table("users").column("nickname").add({ type: t.text() });
  },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "pendrem-"));
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
  writeFileSync(join(work, "registry.json"), JSON.stringify({ users: OWNER_APP }));
  writeFileSync(join(work, "migrations", "20260101000000_create_users.ts"), CREATE);
  writeFileSync(join(work, "migrations", "20260102000000_rename_display_name.ts"), RENAME);
  return work;
}

interface Outcome {
  readonly code: number | null;
  readonly out: string;
  readonly err: string;
}

function runCli(work: string, schema: string, argv: string[]): Promise<Outcome> {
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
    let out = "";
    let err = "";
    child.stdout.on("data", (chunk) => (out += chunk));
    child.stderr.on("data", (chunk) => (err += chunk));
    child.on("close", (code) =>
      resolvePromise({ code, out, err: err.replace(/^WARNING.*$/gm, "").trim() }),
    );
  });
}

/** The CLI with no flags at all, for questions about the verb itself. */
function runBare(work: string, argv: string[]): Promise<Outcome> {
  return new Promise((resolvePromise) => {
    const child = spawn(process.execPath, ["--import", "tsx", CLI_BIN, ...argv], {
      cwd: work,
      env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    });
    let out = "";
    let err = "";
    child.stdout.on("data", (chunk) => (out += chunk));
    child.stderr.on("data", (chunk) => (err += chunk));
    child.on("close", (code) =>
      resolvePromise({ code, out, err: err.replace(/^WARNING.*$/gm, "").trim() }),
    );
  });
}

test("the pending-contract refusal names a command that exists and actually works", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("pendrem");
  const work = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const applied = await runCli(work, schema, ["apply", "--approve"]);
    assert.equal(applied.code, 0, `apply must open the rename window; ${applied.err}`);

    // A later deploy touches the same table while the contract is outstanding.
    writeFileSync(join(work, "migrations", "20260103000000_add_nickname.ts"), TOUCH);
    const blocked = await runCli(work, schema, ["apply", "--approve"]);
    assert.equal(blocked.code, 1, `the pending contract must block this deploy; ${blocked.err}`);
    assert.match(blocked.err, /in-flight online rename/, "the refusal must name the cause");

    // The removed verb must not be what the operator is sent to.
    assert.doesNotMatch(
      blocked.err,
      /resolve-pending/,
      "the refusal must not name a verb this project removed",
    );

    // Lift the suggested command out of the message and run it for real. The
    // placeholder is the only thing the operator has to supply themselves.
    const suggested = /run `([^`]+)`/.exec(blocked.err)?.[1];
    assert.ok(suggested, `the refusal must suggest a command; got: ${blocked.err}`);
    assert.match(
      suggested,
      /^zero-migrate /,
      `the suggestion must invoke this CLI; got: ${suggested}`,
    );
    assert.match(
      suggested,
      /<migration>/,
      `the suggestion must show where the migration name goes; got: ${suggested}`,
    );

    const argv = suggested
      .replace(/^zero-migrate /, "")
      .split(/\s+/)
      .map((token) => (token === "<migration>" ? RENAME_MIGRATION : token));
    const remedied = await runCli(work, schema, argv);
    assert.equal(
      remedied.code,
      0,
      `the suggested command must actually work; ran ${JSON.stringify(argv)}: ${remedied.err}`,
    );

    // It must have done the thing it claimed: the rename is closed.
    const { rows } = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'users' ORDER BY column_name`,
      [schema],
    );
    assert.deepEqual(
      rows.map((row) => row.column_name),
      ["full_name", "id"],
      "the suggested command must have closed the rename window",
    );

    // And the deploy it was blocking now goes through.
    const unblocked = await runCli(work, schema, ["apply", "--approve"]);
    assert.equal(unblocked.code, 0, `the blocked deploy must proceed afterwards; ${unblocked.err}`);
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

test("the verb the refusal used to name is still rejected, so the fix was not cosmetic", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("pendctl");
  const work = project(schema);
  try {
    // The control. If this verb were somehow accepted, the assertion above that
    // the message no longer mentions it would prove nothing about whether the
    // operator could have followed the old advice.
    //
    // The old suggestion is unrunnable twice over, and the CLI reports whichever
    // it reaches first: `--apply` is not a flag it has, and `resolve-pending` is
    // not a verb it has. Both are asserted, because "unrunnable" is the property
    // that matters and pinning only one of them would let the other quietly
    // become valid.
    const oldSuggestion = await runCli(work, schema, [
      "resolve-pending", "--apply", "mig_whatever",
    ]);
    assert.equal(oldSuggestion.code, 1, "the old suggestion must still be rejected");
    assert.match(
      oldSuggestion.err,
      /unknown command|unknown flag/,
      `the old suggestion must be unrunnable; got: ${oldSuggestion.err}`,
    );

    // Bare, with none of the usual flags: `--registry` is validated against the
    // verb list before the verb itself is, so sending it here would measure flag
    // validation and never reach the question being asked.
    const removedVerb = await runBare(work, ["resolve-pending"]);
    assert.equal(removedVerb.code, 1, "the removed verb must still be rejected");
    assert.match(
      removedVerb.err,
      /unknown command .*resolve-pending/,
      `resolve-pending must remain a removed verb; got: ${removedVerb.err}`,
    );
  } finally {
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
