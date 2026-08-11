import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import type { Client } from "pg";
import type { StatusReply } from "../../src/addon.js";
import {
  driverFor,
  formatStatusBusy,
  formatStatusJson,
  formatStatusHuman,
  hasInlinePassword,
  pendingMigrationsForPlan,
  resolveNetworkSecurity,
  resolvePendingVersion,
  rollbackTargetFromArgs,
  formatRollbackHuman,
  statusExitCode,
  statusIsDirty,
} from "../../src/cli.js";
import {
  loadZeroMigrateConfig,
  resolveCliConfig,
} from "../../src/config.js";
import { connectLivePg, liveDbRequired, pgUrl, REQUIRE_LIVE_DB_ENV } from "./live-db.js";
import { noInjectPolicy } from "./policy.js";
import {
  currentIrVersion,
  previewSql,
  type IrEnvelope,
} from "../../src/index.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);
/**
 * A charter that INJECTS NOTHING, for the arms that only need author-shaped output.
 *
 * NOT interchangeable with `noInjectPolicy(schema)` imported above, despite the names.
 * That one grants `schema.cross_schema` over a LITERAL schema, so the migration OWNS
 * something and the guard's confinement scope resolves to it. This one grants nothing,
 * so it owns nothing and `GuardConfig::schema_scope` collapses to `Single("")` - which
 * permits no schema at all.
 *
 * Correct where it is used because those arms never apply against a live project
 * schema: they preview or lint, where the charter shapes INJECTION and confinement is
 * never consulted. Swapping in `noInjectPolicy` would silently change what they assert,
 * and swapping this into an apply arm would deny every create. The distinction is
 * inject-shape versus ownership, and only the names make them look alike.
 */
const NO_INJECT_POLICY = "policy_version = 1\n";
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;

const CONFIG_ENV_KEYS = [
  "DATABASE_URL",
  "ZERO_MIGRATE_URL",
  "ZERO_MIGRATE_DIR",
  "ZERO_MIGRATE_OWNER_APP",
  "ZERO_MIGRATE_SCHEMA",
  "ZERO_MIGRATE_REGISTRY",
  "ZERO_MIGRATE_POLICY",
  "ZERO_MIGRATE_CONFIG",
  "ZERO_MIGRATE_ENV",
  // Cleared so the arms that assert an empty stderr stay hermetic: an operator who
  // exports the diagnostics switch in their own shell must not turn it on for a
  // suite that asserts what an unconfigured run prints.
  "ZERO_MIGRATE_LOG",
] as const;

function cleanEnvironment(overrides: NodeJS.ProcessEnv = {}): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env };
  for (const key of CONFIG_ENV_KEYS) delete env[key];
  env.DATABASE_URL = "";
  return { ...env, ...overrides };
}

// `timeout` is the anti-hang guard for the arms that assert a verb ANSWERS: a run
// that blocks forever has to come back as a killed process the assertion can name,
// not as a test runner that never finishes.
function spawnCli(
  args: readonly string[],
  options: { env?: NodeJS.ProcessEnv; cwd?: string; timeout?: number } = {},
) {
  return spawnSync(process.execPath, ["--import", "tsx", CLI_BIN, ...args], {
    encoding: "utf8",
    cwd: options.cwd,
    env: cleanEnvironment(options.env),
    timeout: options.timeout,
  });
}

function runCli(...args: string[]) {
  return spawnCli(args);
}

function runCliWithEnv(env: NodeJS.ProcessEnv, ...args: string[]) {
  return spawnCli(args, { env });
}

// The async peer of `spawnCli`, for the arms that have to act ON the CLI while it
// is still running (terminating the database backend it is blocked against). A
// `spawnSync` run cannot be reached from the test that started it.
function spawnCliAsync(
  args: readonly string[],
  options: { env?: NodeJS.ProcessEnv; cwd?: string } = {},
): Promise<{ status: number | null; stdout: string; stderr: string }> {
  const child = spawn(process.execPath, ["--import", "tsx", CLI_BIN, ...args], {
    cwd: options.cwd,
    env: cleanEnvironment(options.env),
  });
  let stdout = "";
  let stderr = "";
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", (chunk: string) => {
    stdout += chunk;
  });
  child.stderr.on("data", (chunk: string) => {
    stderr += chunk;
  });
  return new Promise((settle) => {
    child.on("close", (status) => settle({ status, stdout, stderr }));
  });
}

// Wait for a backend to park on the project advisory lock, then terminate it.
//
// Polling `pg_stat_activity` is what makes the kill deterministic instead of a
// race against a sleep: the deploy is only killable once it is actually waiting on
// the lock, and that is the state this waits for. Terminating a session that has
// been granted nothing and is waiting for the grant is exactly the shape
// `drop_grant_from_failed_acquire` compensates for.
async function killAdvisoryLockWaiter(probe: Client): Promise<number> {
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const { rows } = await probe.query<{ pid: number }>(
      `SELECT pid FROM pg_stat_activity
        WHERE pid <> pg_backend_pid()
          AND wait_event_type = 'Lock'
          AND wait_event = 'advisory'
          AND query LIKE '%pg_advisory_lock%'`,
    );
    if (rows.length > 0) {
      const pid = rows[0].pid;
      await probe.query("SELECT pg_terminate_backend($1)", [pid]);
      return pid;
    }
    await new Promise((wake) => setTimeout(wake, 25));
  }
  throw new Error("no backend ever parked on the project advisory lock");
}

function temporaryDirectory(prefix: string): string {
  return mkdtempSync(join(HERE, prefix));
}

function writePolicy(dir: string, name = "policy.toml"): string {
  const path = join(dir, name);
  writeFileSync(path, NO_INJECT_POLICY);
  return path;
}

function writeSimpleMigration(
  dir: string,
  options: {
    filename?: string;
    migrationName?: string;
    tableName?: string;
  } = {},
): string {
  const filename = options.filename ?? "20260715000000_create_widgets.mjs";
  const migrationName = options.migrationName ?? "create_widgets";
  const tableName = options.tableName ?? "widgets";
  const path = join(dir, filename);
  writeFileSync(
    path,
    `import { table, t } from "zero-migrate";
export const name = ${JSON.stringify(migrationName)};
export function up() {
  table(${JSON.stringify(tableName)}).create({ columns: { id: t.int() } });
}
`,
  );
  return path;
}

function makeStatus(overrides: Partial<StatusReply> = {}): StatusReply {
  return {
    applied: [],
    pending: [],
    aborted: [],
    rolledBack: [],
    pendingContracts: [],
    blocked: [],
    unexpectedJournal: [],
    plans: [],
    busy: false,
    lockHolders: [],
    ...overrides,
  };
}

test("CLI valueless flags reject supplied values", () => {
  for (const invocation of [
    ["apply", "--approve=false"],
    ["apply", "--approve", "false"],
    ["resolve", "rename_users", "--commit=false"],
    ["resolve", "rename_users", "--rollback=true"],
    ["status", "--strict=true"],
    ["lint", "--explain=false"],
    ["help", "--json=true"],
    ["help", "--help=true"],
  ]) {
    const result = runCli(...invocation);
    assert.equal(result.status, 1, `${invocation.join(" ")} must fail`);
    assert.match(
      result.stderr,
      /flag --(?:approve|commit|rollback|strict|explain|json|help) does not take a value|does not accept positional arguments/,
    );
  }
});

test("CLI value-taking flags reject a following flag as their value", () => {
  // A forgotten value used to be filled in by the next flag, so `new add_users
  // --dir --json` wrote the migration to ./--json and exited 0.
  for (const invocation of [
    ["new", "add_users", "--dir", "--json"],
    ["status", "--database-url", "--json"],
    ["apply", "--dir", "--approve"],
  ]) {
    const result = runCli(...invocation);
    assert.equal(result.status, 1, `${invocation.join(" ")} must fail`);
    assert.match(result.stderr, /needs a value, but the next argument is the flag/);
  }

  // `--dir=--json` is the documented inline escape hatch for a dash-leading path:
  // the value after `=` is taken literally instead of being parsed as a flag. Assert
  // the scaffold actually landed under a directory named `--json` so the arm proves
  // the literal value reached `--dir`, rather than only proving one error message was
  // absent. Runs in a throwaway cwd because `new` resolves `--dir` relative to cwd and
  // would otherwise write into the package root.
  //
  // Does NOT cover: whether `new` accepts or honors `--json` itself. `--json` is a
  // real boolean flag in the parser (`src/cli.ts` `case "json"`), borrowed here only
  // as a dash-leading STRING. Its FLAG meaning is covered further down this file on
  // `plan` and on `lint` ("lint defaults to all dialects and --dialect narrows"), and
  // its inline-value refusal by the `help --json=true` arm above; what no arm covers
  // is `new` + `--json`, which is a hole.
  //
  // Does NOT cover dash-leading values for any flag other than `--dir` - and does
  // not need to: the inline-`=` escape hatch and the following-flag refusal are ONE
  // shared closure (`takeVal` in `src/cli.ts`) that every value-taking flag routes
  // through, and the loop above already drives the refusal half through
  // `--database-url`.
  //
  // Does NOT cover the space-separated form, which the loop above rejects.
  const inlineCwd = temporaryDirectory(".cli-inline-dash-value-");
  try {
    const inline = spawnCli(["new", "add_users", "--dir=--json"], { cwd: inlineCwd });
    assert.equal(inline.status, 0, inline.stderr);
    const scaffolded = join(inlineCwd, "--json");
    assert.ok(existsSync(scaffolded), "--dir=--json must scaffold into ./--json");
    assert.deepEqual(
      readdirSync(scaffolded).map((entry) => entry.replace(/^\d{14}_/, "<stamp>_")),
      ["<stamp>_add_users.ts"],
    );
  } finally {
    rmSync(inlineCwd, { recursive: true, force: true });
  }
});

// The live continuation of the inline dash-leading `--dir` arm above: prove the file
// `--dir=--json` scaffolded is a real migration by applying it to PostgreSQL and
// reading the journal back out of the catalog instead of trusting rendered SQL or the
// file's text. `apply` is pointed at the same `--dir=--json`, so a parser that stops
// routing the literal value leaves apply with no migration to run and the journal
// read below finds no row.
//
// `new` emits a zero-op stub, so what a correct apply lands in the catalog is the
// journal row, not a user table, and the assertions say exactly that. Authoring ops
// into the file first would test authored ops rather than what the CLI scaffolds.
//
// The arm above stays ungated so the escape hatch keeps a guard on a machine with no
// database; this one adds the database half where PostgreSQL is available.
//
// Does NOT cover: MySQL, which has no scaffold-to-apply arm here and none elsewhere -
// the argv routing under test is dialect-independent (`--dir` is resolved before any
// driver is opened), but nothing proves that end to end on MySQL, so it is a hole.
//
// Does NOT cover the space-separated `--dir --json` form, which the loop above
// rejects.
//
// Does NOT cover `--json` as a flag on any verb: here it is only a dash-leading
// string. Its flag meaning is covered further down this file on `plan` and on `lint`
// ("lint defaults to all dialects and --dialect narrows"); the one verb no arm pairs
// it with is `new`, which is a hole.
//
// Does NOT cover any scaffold content beyond the stub; what `new` writes into the
// file is not asserted here or in the arm above.
//
// If the apply throws, the finally still drops both schemas, so a failure does not
// leak. Only a crash of the test process itself would leave `<schema>_migrations`
// behind, which is the exposure every live arm in this suite already carries.
test("CLI scaffold under an inline dash-leading --dir applies to live PostgreSQL", async (t) => {
  const client = await connectLivePg(t);
  if (client === null) return;
  const cwd = temporaryDirectory(".cli-inline-dash-live-");
  const schema = `zm_inline_dash_${Date.now().toString(36)}`;
  try {
    writePolicy(cwd);
    const scaffolded = spawnCli(["new", "add_users", "--dir=--json"], { cwd });
    assert.equal(scaffolded.status, 0, scaffolded.stderr);

    const applied = spawnCli(
      [
        "apply",
        "--dir=--json",
        `--database-url=${pgUrl()}`,
        `--schema=${schema}`,
        "--policy=policy.toml",
        "--approve",
      ],
      { cwd },
    );
    assert.equal(applied.status, 0, applied.stderr);

    const journal = await client.query(
      `SELECT event_kind, name FROM "${schema}_migrations".schema_migrations
        ORDER BY event_seq`,
    );
    assert.deepEqual(
      journal.rows.map((row) => [row.event_kind, row.name]),
      [["applied", "add_users"]],
    );

    // The stub declares no ops, so the project schema itself is never created.
    const created = await client.query(
      `SELECT nspname FROM pg_catalog.pg_namespace WHERE nspname = $1`,
      [schema],
    );
    assert.deepEqual(created.rows, []);
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(cwd, { recursive: true, force: true });
  }
});

test("CLI resolve parses commit and rollback and enforces its guards", () => {
  const url = "postgres://127.0.0.1:1/never_connect";

  const missingAction = runCli(
    "resolve",
    "rename_users",
    "--approve",
    `--database-url=${url}`,
  );
  assert.equal(missingAction.status, 1);
  assert.match(missingAction.stderr, /choose exactly one of --commit or --rollback/);

  const bothActions = runCli(
    "resolve",
    "rename_users",
    "--commit",
    "--rollback",
    "--approve",
    `--database-url=${url}`,
  );
  assert.equal(bothActions.status, 1);
  assert.match(bothActions.stderr, /choose exactly one of --commit or --rollback/);

  const missingApproval = runCli(
    "resolve",
    "rename_users",
    "--commit",
    `--database-url=${url}`,
  );
  assert.equal(missingApproval.status, 1);
  assert.match(missingApproval.stderr, /requires --approve/);

  const mysql = runCli(
    "resolve",
    "rename_users",
    "--commit",
    "--approve",
    "--database-url=mysql://127.0.0.1:1/never_connect",
  );
  assert.equal(mysql.status, 1);
  assert.match(mysql.stderr, /only PostgreSQL online renames/);
  assert.doesNotMatch(mysql.stderr, /ECONNREFUSED/);

  for (const action of ["--commit", "--rollback"]) {
    const accepted = runCli(
      "resolve",
      "rename_users",
      action,
      "--approve",
      `--database-url=${url}`,
    );
    assert.equal(accepted.status, 1);
    assert.match(accepted.stderr, /missing policy.*--policy <file>/i);
    assert.doesNotMatch(accepted.stderr, /unknown flag|ECONNREFUSED/i);
  }
});

test("rollback demands exactly one target and rejects a bad step count", () => {
  assert.deepEqual(rollbackTargetFromArgs({ rollbackAll: true }), { kind: "all" });
  assert.deepEqual(rollbackTargetFromArgs({ rollbackAll: false, rollbackSteps: "2" }), {
    kind: "steps",
    steps: 2,
  });
  assert.deepEqual(
    rollbackTargetFromArgs({ rollbackAll: false, rollbackTo: "mig_00000000000000000001" }),
    { kind: "toVersion", version: "mig_00000000000000000001" },
  );

  // No default: unwinding "some" of a schema has no safe fallback.
  assert.throws(() => rollbackTargetFromArgs({ rollbackAll: false }), /needs a target/);
  // Two targets means the operator meant one of them, and choosing for them would
  // unwind the wrong amount.
  assert.throws(
    () => rollbackTargetFromArgs({ rollbackAll: true, rollbackSteps: "1" }),
    /takes one target/,
  );
  for (const steps of ["-1", "1.5", "two", ""]) {
    assert.throws(
      () => rollbackTargetFromArgs({ rollbackAll: false, rollbackSteps: steps }),
      /non-negative whole number/,
      `--steps ${JSON.stringify(steps)} must be refused`,
    );
  }
});

test("rollback refuses without a target or without approval, before touching a database", () => {
  // A bogus URL: reaching the driver at all would fail differently, so these
  // refusals landing first is what proves they precede the connection.
  const url = "postgres://user:secret@127.0.0.1:1/none";

  const noTarget = runCli("rollback", `--database-url=${url}`, "--approve");
  assert.equal(noTarget.status, 1);
  assert.match(noTarget.stderr, /rollback needs a target/);
  assert.doesNotMatch(noTarget.stderr, /secret/);

  const noApprove = runCli("rollback", `--database-url=${url}`, "--all");
  assert.equal(noApprove.status, 1);
  assert.match(noApprove.stderr, /needs --approve/);

  const bothTargets = runCli("rollback", `--database-url=${url}`, "--all", "--steps=1", "--approve");
  assert.equal(bothTargets.status, 1);
  assert.match(bothTargets.stderr, /takes one target/);
});

test("the rollback-only flags are refused on every other command", () => {
  for (const flag of ["--all", "--steps=1", "--to=mig_1", "--force", "--backup-acknowledged"]) {
    const result = runCli("status", flag, "--database-url=postgres://u@127.0.0.1:1/n");
    assert.equal(result.status, 1, `${flag} must not be accepted by status`);
    assert.match(result.stderr, /only valid with rollback/);
  }
});

test("the rollback operator lines report reversed, skipped, and empty runs", () => {
  assert.equal(
    formatRollbackHuman({ rolledBack: ["mig_a", "mig_b"], skippedIrreversible: [] }),
    "rollback: 2 rolled back\n  reversed mig_a\n  reversed mig_b\n",
  );
  assert.match(
    formatRollbackHuman({ rolledBack: [], skippedIrreversible: ["mig_c"] }),
    /skipped mig_c \(declares no down; crossed under --force\)/,
  );
  // An empty run is not a silent success: nothing in range must say so.
  assert.match(
    formatRollbackHuman({ rolledBack: [], skippedIrreversible: [] }),
    /nothing was applied in the requested range/,
  );
});

test("removed CLI verbs are unknown and absent from help", () => {
  for (const command of ["preview", "resolve-pending"]) {
    const result = runCli(command);
    assert.equal(result.status, 1);
    assert.match(result.stderr, new RegExp(`unknown command .*${command}`));
  }

  const help = runCli("--help");
  assert.equal(help.status, 0);
  assert.doesNotMatch(help.stdout, /resolve-pending|zero-migrate preview/);
});

test("CLI rejects unsupported URL schemes without printing credentials", () => {
  const databaseUrl = "mariadb://private-user:secret-password@localhost/app.db";
  const result = runCli("apply", `--database-url=${databaseUrl}`);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /WARNING: --database-url contains an inline password/);
  assert.match(result.stderr, /could not infer a driver/);
  assert.match(result.stderr, /expected a postgres:\/\/ or mysql:\/\/ scheme/);
  assert.doesNotMatch(result.stderr, /private-user|secret-password|localhost\/app/);

  const dir = temporaryDirectory(".cli-credential-warning-");
  try {
    const configFailure = runCli(
      "apply",
      `--database-url=${databaseUrl}`,
      `--config=${join(dir, "missing.toml")}`,
    );
    assert.equal(configFailure.status, 1);
    assert.match(
      configFailure.stderr,
      /WARNING: --database-url contains an inline password/,
    );
    assert.match(configFailure.stderr, /read config file/);
    assert.doesNotMatch(
      configFailure.stderr,
      /private-user|secret-password|localhost\/app/,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("CLI never prints an unsupported database URL", () => {
  const secretUrl = "unknown://private-user:secret-password@localhost/app";
  const result = runCli("status", `--database-url=${secretUrl}`);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /could not infer a driver/);
  assert.equal(result.stderr.includes(secretUrl), false);
  assert.doesNotMatch(result.stderr, /private-user|secret-password/);

  const misplaced = runCli("status", "ignored", secretUrl);
  assert.equal(misplaced.status, 1);
  assert.doesNotMatch(misplaced.stderr, /private-user|secret-password|unknown:\/\//);
  assert.match(misplaced.stderr, /redacted database URL/);

  const asCommand = runCli(secretUrl);
  assert.equal(asCommand.status, 1);
  assert.doesNotMatch(asCommand.stderr, /private-user|secret-password|unknown:\/\//);
  assert.match(asCommand.stderr, /redacted database URL/);
});

test("CLI rejects an explicit empty URL instead of using DATABASE_URL", () => {
  const result = runCliWithEnv(
    { DATABASE_URL: "postgres://production.example/app" },
    "apply",
    "--database-url=",
  );
  assert.equal(result.status, 1);
  assert.match(result.stderr, /missing database URL/);
  assert.doesNotMatch(result.stderr, /migrations dir|production\.example/);
});

test("inline-password detection follows the URL authority rule", () => {
  assert.equal(hasInlinePassword("postgres://user:password@db.example/app"), true);
  assert.equal(hasInlinePassword("mysql://user:@db.example/app"), true);
  assert.equal(hasInlinePassword("postgres://user@db.example/app"), false);
  assert.equal(hasInlinePassword("sqlite:./app.db"), false);
});

test("CLI honors help before validating command positionals", () => {
  for (const invocation of [
    ["lint", "--help"],
    ["plan", "--help"],
    ["apply", "--help"],
    ["status", "--help"],
    ["resolve", "rename_users", "--help"],
    ["new", "demo", "--help"],
  ]) {
    const result = runCli(...invocation);
    assert.equal(result.status, 0, `${invocation.join(" ")} must show help`);
    assert.match(result.stdout, /^zero-migrate: database migrations from JavaScript/);
    assert.equal(result.stderr, "");
  }
});

test("CLI help documents the v2 surface, config, and dialect rule", () => {
  const help = runCli("--help");
  assert.equal(help.status, 0);
  for (const command of ["new", "lint", "plan", "apply", "status", "resolve", "history"]) {
    assert.match(help.stdout, new RegExp(`zero-migrate ${command}(?: |$)`));
  }
  assert.match(help.stdout, /--config <path>/);
  assert.match(help.stdout, /--env <name>/);
  assert.match(help.stdout, /--dialect <name>\s+lint only/);
  assert.match(help.stdout, /Only lint accepts --dialect/);
  // An opt-in switch nobody can find is off for everyone. The help is the one
  // place an operator looks for it, and it has to say which stream it writes to.
  assert.match(help.stdout, /ZERO_MIGRATE_LOG=1[\s\S]*stderr/);
  assert.match(help.stdout, /--commit/);
  assert.match(help.stdout, /--rollback/);
  assert.match(help.stdout, /--strict/);
  assert.match(help.stdout, /rollback reverses\napplied migrations from their authored down; there is no clean command/);
  assert.match(help.stdout, /zero-migrate rollback \(--to <version> \| --steps <n> \| --all\)/);
  assert.match(help.stdout, /plan, apply and rollback support PostgreSQL, MySQL 8, and SQLite/);
  assert.match(help.stdout, /status supports\s+PostgreSQL and MySQL 8/);
  assert.doesNotMatch(help.stdout, /\u2014|host driver seam|addon|in-process/);

  const liveDialect = runCli("plan", "--dialect=postgres");
  assert.equal(liveDialect.status, 1);
  assert.match(liveDialect.stderr, /--dialect is only valid with the lint command/);
});

test("CLI derives SQLite app and journal paths and honors --journal", () => {
  assert.deepEqual(driverFor("sqlite:/tmp/app.db"), {
    kind: "sqlite",
    appPath: "/tmp/app.db",
    journalPath: "/tmp/app.migrations.db",
  });
  assert.deepEqual(driverFor("sqlite:///tmp/app.sqlite"), {
    kind: "sqlite",
    appPath: "/tmp/app.sqlite",
    journalPath: "/tmp/app.migrations.sqlite",
  });
  assert.deepEqual(driverFor("sqlite:./data/app"), {
    kind: "sqlite",
    appPath: "./data/app",
    journalPath: "./data/app.migrations",
  });
  assert.deepEqual(driverFor("./data/app.db", "/tmp/custom-journal.sqlite"), {
    kind: "sqlite",
    appPath: "./data/app.db",
    journalPath: "/tmp/custom-journal.sqlite",
  });
  assert.throws(
    () => driverFor("postgres://localhost/app", "/tmp/journal.db"),
    /--journal is only valid for a SQLite database URL/,
  );
  assert.throws(
    () => driverFor("sqlite:/tmp/app.db", ""),
    /--journal needs a non-empty file path/,
  );
});

test("driverFor attaches host-enforced transport security to network drivers", () => {
  const security = { hostAllowlist: ["db.internal"], queryTimeoutMs: 5000 };
  assert.deepEqual(driverFor("postgres://db.internal/app", undefined, security), {
    kind: "postgres",
    url: "postgres://db.internal/app",
    security,
  });
  assert.deepEqual(driverFor("mysql://db.internal/app", undefined, security), {
    kind: "mysql",
    url: "mysql://db.internal/app",
    security,
  });
});

test("resolveNetworkSecurity: flags win over env, absent yields undefined", () => {
  // Nothing set anywhere -> undefined (driver default unchanged, controls off).
  assert.equal(resolveNetworkSecurity({}, {}), undefined);

  // Env-only supplies the controls.
  const fromEnv = resolveNetworkSecurity(
    {},
    {
      ZERO_MIGRATE_HOST_ALLOWLIST: "a.example, b.example ,",
      ZERO_MIGRATE_QUERY_TIMEOUT_MS: "2500",
    },
  );
  assert.deepEqual(fromEnv, {
    hostAllowlist: ["a.example", "b.example"],
    queryTimeoutMs: 2500,
  });

  // A flag overrides the env for the same control.
  const flagWins = resolveNetworkSecurity(
    { hostAllowlist: "only.flag" },
    { ZERO_MIGRATE_HOST_ALLOWLIST: "ignored.env" },
  );
  assert.deepEqual(flagWins, { hostAllowlist: ["only.flag"] });
});

test("resolveNetworkSecurity rejects a non-positive query timeout", () => {
  assert.throws(
    () => resolveNetworkSecurity({ queryTimeoutMs: "0" }, {}),
    /--query-timeout must be a positive integer/,
  );
  assert.throws(
    () => resolveNetworkSecurity({ queryTimeoutMs: "abc" }, {}),
    /--query-timeout must be a positive integer/,
  );
});

test("resolveNetworkSecurity reads and pins the --tls-ca bundle contents", () => {
  const dir = mkdtempSync(join(tmpdir(), "zm-tlsca-"));
  try {
    const caPath = join(dir, "ca.pem");
    writeFileSync(caPath, "-----BEGIN CERTIFICATE-----\nPINME\n-----END CERTIFICATE-----\n");
    const security = resolveNetworkSecurity({ tlsCaPath: caPath }, {});
    assert.match(security?.tlsCa ?? "", /PINME/);
    assert.throws(
      () => resolveNetworkSecurity({ tlsCaPath: join(dir, "missing.pem") }, {}),
      /--tls-ca: cannot read CA bundle/,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("live plan leaves a fresh SQLite journal absent", () => {
  const dir = temporaryDirectory(".cli-plan-sqlite-read-only-");
  try {
    writeSimpleMigration(dir);
    const policyPath = writePolicy(dir);
    const appPath = join(dir, "fresh.db");
    const journalPath = join(dir, "fresh.migrations.db");
    assert.equal(existsSync(appPath), false, "application database starts absent");
    assert.equal(existsSync(journalPath), false, "migration journal starts absent");

    const result = runCliWithEnv(
      { ZERO_MIGRATE_ADDON_PATH: ADDON_PATH },
      "plan",
      `--dir=${dir}`,
      `--policy=${policyPath}`,
      `--database-url=sqlite:${appPath}`,
      "--json",
    );

    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.stderr, "");
    const report = JSON.parse(result.stdout) as {
      count: number;
      pending: Array<{ version: string; name: string; sql: string }>;
    };
    assert.equal(report.count, 1);
    assert.equal(report.pending.length, 1);
    assert.match(report.pending[0].version, /^mig_/);
    assert.equal(report.pending[0].name, "create_widgets");
    assert.match(report.pending[0].sql, /CREATE TABLE/i);
    assert.match(report.pending[0].sql, /widgets/i);
    assert.equal(
      existsSync(journalPath),
      false,
      "read-only plan must not create the derived SQLite journal",
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("CLI database verbs require an explicit policy file", () => {
  const invocations = [
    ["plan"],
    ["apply"],
    ["status"],
    ["history"],
    ["resolve", "rename_users", "--commit", "--approve"],
  ];
  for (const invocation of invocations) {
    const base = [
      ...invocation,
      "--database-url=postgres://127.0.0.1:1/never_connect",
    ];
    const missing = runCli(...base);
    assert.equal(missing.status, 1, `${invocation[0]} must reject a missing policy`);
    assert.match(missing.stderr, /missing policy.*--policy <file>/i);
    assert.doesNotMatch(missing.stderr, /ECONNREFUSED|connect/i);

    const empty = runCli(...base, "--policy=");
    assert.equal(empty.status, 1, `${invocation[0]} must reject an empty policy path`);
    assert.match(empty.stderr, /--policy needs a non-empty file path/i);
    assert.doesNotMatch(empty.stderr, /ECONNREFUSED|connect/i);
  }
});

test("CLI rejects an empty policy before opening a database session", () => {
  const dir = temporaryDirectory(".cli-policy-");
  try {
    const policyPath = join(dir, "empty-policy.toml");
    writeFileSync(policyPath, "");
    const result = runCli(
      "apply",
      "--database-url=postgres://127.0.0.1:1/never_connect",
      `--policy=${policyPath}`,
    );
    assert.equal(result.status, 1);
    assert.match(result.stderr, /policy file .* is empty/i);
    assert.doesNotMatch(result.stderr, /ECONNREFUSED|connect/i);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("CLI loads repeated policy files in occurrence order", () => {
  const dir = temporaryDirectory(".cli-policy-layers-");
  try {
    const missingRootPath = join(dir, "missing-root.toml");
    const laterLayerPath = writePolicy(dir, "later-layer.toml");
    const result = runCli(
      "apply",
      "--database-url=postgres://127.0.0.1:1/never_connect",
      "--policy",
      missingRootPath,
      `--policy=${laterLayerPath}`,
    );

    assert.equal(result.status, 1);
    assert.ok(result.stderr.includes(missingRootPath), result.stderr);
    assert.doesNotMatch(result.stderr, /migrations dir|ECONNREFUSED|connect/i);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("config precedence is flag, environment, config, then default", () => {
  const dir = temporaryDirectory(".cli-config-precedence-");
  try {
    const configPath = join(dir, "zero-migrate.toml");
    writeFileSync(
      configPath,
      `[env.dev]
url = "postgres://config.example/app"
dir = "./config-migrations"
owner_app = "app_config"
schema = "config_schema"
registry = "./config-registry.json"
policy = ["./root.toml", "./leaf.toml"]
`,
    );

    const resolved = resolveCliConfig({
      cwd: dir,
      configPath,
      explicit: {
        databaseUrl: "postgres://flag.example/app",
        projectSchema: "flag_schema",
      },
      processEnv: {
        ZERO_MIGRATE_URL: "postgres://environment.example/app",
        ZERO_MIGRATE_OWNER_APP: "app_environment",
      },
    });

    assert.equal(resolved.databaseUrl, "postgres://flag.example/app");
    assert.equal(resolved.projectSchema, "flag_schema");
    assert.equal(resolved.ownerApp, "app_environment");
    assert.equal(resolved.dir, resolve(dir, "config-migrations"));
    assert.equal(resolved.registryPath, resolve(dir, "config-registry.json"));
    assert.deepEqual(resolved.policyPaths, [
      resolve(dir, "root.toml"),
      resolve(dir, "leaf.toml"),
    ]);

    const configOverLegacyUrl = resolveCliConfig({
      cwd: dir,
      configPath,
      processEnv: { DATABASE_URL: "postgres://legacy.example/app" },
    });
    assert.equal(configOverLegacyUrl.databaseUrl, "postgres://config.example/app");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("config env references resolve set values and reject unset values", () => {
  const dir = temporaryDirectory(".cli-config-env-");
  try {
    const configPath = join(dir, "zero-migrate.toml");
    writeFileSync(
      configPath,
      `[env.dev]
url = "env:CLI_TEST_DATABASE_URL"
policy = ["env:CLI_TEST_ROOT_POLICY", "./leaf.toml"]
`,
    );

    const loaded = loadZeroMigrateConfig({
      cwd: dir,
      configPath,
      processEnv: {
        CLI_TEST_DATABASE_URL: "postgres://configured.example/app",
        CLI_TEST_ROOT_POLICY: "./root.toml",
      },
    });
    assert.equal(loaded?.values.url, "postgres://configured.example/app");
    assert.deepEqual(loaded?.values.policy, [
      resolve(dir, "root.toml"),
      resolve(dir, "leaf.toml"),
    ]);

    assert.throws(
      () =>
        loadZeroMigrateConfig({
          cwd: dir,
          configPath,
          processEnv: { CLI_TEST_ROOT_POLICY: "./root.toml" },
        }),
      /references unset environment variable CLI_TEST_DATABASE_URL/,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("lint defaults to all dialects and --dialect narrows", () => {
  const dir = temporaryDirectory(".cli-lint-dialects-");
  try {
    writeSimpleMigration(dir);
    const env = { ZERO_MIGRATE_ADDON_PATH: ADDON_PATH };

    const all = runCliWithEnv(env, "lint", `--dir=${dir}`, "--json");
    assert.equal(all.status, 0, all.stderr || all.stdout);
    const allReports = JSON.parse(all.stdout) as Array<{
      ok: boolean;
      dialects: Array<{ dialect: string; ok: boolean }>;
    }>;
    assert.equal(allReports.length, 1);
    assert.equal(allReports[0].ok, true);
    assert.deepEqual(
      allReports[0].dialects.map(({ dialect }) => dialect),
      ["postgres", "mysql", "sqlite"],
    );
    assert.ok(allReports[0].dialects.every(({ ok }) => ok));

    const mysql = runCliWithEnv(
      env,
      "lint",
      `--dir=${dir}`,
      "--dialect=mysql",
      "--json",
    );
    assert.equal(mysql.status, 0, mysql.stderr || mysql.stdout);
    const mysqlReports = JSON.parse(mysql.stdout) as Array<{
      dialects: Array<{ dialect: string }>;
    }>;
    assert.deepEqual(mysqlReports[0].dialects.map(({ dialect }) => dialect), ["mysql"]);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("lint --explain prints rendered SQL", () => {
  const dir = temporaryDirectory(".cli-lint-explain-");
  try {
    writeSimpleMigration(dir);
    const result = runCliWithEnv(
      { ZERO_MIGRATE_ADDON_PATH: ADDON_PATH },
      "lint",
      `--dir=${dir}`,
      "--dialect=postgres",
      "--explain",
    );
    assert.equal(result.status, 0, result.stderr || result.stdout);
    assert.match(result.stdout, /lint create_widgets: ok/);
    assert.match(result.stdout, /CREATE TABLE/i);
    assert.match(result.stdout, /dialect: postgres/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("lint reports preview failures as per-migration dialect verdicts", () => {
  const dir = temporaryDirectory(".cli-lint-dialect-failure-");
  try {
    writeFileSync(
      join(dir, "20260715000000_rename_label.mjs"),
      `import { table, t } from "zero-migrate";
export const name = "rename_label";
export function up() {
  table("widgets").column("old_label").rename({
    to: "new_label",
    type: t.text(),
  });
}
`,
    );
    const registryPath = join(dir, "registry.json");
    writeFileSync(registryPath, JSON.stringify({ widgets: "app_cli" }));

    const result = runCliWithEnv(
      { ZERO_MIGRATE_ADDON_PATH: ADDON_PATH },
      "lint",
      `--dir=${dir}`,
      "--dialect=mysql",
      `--registry=${registryPath}`,
    );

    assert.equal(result.status, 1);
    assert.match(result.stdout, /lint rename_label: fail/);
    assert.match(result.stdout, /mysql/);
    assert.equal(result.stderr, "");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("lint validates the selected policy charter without --explain", () => {
  const dir = temporaryDirectory(".cli-lint-policy-");
  try {
    writeSimpleMigration(dir);
    const policyPath = join(dir, "invalid-policy.toml");
    writeFileSync(policyPath, "this is not valid charter TOML");

    const result = runCliWithEnv(
      { ZERO_MIGRATE_ADDON_PATH: ADDON_PATH },
      "lint",
      `--dir=${dir}`,
      "--dialect=postgres",
      `--policy=${policyPath}`,
    );

    assert.equal(result.status, 1);
    assert.match(result.stdout, /lint create_widgets: fail/);
    assert.match(result.stdout, /policy|charter|TOML/i);
    assert.equal(result.stderr, "");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("lint accepts a trusted ownership registry", () => {
  const dir = temporaryDirectory(".cli-registry-");
  try {
    writeSimpleMigration(dir, {
      filename: "20260715000000_create_users.mjs",
      migrationName: "create_users",
      tableName: "users",
    });
    writeFileSync(
      join(dir, "20260715000001_add_timezone.mjs"),
      `import { table, t } from "zero-migrate";
export const name = "add_timezone";
export function up() {
  table("users").column("timezone").add({ type: t.text() });
}
`,
    );
    const registryPath = join(dir, "registry.json");
    writeFileSync(registryPath, JSON.stringify({ users: "app_cli" }));
    const env = { ZERO_MIGRATE_ADDON_PATH: ADDON_PATH };

    const withoutRegistry = runCliWithEnv(env, "lint", `--dir=${dir}`);
    assert.equal(withoutRegistry.status, 1);
    assert.match(withoutRegistry.stdout, /lint add_timezone: fail/);
    assert.match(withoutRegistry.stdout, /unregistered/i);

    const withRegistry = runCliWithEnv(
      env,
      "lint",
      `--dir=${dir}`,
      `--registry=${registryPath}`,
    );
    assert.equal(withRegistry.status, 0, withRegistry.stderr || withRegistry.stdout);
    assert.match(withRegistry.stdout, /lint create_users: ok/);
    assert.match(withRegistry.stdout, /lint add_timezone: ok/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("lint and apply reject duplicate resolved migration names before applying", () => {
  const dir = temporaryDirectory(".cli-duplicate-names-");
  try {
    writeFileSync(
      join(dir, "20260715000000_first.mjs"),
      `export const name = "shared_identity";
export function up() {}
`,
    );
    writeFileSync(
      join(dir, "20260715000001_second.mjs"),
      `export default {
  name: "shared_identity",
  up() {},
};
`,
    );
    const env = { ZERO_MIGRATE_ADDON_PATH: ADDON_PATH };
    const policyPath = writePolicy(dir);

    const linted = runCliWithEnv(env, "lint", `--dir=${dir}`);
    assert.equal(linted.status, 1);
    assert.match(linted.stderr, /duplicate migration name.*shared_identity/i);
    assert.match(linted.stderr, /20260715000000_first/);
    assert.match(linted.stderr, /20260715000001_second/);

    const applied = runCliWithEnv(
      env,
      "apply",
      `--dir=${dir}`,
      "--database-url=postgres://127.0.0.1:1/never_connect",
      `--policy=${policyPath}`,
    );
    assert.equal(applied.status, 1);
    assert.match(applied.stderr, /duplicate migration name.*shared_identity/i);
    assert.doesNotMatch(applied.stderr, /ECONNREFUSED|connect/i);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("lint reports schema-confinement failures as a migration verdict", () => {
  const dir = temporaryDirectory(".cli-schema-");
  try {
    writeFileSync(
      join(dir, "20260715000000_foreign_schema.mjs"),
      `import { table, t } from "zero-migrate";
export const name = "foreign_schema";
export function up() {
  table("widgets", { schema: "outside_project" }).create({
    columns: { id: t.int() },
  });
}
`,
    );
    const result = runCliWithEnv(
      { ZERO_MIGRATE_ADDON_PATH: ADDON_PATH },
      "lint",
      `--dir=${dir}`,
      "--schema=app_data",
    );

    assert.equal(result.status, 1);
    assert.match(result.stdout, /lint foreign_schema: fail/);
    assert.match(result.stdout, /outside_project|cross.schema/i);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("status helpers implement clean, pending, and dirty strict outcomes", () => {
  const clean = makeStatus({
    currentVersion: "mig_applied",
    applied: ["mig_applied"],
    plans: [
      {
        version: "mig_applied",
        name: "create_users",
        state: "applied",
        steps: [
          { version: "mig_step_applied", name: "create users", kind: "ddl", state: "applied" },
        ],
        missingDependencies: [],
      },
    ],
  });
  assert.equal(statusIsDirty(clean), false);
  assert.equal(statusExitCode(clean, true), 0);
  assert.equal(statusExitCode(clean, false), 0);
  assert.equal(formatStatusHuman(clean), "status: 1 applied, 0 pending\n");

  const pending = makeStatus({
    pending: ["mig_pending"],
    plans: [
      {
        version: "mig_pending",
        name: "add_timezone",
        state: "pending",
        steps: [
          { version: "mig_step_pending", name: "add timezone", kind: "ddl", state: "pending" },
        ],
        missingDependencies: [],
      },
    ],
  });
  assert.equal(statusIsDirty(pending), true);
  assert.equal(statusExitCode(pending, true), 1);
  assert.equal(statusExitCode(pending, false), 0);
  assert.match(formatStatusHuman(pending), /^status: 0 applied, 1 pending\n$/);

  const drifted = makeStatus({
    unexpectedJournal: [
      {
        version: "mig_unexpected",
        state: "applied",
        journalChecksum: "recorded-checksum",
        journalKind: "apply",
      },
    ],
    plans: [
      {
        version: "mig_drifted",
        name: "create_orders",
        state: "drifted",
        steps: [
          {
            version: "mig_step_drifted",
            name: "create orders",
            kind: "ddl",
            state: "drifted",
          },
        ],
        missingDependencies: [],
      },
    ],
  });
  assert.equal(statusIsDirty(drifted), true);
  assert.equal(statusExitCode(drifted, true), 1);
  const human = formatStatusHuman(drifted);
  assert.match(human, /drift: unexpected journal entry mig_unexpected \(applied\)/);
  assert.match(human, /checksum mismatch: create_orders, step create orders \(mig_step_drifted\)/);

  const json = JSON.parse(formatStatusJson(drifted)) as StatusReply;
  assert.deepEqual(json.pending, []);
  assert.equal(json.unexpectedJournal[0].journalChecksum, "recorded-checksum");
  assert.equal(json.plans?.[0].steps[0].state, "drifted");
});

test("status strict treats outstanding contracts as dirty", () => {
  const reply = makeStatus({
    pendingContracts: [
      { table: "users", pendingVersion: "mig_trigger", orphaned: false },
    ],
  });
  assert.equal(statusIsDirty(reply), true);
  assert.match(formatStatusHuman(reply), /pending online rename: users \(mig_trigger\)/);
});

test("pendingMigrationsForPlan maps pending plan IDs to source envelopes", () => {
  const first: IrEnvelope = { ir_version: 1, name: "first", ops: [] };
  const second: IrEnvelope = { ir_version: 1, name: "second", ops: [] };
  const reply = makeStatus({
    applied: ["mig_first"],
    pending: ["mig_second"],
    plans: [
      {
        version: "mig_second",
        name: "second",
        state: "pending",
        steps: [],
        missingDependencies: [],
      },
      {
        version: "mig_first",
        name: "first",
        state: "applied",
        steps: [],
        missingDependencies: [],
      },
    ],
  });

  assert.deepEqual(pendingMigrationsForPlan(reply, [first, second]), [
    { version: "mig_second", name: "second", envelope: second },
  ]);

  assert.throws(
    () => pendingMigrationsForPlan(makeStatus({ pending: ["mig_missing"] }), [first]),
    /cannot be matched to source.*mig_missing/,
  );
  assert.throws(
    () =>
      pendingMigrationsForPlan(
        makeStatus({
          plans: [
            { version: "mig_one", name: "first", state: "pending", steps: [], missingDependencies: [] },
            { version: "mig_two", name: "first", state: "pending", steps: [], missingDependencies: [] },
          ],
        }),
        [first],
      ),
    /ambiguous migration name "first"/,
  );
});

test("pending plan envelopes feed the offline SQL renderer", () => {
  const previousAddonPath = process.env.ZERO_MIGRATE_ADDON_PATH;
  process.env.ZERO_MIGRATE_ADDON_PATH = ADDON_PATH;
  try {
    const envelope: IrEnvelope = {
      ir_version: currentIrVersion(),
      name: "create_plan_widgets",
      ops: [
        {
          op: "createTable",
          name: "plan_widgets",
          columns: [{ name: "id", type: "int" }],
          primaryKey: null,
          constraints: [],
          indexes: [],
        },
      ],
    };
    const selected = pendingMigrationsForPlan(
      makeStatus({
        pending: ["mig_plan_widgets"],
        plans: [
          {
            version: "mig_plan_widgets",
            name: envelope.name,
            state: "pending",
            steps: [],
            missingDependencies: [],
          },
        ],
      }),
      [envelope],
    );
    const rendered = previewSql({
      envelopes: selected.map(({ envelope: value }) => JSON.stringify(value)),
      dialect: "postgres",
      defaultSchema: "public",
      ownerApp: "app_cli",
      charterLayers: [NO_INJECT_POLICY],
    });
    assert.equal(rendered.length, 1);
    assert.match(rendered[0], /CREATE TABLE/i);
    assert.match(rendered[0], /plan_widgets/);
  } finally {
    if (previousAddonPath === undefined) delete process.env.ZERO_MIGRATE_ADDON_PATH;
    else process.env.ZERO_MIGRATE_ADDON_PATH = previousAddonPath;
  }
});

test("resolvePendingVersion maps a migration name to one obligation", () => {
  const reply = makeStatus({
    pending: ["mig_rename_plan"],
    pendingContracts: [
      { table: "users", pendingVersion: "mig_rename_trigger", orphaned: false },
    ],
    plans: [
      {
        version: "mig_rename_plan",
        name: "rename_users",
        state: "partial",
        steps: [
          {
            version: "mig_rename_trigger",
            name: "install rename trigger",
            kind: "onlineExpand",
            state: "applied",
          },
        ],
        missingDependencies: [],
      },
    ],
  });
  assert.equal(resolvePendingVersion(reply, "rename_users"), "mig_rename_trigger");

  assert.throws(
    () => resolvePendingVersion(reply, "unknown_rename"),
    /unknown pending online-rename migration "unknown_rename"/,
  );

  const ambiguousName = makeStatus({
    plans: [
      { version: "mig_one", name: "rename_users", state: "partial", steps: [], missingDependencies: [] },
      { version: "mig_two", name: "rename_users", state: "partial", steps: [], missingDependencies: [] },
    ],
  });
  assert.throws(
    () => resolvePendingVersion(ambiguousName, "rename_users"),
    /ambiguous migration name "rename_users"/,
  );

  const ambiguousContract = makeStatus({
    pending: ["mig_plan"],
    pendingContracts: [
      { table: "users", pendingVersion: "mig_trigger_one", orphaned: false },
      { table: "users", pendingVersion: "mig_trigger_two", orphaned: false },
    ],
    plans: [
      {
        version: "mig_plan",
        name: "rename_users",
        state: "partial",
        steps: [
          { version: "mig_trigger_one", name: "trigger one", kind: "onlineExpand", state: "applied" },
          { version: "mig_trigger_two", name: "trigger two", kind: "onlineExpand", state: "applied" },
        ],
        missingDependencies: [],
      },
    ],
  });
  assert.throws(
    () => resolvePendingVersion(ambiguousContract, "rename_users"),
    /multiple pending online renames.*ambiguous/,
  );
});

test("resolve reports an unknown local migration before connecting", () => {
  const dir = temporaryDirectory(".cli-resolve-unknown-");
  try {
    writeSimpleMigration(dir, { migrationName: "known_migration" });
    const policyPath = writePolicy(dir);
    const result = runCliWithEnv(
      { ZERO_MIGRATE_ADDON_PATH: ADDON_PATH },
      "resolve",
      "unknown_migration",
      "--commit",
      "--approve",
      `--dir=${dir}`,
      `--policy=${policyPath}`,
      "--database-url=postgres://127.0.0.1:1/never_connect",
    );
    assert.equal(result.status, 1);
    assert.match(result.stderr, /unknown migration "unknown_migration"/);
    assert.doesNotMatch(result.stderr, /ECONNREFUSED|connect/i);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("live plan connect failures are clean, warned, and redacted", () => {
  const dir = temporaryDirectory(".cli-plan-connect-");
  try {
    writeSimpleMigration(dir);
    const policyPath = writePolicy(dir);
    const secretUrl =
      "postgres://private-user:secret-password@127.0.0.1:1/never_connect";
    const result = runCliWithEnv(
      { ZERO_MIGRATE_ADDON_PATH: ADDON_PATH },
      "plan",
      `--dir=${dir}`,
      `--policy=${policyPath}`,
      `--database-url=${secretUrl}`,
    );

    assert.equal(result.status, 1);
    assert.match(result.stderr, /WARNING: --database-url contains an inline password/);
    assert.match(result.stderr, /ECONNREFUSED|connect/i);
    assert.equal(result.stderr.includes(secretUrl), false);
    assert.doesNotMatch(result.stderr, /private-user|secret-password|postgres:\/\//i);
    assert.doesNotMatch(result.stdout, /CREATE TABLE|would apply/i);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// A peer's deploy holds the project advisory lock for the whole length of its run.
// `status --strict` is the documented CI gate and `plan` is the read-only preview,
// so neither may sit behind that lock: waiting turns one slow deploy into a stalled
// pipeline, and there is no timeout to end the wait.
//
// The lock is held from a SECOND LIVE SESSION for the duration of both runs, which
// is the only way to reproduce what a peer's deploy does to a reader: a mock that
// merely reports contention proves the plumbing, not that the acquisition itself
// stopped waiting. Both verbs must answer WHILE the lock is still held.
//
// Exit 0 is deliberate: contention is not a dirty migration set, and a strict gate
// that failed on it would fail every pipeline that overlaps a deploy. A CI that
// wants to fail on contention opts in through the machine-readable `busy` flag in
// the `--json` reply, which is why that flag is asserted here too.
test("CLI status and plan answer while a peer holds the project lock", async (t) => {
  const holder = await connectLivePg(t);
  if (holder === null) return;
  const cwd = temporaryDirectory(".cli-lock-busy-");
  const schema = `zm_lock_busy_${Date.now().toString(36)}`;
  let held = false;
  try {
    writeSimpleMigration(cwd);
    // The migration renders into the per-test schema, so the charter has to own
    // that schema; the bare policy other arms use owns none and the guard would
    // deny the CREATE TABLE before the lock ever mattered.
    const policyPath = join(cwd, "policy.toml");
    writeFileSync(policyPath, noInjectPolicy(schema));

    // The project lock key is `hashtext(project_id)` and the CLI's project_id is the
    // project schema, so this is the same key the reader will try to take.
    await holder.query("SELECT pg_advisory_lock(hashtext($1)::bigint)", [schema]);
    held = true;
    const holderPid = (await holder.query("SELECT pg_backend_pid() AS pid")).rows[0]
      .pid as number;

    const common = [
      `--dir=${cwd}`,
      `--policy=${policyPath}`,
      `--database-url=${pgUrl()}`,
      `--schema=${schema}`,
    ];
    const options = { env: { ZERO_MIGRATE_ADDON_PATH: ADDON_PATH }, timeout: 60_000 };

    const status = spawnCli(["status", "--strict", ...common], options);
    assert.equal(
      status.signal,
      null,
      `status blocked on the peer's project lock instead of reporting it: ${status.stderr}`,
    );
    assert.equal(status.status, 0, status.stderr);
    assert.match(status.stderr, /project lock/i);
    assert.match(status.stderr, new RegExp(`\\b${holderPid}\\b`));

    const plan = spawnCli(["plan", ...common], options);
    assert.equal(
      plan.signal,
      null,
      `plan blocked on the peer's project lock instead of reporting it: ${plan.stderr}`,
    );
    assert.equal(plan.status, 0, plan.stderr);
    assert.match(plan.stderr, /project lock/i);
    assert.doesNotMatch(plan.stdout, /would apply|CREATE TABLE/i);

    const json = spawnCli(["status", "--strict", "--json", ...common], options);
    assert.equal(json.signal, null, json.stderr);
    assert.equal(json.status, 0, json.stderr);
    const reply = JSON.parse(json.stdout) as StatusReply;
    assert.equal(reply.busy, true);
    assert.deepEqual(
      reply.lockHolders.map((entry) => entry.pid),
      [holderPid],
    );

    // `resolve` reads through the same status verb but WRITES, so it cannot carry
    // on with a reply that read nothing. It fails loudly and names the contention
    // rather than blaming the operator's migration argument for a peer's deploy.
    const resolve = spawnCli(
      ["resolve", "create_widgets", "--commit", "--approve", ...common],
      options,
    );
    assert.equal(resolve.signal, null, resolve.stderr);
    assert.equal(resolve.status, 1, resolve.stderr);
    assert.match(resolve.stderr, /another deploy holds the project lock/);
    assert.doesNotMatch(resolve.stderr, /unknown migration/);

    // Positive control: the same command against the same database, with the lock
    // free, still takes the lock, still reads, and still returns its real verdict --
    // exit 1 for the pending migration this directory carries. Without it, the arms
    // above would also pass if the verbs had simply stopped doing any work.
    await holder.query("SELECT pg_advisory_unlock(hashtext($1)::bigint)", [schema]);
    held = false;

    const uncontended = spawnCli(["status", "--strict", ...common], options);
    assert.equal(uncontended.signal, null, uncontended.stderr);
    assert.equal(uncontended.status, 1, uncontended.stderr);
    assert.match(uncontended.stdout, /status: 0 applied, 1 pending/);
    assert.doesNotMatch(uncontended.stderr, /project lock/i);

    const uncontendedPlan = spawnCli(["plan", ...common], options);
    assert.equal(uncontendedPlan.status, 0, uncontendedPlan.stderr);
    assert.match(uncontendedPlan.stdout, /would apply 1 migration/);
    assert.match(uncontendedPlan.stdout, /CREATE TABLE/i);
  } finally {
    if (held) {
      await holder
        .query("SELECT pg_advisory_unlock(hashtext($1)::bigint)", [schema])
        .catch(() => {});
    }
    await holder
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
      )
      .catch(() => {});
    await holder.end().catch(() => {});
    rmSync(cwd, { recursive: true, force: true });
  }
});

// The engine's cleanup warnings are the ONLY record of a secondary failure that
// the reply cannot carry: the deploy already has an error to return, so a release
// or a RESET that fails on the way out is reported nowhere else. They reached
// nobody -- the events were emitted, no subscriber was ever installed, and every
// one of them was discarded before it was formatted.
//
// The failure here is provoked, not simulated. A peer holds the project lock, the
// deploy blocks inside `pg_advisory_lock`, and its backend is terminated from
// another session. The acquisition fails, and the compensating
// `pg_advisory_unlock` that `drop_grant_from_failed_acquire` runs to drop a grant
// the server may still have recorded fails too, because it runs on the connection
// that just died. That is the exact secondary failure the warning exists for, on a
// real connection, reached through the shipped CLI.
//
// STDERR, not stdout: `lint`/`plan`/`status`/`history` write a single JSON
// document to stdout under `--json` and callers parse it, so diagnostics on that
// stream would corrupt the reply.
test("ZERO_MIGRATE_LOG shows a real cleanup failure on stderr", async (t) => {
  const holder = await connectLivePg(t);
  if (holder === null) return;
  const cwd = temporaryDirectory(".cli-log-cleanup-");
  const schema = `zm_log_cleanup_${Date.now().toString(36)}`;
  let held = false;
  try {
    writeSimpleMigration(cwd);
    const policyPath = join(cwd, "policy.toml");
    writeFileSync(policyPath, noInjectPolicy(schema));

    await holder.query("SELECT pg_advisory_lock(hashtext($1)::bigint)", [schema]);
    held = true;

    const common = [
      `--dir=${cwd}`,
      `--policy=${policyPath}`,
      `--database-url=${pgUrl()}`,
      `--schema=${schema}`,
      "--approve",
    ];

    // Opted in: the operator asked for diagnostics and gets the cleanup failure.
    const logged = spawnCliAsync(["apply", ...common], {
      env: { ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, ZERO_MIGRATE_LOG: "1" },
    });
    await killAdvisoryLockWaiter(holder);
    const loggedRun = await logged;

    assert.equal(loggedRun.status, 1, loggedRun.stderr);
    assert.match(
      loggedRun.stderr,
      /failed to drop a possible advisory-lock grant after a failed project-lock acquisition/,
      "the opted-in operator must see the cleanup failure the reply cannot carry",
    );
    assert.doesNotMatch(
      loggedRun.stdout,
      /advisory-lock grant/,
      "diagnostics must never reach the stream the JSON replies own",
    );
    assert.doesNotMatch(
      loggedRun.stderr,
      /\u001b\[/u,
      "a piped stderr must carry no ANSI escapes",
    );

    // Default off: the identical run, with the variable unset, says nothing extra.
    const quiet = spawnCliAsync(["apply", ...common], {
      env: { ZERO_MIGRATE_ADDON_PATH: ADDON_PATH },
    });
    await killAdvisoryLockWaiter(holder);
    const quietRun = await quiet;

    assert.equal(quietRun.status, 1, quietRun.stderr);
    assert.doesNotMatch(
      quietRun.stderr,
      /advisory-lock grant/,
      "diagnostics are opt-in; an unset variable must change nothing an operator sees",
    );

    // Both runs failed for the SAME reason, so the arms above compare like with
    // like: the only difference between them is the opt-in.
    for (const run of [loggedRun, quietRun]) {
      assert.match(run.stderr, /terminat/i, run.stderr);
    }

    // A verb whose stdout is a machine-readable reply still emits exactly one JSON
    // document with the variable set.
    await holder.query("SELECT pg_advisory_unlock(hashtext($1)::bigint)", [schema]);
    held = false;
    const json = spawnCli(
      [
        "status",
        "--strict",
        "--json",
        `--dir=${cwd}`,
        `--policy=${policyPath}`,
        `--database-url=${pgUrl()}`,
        `--schema=${schema}`,
      ],
      {
        env: { ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, ZERO_MIGRATE_LOG: "1" },
        timeout: 60_000,
      },
    );
    assert.equal(json.status, 1, json.stderr);
    const reply = JSON.parse(json.stdout) as StatusReply;
    assert.equal(reply.pending.length, 1);
  } finally {
    if (held) {
      await holder
        .query("SELECT pg_advisory_unlock(hashtext($1)::bigint)", [schema])
        .catch(() => {});
    }
    await holder
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
      )
      .catch(() => {});
    await holder.end().catch(() => {});
    rmSync(cwd, { recursive: true, force: true });
  }
});

// The same defect on MySQL, where it is slower to arrive rather than absent.
// `GET_LOCK(name, 10)` bounds the wait, so a reader never hung -- it spent ten
// seconds and then reported the timeout as an ERROR, and every runtime error exits
// 1. `status --strict` therefore still failed a healthy build while a peer
// deployed, which is the same false red on the same documented CI gate.
//
// The lock is held from a SECOND LIVE MySQL session for the duration of both runs.
// A mock that returns contention would prove the plumbing already shipped for
// PostgreSQL; only a real `GET_LOCK` holder proves that the MySQL acquisition
// itself stopped waiting and stopped erroring.
//
// `resolve` has no arm here: it refuses any non-PostgreSQL driver before it reads,
// so there is no MySQL busy path to assert.
test("MySQL: CLI status and plan answer while a peer holds the project lock", async (t) => {
  if (!MYSQL_URL) {
    if (liveDbRequired()) {
      throw new Error(
        `${REQUIRE_LIVE_DB_ENV} demands a live database but ZERO_MIGRATE_MYSQL_URL is unset, ` +
          "so this run has no live MySQL project-lock coverage to offer",
      );
    }
    t.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL project-lock contention arm skipped");
    return;
  }

  const mysql = (await import("mysql2/promise")).default;
  const holder = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  const cwd = temporaryDirectory(".cli-lock-busy-mysql-");
  // MySQL identifiers cap at 64 characters and the journal database appends
  // `_migrations`, so the generated name has to stay well inside the cap.
  const schema = `zm_lock_busy_${Date.now().toString(36)}`;
  // The MySQL project lock is a NAMED user-level lock, not a hashed key: the name
  // is `zero_migrate:<project id>` and the CLI's project id is the project schema,
  // so this is the exact name the reader will try to take.
  const lockName = `zero_migrate:${schema}`;
  let held = false;
  try {
    await holder.query(`CREATE DATABASE \`${schema}\``);
    writeSimpleMigration(cwd);
    const policyPath = join(cwd, "policy.toml");
    writeFileSync(policyPath, noInjectPolicy(schema));

    const [lockRows] = await holder.query("SELECT GET_LOCK(?, 0) AS got", [lockName]);
    assert.equal(
      Number((lockRows as Array<Record<string, unknown>>)[0].got),
      1,
      "the peer session must actually hold the project lock",
    );
    held = true;
    const [idRows] = await holder.query("SELECT CONNECTION_ID() AS id");
    const holderId = Number((idRows as Array<Record<string, unknown>>)[0].id);

    const common = [
      `--dir=${cwd}`,
      `--policy=${policyPath}`,
      `--database-url=${MYSQL_URL}`,
      `--schema=${schema}`,
    ];
    const options = { env: { ZERO_MIGRATE_ADDON_PATH: ADDON_PATH }, timeout: 60_000 };

    const status = spawnCli(["status", "--strict", ...common], options);
    assert.equal(
      status.signal,
      null,
      `status blocked on the peer's project lock instead of reporting it: ${status.stderr}`,
    );
    assert.equal(status.status, 0, status.stderr);
    assert.match(status.stderr, /project lock/i);
    assert.match(status.stderr, new RegExp(`\\b${holderId}\\b`));
    // The bounded ten-second GET_LOCK is what used to turn contention into an
    // error, so its message must be gone rather than merely outranked.
    assert.doesNotMatch(status.stderr, /timed out after/);

    const plan = spawnCli(["plan", ...common], options);
    assert.equal(
      plan.signal,
      null,
      `plan blocked on the peer's project lock instead of reporting it: ${plan.stderr}`,
    );
    assert.equal(plan.status, 0, plan.stderr);
    assert.match(plan.stderr, /project lock/i);
    assert.doesNotMatch(plan.stdout, /would apply|CREATE TABLE/i);

    const json = spawnCli(["status", "--strict", "--json", ...common], options);
    assert.equal(json.signal, null, json.stderr);
    assert.equal(json.status, 0, json.stderr);
    const reply = JSON.parse(json.stdout) as StatusReply;
    assert.equal(reply.busy, true);
    assert.deepEqual(
      reply.lockHolders.map((entry) => entry.pid),
      [holderId],
    );
    // The holder is a connection that took the lock and went quiet, which is what
    // an operator sees while a peer's deploy waits on a long DDL: MySQL reports
    // PROCESSLIST_INFO as NULL for it. The reply says so through the command
    // (`Sleep`) instead of carrying an empty statement the message would print as
    // a bare trailing colon.
    assert.equal(reply.lockHolders[0].query ?? null, null);
    assert.match(reply.lockHolders[0].state ?? "", /Sleep/);
    assert.doesNotMatch(status.stderr, /\)\s*:\s*$/m);

    // Positive control: the same commands against the same database with the lock
    // free still take the lock, still read, and still return their real verdict --
    // exit 1 for the pending migration this directory carries. Without it the arms
    // above would also pass if the verbs had simply stopped doing any work.
    await holder.query("SELECT RELEASE_LOCK(?)", [lockName]);
    held = false;

    const uncontended = spawnCli(["status", "--strict", ...common], options);
    assert.equal(uncontended.signal, null, uncontended.stderr);
    assert.equal(uncontended.status, 1, uncontended.stderr);
    assert.match(uncontended.stdout, /status: 0 applied, 1 pending/);
    assert.doesNotMatch(uncontended.stderr, /project lock/i);

    const uncontendedPlan = spawnCli(["plan", ...common], options);
    assert.equal(uncontendedPlan.status, 0, uncontendedPlan.stderr);
    assert.match(uncontendedPlan.stdout, /would apply 1 migration/);
    assert.match(uncontendedPlan.stdout, /CREATE TABLE/i);
  } finally {
    if (held) {
      await holder.query("SELECT RELEASE_LOCK(?)", [lockName]).catch(() => {});
    }
    await holder
      .query(
        `DROP DATABASE IF EXISTS \`${schema}\`;
         DROP DATABASE IF EXISTS \`${schema}_migrations\``,
      )
      .catch(() => {});
    await holder.end().catch(() => {});
    rmSync(cwd, { recursive: true, force: true });
  }
});

// The busy reply's presentation, kept away from a database so every branch is
// covered: with a holder, with the holder's statement hidden (the reading role may
// not see other sessions' query text), and with no holder at all.
//
// `statusIsDirty`/`statusExitCode` stay pure over a real reply, so the arm that
// matters is that they are never the thing deciding a busy run: the reply carries
// no reconciled state, so "not dirty" is not a verdict, it is the absence of one.
test("a busy status reply reports the holder and never renders counts", () => {
  const busy = makeStatus({
    busy: true,
    lockHolders: [
      {
        pid: 4242,
        applicationName: "zero-migrate",
        state: "active",
        query: "CREATE INDEX CONCURRENTLY ix_widgets_name ON widgets (name)",
      },
    ],
  });

  const message = formatStatusBusy(busy, "status");
  assert.match(message, /another deploy holds the project lock/);
  assert.match(message, /status read nothing and did not wait for it/);
  assert.match(message, /held by pid 4242 \(zero-migrate, active\)/);
  assert.match(message, /CREATE INDEX CONCURRENTLY ix_widgets_name/);
  // No duration: pg_locks records no acquisition time, so any age reported here
  // would be the holder's session or statement, not the lock's.
  assert.doesNotMatch(message, /held for|\d+\s*(ms|s|seconds|minutes)\b/);
  assert.equal(formatStatusBusy(busy, "plan").includes("plan read nothing"), true);

  // Never the counts: "0 applied, 0 pending" for a database nobody read is the
  // exact confusion this reply exists to prevent.
  assert.equal(formatStatusHuman(busy), message);
  assert.doesNotMatch(formatStatusHuman(busy), /0 applied, 0 pending/);

  // The JSON reply stays the machine-readable contract a CI opts into.
  const json = JSON.parse(formatStatusJson(busy)) as StatusReply;
  assert.equal(json.busy, true);
  assert.equal(json.lockHolders[0].pid, 4242);

  const anonymous = makeStatus({ busy: true, lockHolders: [{ pid: 77 }] });
  assert.match(formatStatusBusy(anonymous, "status"), /held by pid 77\n/);

  const unknown = makeStatus({ busy: true });
  assert.match(
    formatStatusBusy(unknown, "status"),
    /the holding session could not be identified/,
  );

  // Contention is not dirt: a clean reply and a busy reply agree here, which is
  // why the busy branch has to run BEFORE the exit-code rule rather than lean on
  // it.
  assert.equal(statusIsDirty(busy), false);
  assert.equal(statusExitCode(busy, true), 0);
});

// The identity of the LOADED addon, which `version` alone cannot report. A stale
// prebuilt `.node` has caused real incidents here, and `buildInfo().sourceDigest` is
// the value that separates the source in the tree from the binary in memory. The
// default output stays a bare scalar because `$(zero-migrate version)` captures the
// whole of stdout, so anything appended below it is a breaking change to every
// caller that substitutes the command.
test("version reports only the package version, and the addon identity on request", () => {
  const bare = runCli("version");
  assert.equal(bare.status, 0, `version exit: ${bare.stderr}`);
  assert.match(
    bare.stdout,
    /^\d+\.\d+\.\d+[^\n]*\n$/,
    `version must print one bare scalar and nothing else, got ${JSON.stringify(bare.stdout)}`,
  );
  const cliVersion = bare.stdout.trim();

  // `--json` is parsed globally and never validated against the verb, so
  // `version --json` already succeeds today and prints the bare scalar. Pinned so
  // that teaching it to emit a document is a deliberate change, not a side effect.
  const bareJson = runCli("version", "--json");
  assert.equal(bareJson.status, 0, `version --json exit: ${bareJson.stderr}`);
  assert.equal(
    bareJson.stdout,
    bare.stdout,
    "version --json must stay byte-identical to version until a verb opts in",
  );

  const verbose = runCliWithEnv({ ZERO_MIGRATE_ADDON_PATH: ADDON_PATH }, "version", "--verbose");
  assert.equal(verbose.status, 0, `version --verbose exit: ${verbose.stderr}`);
  assert.match(
    verbose.stdout,
    new RegExp(`\\b${cliVersion.replace(/\./g, "\\.")}\\b`),
    `version --verbose must still name the CLI version, got ${JSON.stringify(verbose.stdout)}`,
  );
  assert.match(
    verbose.stdout,
    /[0-9a-f]{64}/,
    `version --verbose must report the addon source digest, got ${JSON.stringify(verbose.stdout)}`,
  );

  const asJson = runCliWithEnv(
    { ZERO_MIGRATE_ADDON_PATH: ADDON_PATH },
    "version",
    "--verbose",
    "--json",
  );
  assert.equal(asJson.status, 0, `version --verbose --json exit: ${asJson.stderr}`);
  const doc = JSON.parse(asJson.stdout) as {
    cliVersion?: string;
    addon?: { version?: string; irVersion?: number; sourceDigest?: string };
  };
  assert.equal(doc.cliVersion, cliVersion, "cliVersion must match the bare output");
  assert.equal(typeof doc.addon?.version, "string", `addon.version: ${asJson.stdout}`);
  assert.equal(typeof doc.addon?.irVersion, "number", `addon.irVersion: ${asJson.stdout}`);
  assert.match(
    doc.addon?.sourceDigest ?? "",
    /^[0-9a-f]{64}$/,
    `addon.sourceDigest must be a lowercase sha256: ${asJson.stdout}`,
  );
});
