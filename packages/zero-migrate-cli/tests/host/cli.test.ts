import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { driverFor } from "../../src/cli.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

function runCli(...args: string[]) {
  return spawnSync(process.execPath, ["--import", "tsx", CLI_BIN, ...args], {
    encoding: "utf8",
    env: { ...process.env, DATABASE_URL: "" },
  });
}

function runCliWithEnv(env: NodeJS.ProcessEnv, ...args: string[]) {
  return spawnSync(process.execPath, ["--import", "tsx", CLI_BIN, ...args], {
    encoding: "utf8",
    env: { ...process.env, ...env },
  });
}

test("CLI valueless flags reject supplied values", () => {
  for (const invocation of [
    ["apply", "--approve=false"],
    ["apply", "--approve", "false"],
    ["resolve-pending", "mig_0000000000000000000001", "--apply=false"],
    ["resolve-pending", "mig_0000000000000000000001", "--abort=true"],
    ["help", "--json=true"],
    ["help", "--help=true"],
  ]) {
    const result = runCli(...invocation);
    assert.equal(result.status, 1, `${invocation.join(" ")} must fail`);
    assert.match(
      result.stderr,
      /flag --(?:approve|apply|abort|json|help) does not take a value|does not accept positional arguments/,
    );
  }
});

test("CLI resolve-pending requires one action, approval, and PostgreSQL", () => {
  const pending = "mig_0000000000000000000001";
  const missingAction = runCli(
    "resolve-pending",
    pending,
    "--approve",
    "--database-url=postgres://127.0.0.1:1/never_connect",
  );
  assert.equal(missingAction.status, 1);
  assert.match(missingAction.stderr, /choose exactly one of --apply or --abort/);

  const bothActions = runCli(
    "resolve-pending",
    pending,
    "--apply",
    "--abort",
    "--approve",
    "--database-url=postgres://127.0.0.1:1/never_connect",
  );
  assert.equal(bothActions.status, 1);
  assert.match(bothActions.stderr, /choose exactly one of --apply or --abort/);

  const missingApproval = runCli(
    "resolve-pending",
    pending,
    "--apply",
    "--database-url=postgres://127.0.0.1:1/never_connect",
  );
  assert.equal(missingApproval.status, 1);
  assert.match(missingApproval.stderr, /requires --approve/);

  const mysql = runCli(
    "resolve-pending",
    pending,
    "--apply",
    "--approve",
    "--database-url=mysql://127.0.0.1:1/never_connect",
  );
  assert.equal(mysql.status, 1);
  assert.match(mysql.stderr, /only PostgreSQL online renames/);
  assert.doesNotMatch(mysql.stderr, /ECONNREFUSED/);
});

test("CLI rejects the unsupported mariadb URL scheme", () => {
  const result = runCli(
    "apply",
    "--database-url=mariadb://private-user:secret-password@localhost/app.db",
  );
  assert.equal(result.status, 1);
  assert.match(result.stderr, /could not infer a driver/);
  assert.match(result.stderr, /expected a postgres:\/\/ or mysql:\/\/ scheme/);
  assert.doesNotMatch(result.stderr, /private-user|secret-password|localhost\/app/);
});

test("CLI never prints an unsupported database URL", () => {
  const secretUrl = "unknown://private-user:secret-password@localhost/app";
  const result = runCli("status", `--database-url=${secretUrl}`);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /could not infer a driver/);
  assert.equal(result.stderr.includes(secretUrl), false);
  assert.doesNotMatch(result.stderr, /private-user|secret-password/);
});

test("CLI rejects an explicitly empty database URL instead of using DATABASE_URL", () => {
  const result = runCliWithEnv(
    { DATABASE_URL: "postgres://production.example/app" },
    "apply",
    "--database-url=",
  );
  assert.equal(result.status, 1);
  assert.match(result.stderr, /missing database URL/);
  assert.doesNotMatch(result.stderr, /migrations dir|production\.example/);
});

test("CLI honors --help before validating subcommand positionals", () => {
  for (const invocation of [
    ["plan", "--help"],
    ["preview", "--help"],
    ["apply", "--help"],
    ["status", "--help"],
    ["new", "demo", "--help"],
  ]) {
    const result = runCli(...invocation);
    assert.equal(result.status, 0, `${invocation.join(" ")} must show help`);
    assert.match(result.stdout, /^zero-migrate: database migrations from JavaScript/);
    assert.equal(result.stderr, "");
  }
});

test("CLI help advertises SQLite apply in user-facing language", () => {
  const help = runCli("--help");
  assert.equal(help.status, 0);
  assert.match(help.stdout, /--dialect <name>/);
  assert.match(help.stdout, /--registry <file>/);
  assert.match(help.stdout, /--policy <file>/);
  assert.match(help.stdout, /history .*--policy <file>/);
  assert.match(help.stdout, /resolve-pending .*--policy <file>/);
  assert.match(help.stdout, /Repeatable ordered TOML policy layer; first is the root\/bound/);
  assert.match(help.stdout, /only root may use mandatory injects/);
  assert.match(help.stdout, /--journal <path>/);
  assert.match(help.stdout, /apply supports\s+PostgreSQL, MySQL 8, and SQLite/);
  assert.doesNotMatch(help.stdout, /\u2014|host driver seam|addon|in-process/);

  const sqlite = runCli("apply", "--database-url=sqlite:///tmp/app.db");
  assert.equal(sqlite.status, 1);
  assert.match(sqlite.stderr, /missing policy/);
  assert.doesNotMatch(sqlite.stderr, /SQLite is not supported/);
  assert.doesNotMatch(sqlite.stderr, /\u2014|host driver seam|addon|in-process/);
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

test("CLI database verbs require an explicit policy file", () => {
  const invocations = [
    ["apply"],
    ["status"],
    ["history"],
    ["resolve-pending", "mig_0000000000000000000001", "--apply", "--approve"],
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

test("CLI rejects an empty policy document before opening a database session", () => {
  const dir = mkdtempSync(join(HERE, ".cli-policy-"));
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
  const dir = mkdtempSync(join(HERE, ".cli-policy-layers-"));
  try {
    const missingRootPath = join(dir, "missing-root.toml");
    const laterLayerPath = join(dir, "later-layer.toml");
    writeFileSync(laterLayerPath, "policy_version = 1\n");

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

test("CLI plan validates the selected database dialect", () => {
  const dir = mkdtempSync(join(HERE, ".cli-dialect-"));
  try {
    writeFileSync(
      join(dir, "20260715000000_virtual_column.mjs"),
      `import { table, t } from "zero-migrate";
export function up() {
  table("products").create({
    columns: {
      base: t.int(),
      computed: t.int().generated((col) => col("base").mul(2), { virtual: true }),
    },
  });
}
`,
    );
    const env = { DATABASE_URL: "", ZERO_MIGRATE_ADDON_PATH: ADDON_PATH };
    const postgres = runCliWithEnv(env, "plan", `--dir=${dir}`, "--dialect=postgres");
    const mysql = runCliWithEnv(env, "plan", `--dir=${dir}`, "--dialect=mysql");

    assert.equal(postgres.status, 1);
    assert.match(postgres.stdout, /ERROR/);
    assert.equal(mysql.status, 0, mysql.stderr || mysql.stdout);
    assert.match(mysql.stdout, /plan .*: ok/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("CLI plan accepts a trusted ownership registry file", () => {
  const dir = mkdtempSync(join(HERE, ".cli-registry-"));
  try {
    writeFileSync(
      join(dir, "20260715000000_create_users.mjs"),
      `import { table, t } from "zero-migrate";
export function up() {
  table("users").create({ columns: { id: t.int() } });
}
`,
    );
    writeFileSync(
      join(dir, "20260715000001_add_timezone.mjs"),
      `import { table, t } from "zero-migrate";
export function up() {
  table("users").column("timezone").add({ type: t.text() });
}
`,
    );
    const registryPath = join(dir, "registry.json");
    writeFileSync(registryPath, JSON.stringify({ users: "app_cli" }));
    const env = { DATABASE_URL: "", ZERO_MIGRATE_ADDON_PATH: ADDON_PATH };
    const withoutRegistry = runCliWithEnv(env, "plan", `--dir=${dir}`);
    const withRegistry = runCliWithEnv(
      env,
      "plan",
      `--dir=${dir}`,
      `--registry=${registryPath}`,
    );

    assert.equal(withoutRegistry.status, 1);
    assert.match(withoutRegistry.stdout, /unregistered/i);
    assert.equal(withRegistry.status, 0, withRegistry.stderr || withRegistry.stdout);
    assert.match(withRegistry.stdout, /plan .*: ok/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("CLI plan and apply reject duplicate resolved migration names before applying", () => {
  const dir = mkdtempSync(join(HERE, ".cli-duplicate-names-"));
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
    const env = { DATABASE_URL: "", ZERO_MIGRATE_ADDON_PATH: ADDON_PATH };
    const policyPath = join(dir, "policy.toml");
    writeFileSync(policyPath, "policy_version = 1\n");

    const planned = runCliWithEnv(env, "plan", `--dir=${dir}`);
    assert.equal(planned.status, 1);
    assert.match(planned.stderr, /duplicate migration name.*shared_identity/i);
    assert.match(planned.stderr, /20260715000000_first/);
    assert.match(planned.stderr, /20260715000001_second/);

    // Port 1 is intentionally unreachable. Seeing the identity error instead of a
    // connection error proves every name was checked before the first apply call.
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

test("CLI plan applies --schema confinement before reporting a migration as valid", () => {
  const dir = mkdtempSync(join(HERE, ".cli-schema-"));
  try {
    writeFileSync(
      join(dir, "20260715000000_foreign_schema.mjs"),
      `import { table, t } from "zero-migrate";
export function up() {
  table("widgets", { schema: "outside_project" }).create({
    columns: { id: t.int() },
  });
}
`,
    );
    const result = runCliWithEnv(
      { DATABASE_URL: "", ZERO_MIGRATE_ADDON_PATH: ADDON_PATH },
      "plan",
      `--dir=${dir}`,
      "--schema=app_data",
    );

    assert.equal(result.status, 1);
    assert.match(result.stdout, /ERROR/);
    assert.match(result.stdout, /outside_project|cross.schema/i);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
