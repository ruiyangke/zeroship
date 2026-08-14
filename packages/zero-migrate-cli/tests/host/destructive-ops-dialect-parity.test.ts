// `safety.destructive_ops` must mean the same thing on every target.
//
// The posture defaults to `forbid` and is registered `Enforcement::Enforced`,
// which `policy_registry.rs` defines as "a guard, executor or validator path reads
// them and they do what they say". That registry also carries a
// `DeclaredOnly` class for knobs nothing implements, whose non-default values are
// refused AT LOAD, precisely so an operator cannot "hold a sealed policy
// advertising a control that never runs".
//
// It was PostgreSQL-only. The posture was read by the PG SQL-text guard, and
// `guard_for` hands MySQL and SQLite a descriptor guard constructed WITHOUT the
// policy, so it could not see the knob at all. Measured before this file existed:
// PostgreSQL refused a `DROP TABLE` under the default posture, MySQL and SQLite
// applied it. `Enforcement` has no dialect dimension, so the load-time refusal
// could not fire either - the operator got silence instead of protection.
//
// Sharing the descriptor guard is right for the TEXT rules: the deny-list scans
// raw SQL for file, program, privilege and session behaviour, and neither MySQL
// nor SQLite has a raw door (`SqlGuard::check` refuses non-Postgres outright with
// `SqliteRawSqlRejected` / `MysqlRawSqlRejected`). Those rules have no surface
// there. The destructive posture is different in kind: it is a property of the
// OPERATION, not of any SQL text, so it belongs where every dialect can see it.
//
// THREE ARMS PER DIALECT, and only the first is about the refusal:
//
//   1. default posture (the policy never mentions the knob) + a drop -> REFUSED,
//      and the table is still there afterwards;
//   1b. default posture + a WHERE-bounded row update -> APPLIED. PostgreSQL applies
//      it, so anything stricter is a regression rather than parity. The first
//      version of this fix reused `Op::is_destructive` - the APPROVAL notion,
//      which includes DML - and made two dialects refuse what PostgreSQL allows;
//   2. `safety.destructive_ops = "allow"` + the same drop -> APPLIED.
//
// Without arm 2 every arm 1 would pass on a build where drops never work at all,
// which is the failure mode a parity fix is most likely to introduce; without arm
// 1b it would pass on one that refuses far too much.
//
// GATES: `ZERO_MIGRATE_TEST_PG_URL`, `ZERO_MIGRATE_MYSQL_URL`. SQLite needs no
// server and always runs.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const PG_URL = process.env.ZERO_MIGRATE_TEST_PG_URL;
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const OWNER_APP = "app_destructive_parity";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** `posture` omitted means the policy never mentions the knob, so the registry
 *  default (`forbid`) applies - the case an operator gets by writing nothing. */
function policy(namespace: string | null, posture: "allow" | null): string {
  const scope = namespace ? `{ include = [${JSON.stringify(namespace)}] }` : `"all"`;
  const crossSchema = namespace
    ? `[[grant]]
key = "schema.cross_schema"
value = true
scope = ${scope}

`
    : "";
  const destructive = posture
    ? `
[[grant]]
key = "safety.destructive_ops"
value = "${posture}"
scope = "all"
`
    : "";
  return `policy_version = 1

${crossSchema}[[grant]]
key = "schema.create_table"
value = true
scope = "all"
${destructive}`;
}

function project(namespace: string | null, posture: "allow" | null): string {
  const work = mkdtempSync(join(HERE, "destparity-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(join(work, "policy.toml"), policy(namespace, posture));
  writeFileSync(join(work, "registry.json"), JSON.stringify({ doomed: OWNER_APP }));
  writeFileSync(
    join(work, "migrations", "20260101000000_make.ts"),
    `import { table, t } from "zero-migrate";
export const name = "make_doomed";
export default {
  schema() {
    table("doomed").create({ columns: { id: t.int().notNull(), val: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`,
  );
  return work;
}

/** A WHERE-bounded row update. PostgreSQL APPLIES this under the default forbid
 *  posture, because DML lowers to a bound `PlanStep::Dml` its SQL-text guard never
 *  inspects. Any IR-side enforcement must match that, or the dialects it covers end
 *  up STRICTER than PostgreSQL. */
function addUpdate(work: string): void {
  writeFileSync(
    join(work, "migrations", "20260102000000_bump.ts"),
    `import { table } from "zero-migrate";
export const name = "bump_rows";
export default {
  data() {
    table("doomed").update({
      set: { val: (col) => col("val").add(1) },
      where: (col) => col("id").gt(0),
    });
  },
  inverse() {
    table("doomed").update({
      set: { val: (col) => col("val").sub(1) },
      where: (col) => col("id").gt(0),
    });
  },
};
`,
  );
}

function addDrop(work: string): void {
  writeFileSync(
    join(work, "migrations", "20260102000000_drop.ts"),
    `import { table } from "zero-migrate";
export const name = "drop_doomed";
export default { schema() { table("doomed").drop(); } };
`,
  );
}

function cli(
  work: string,
  databaseUrl: string,
  namespace: string | null,
): Promise<{ code: number | null; text: string }> {
  const schemaArgs = namespace ? ["--schema", namespace] : [];
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, "apply", "--approve",
        "--dir", join(work, "migrations"),
        "--database-url", databaseUrl,
        "--policy", join(work, "policy.toml"),
        "--registry", join(work, "registry.json"),
        ...schemaArgs,
        "--owner-app", OWNER_APP,
      ],
      {
        cwd: work,
        env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
      },
    );
    let out = "";
    let err = "";
    child.stdout.on("data", (chunk) => (out += chunk));
    child.stderr.on("data", (chunk) => (err += chunk));
    child.on("close", (code) =>
      resolvePromise({
        code,
        text: `${out}\n${err}`.replace(/^WARNING.*$/gm, "").trim(),
      }),
    );
  });
}

interface Target {
  readonly name: string;
  /** Returns the database URL to use, after creating any namespace it needs. */
  open(posture: "allow" | null): Promise<{ url: string; namespace: string | null; work: string }>;
  exists(namespace: string | null, work: string): Promise<boolean>;
  cleanUp(namespace: string | null, work: string): Promise<void>;
}

const postgresTarget: Target = {
  name: "PostgreSQL",
  async open(posture) {
    const namespace = uniqueNamespace("destparity_pg");
    const pg = await import("pg");
    const client = new pg.Client({ connectionString: pgUrl() });
    await client.connect();
    await client.query(`CREATE SCHEMA "${namespace}"`);
    await client.end();
    return { url: pgUrl(), namespace, work: project(namespace, posture) };
  },
  async exists(namespace) {
    const pg = await import("pg");
    const client = new pg.Client({ connectionString: pgUrl() });
    await client.connect();
    const { rows } = await client.query(
      `SELECT count(*)::int AS n FROM information_schema.tables
        WHERE table_schema = $1 AND table_name = 'doomed'`,
      [namespace],
    );
    await client.end();
    return rows[0].n === 1;
  },
  async cleanUp(namespace, work) {
    const pg = await import("pg");
    const client = new pg.Client({ connectionString: pgUrl() });
    await client.connect();
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${namespace}" CASCADE;
         DROP SCHEMA IF EXISTS "${namespace}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  },
};

const mysqlTarget: Target = {
  name: "MySQL",
  async open(posture) {
    const namespace = uniqueNamespace("destparity_my");
    const driver = (await import("mysql2/promise")).default;
    const connection = await driver.createConnection({ uri: String(MYSQL_URL) });
    await connection.query(`CREATE DATABASE \`${namespace}\``);
    await connection.end();
    const base = String(MYSQL_URL).replace(/\/[^/]*$/, "");
    return { url: `${base}/${namespace}`, namespace, work: project(namespace, posture) };
  },
  async exists(namespace) {
    const driver = (await import("mysql2/promise")).default;
    const connection = await driver.createConnection({ uri: String(MYSQL_URL) });
    const [rows] = await connection.query(
      `SELECT COUNT(*) AS n FROM information_schema.TABLES
        WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'doomed'`,
      [namespace],
    );
    await connection.end();
    return Number((rows as Array<{ n: number }>)[0].n) === 1;
  },
  async cleanUp(namespace, work) {
    const driver = (await import("mysql2/promise")).default;
    const connection = await driver.createConnection({ uri: String(MYSQL_URL) });
    await connection.query(`DROP DATABASE IF EXISTS \`${namespace}\``).catch(() => {});
    await connection.query(`DROP DATABASE IF EXISTS \`${namespace}_migrations\``).catch(() => {});
    await connection.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  },
};

const sqliteTarget: Target = {
  name: "SQLite",
  async open(posture) {
    const work = project(null, posture);
    return { url: `sqlite:${join(work, "app.db")}`, namespace: null, work };
  },
  async exists(_namespace, work) {
    const { DatabaseSync } = await import("node:sqlite");
    const db = new DatabaseSync(join(work, "app.db"), { readOnly: true });
    const row = db
      .prepare("SELECT count(*) AS c FROM sqlite_master WHERE type='table' AND name='doomed'")
      .get() as { c: number };
    db.close();
    return Number(row.c) === 1;
  },
  async cleanUp(_namespace, work) {
    rmSync(work, { recursive: true, force: true });
  },
};

async function runParity(target: Target): Promise<void> {
  // ARM 1 — the default posture must refuse the drop.
  {
    const { url, namespace, work } = await target.open(null);
    try {
      const made = await cli(work, url, namespace);
      assert.equal(made.code, 0, `${target.name}: the table must be created; ${made.text}`);

      addDrop(work);
      const dropped = await cli(work, url, namespace);
      assert.equal(
        dropped.code,
        1,
        `${target.name}: the default forbid posture must refuse a drop; ${dropped.text}`,
      );
      // By CONTENT: an unrelated failure also exits 1, and the whole point is that
      // this particular policy is what stopped it.
      assert.match(
        dropped.text,
        /destructive/i,
        `${target.name}: the refusal must name the destructive posture: ${dropped.text}`,
      );
      assert.equal(
        await target.exists(namespace, work),
        true,
        `${target.name}: the refused drop must leave the table in place`,
      );
    } finally {
      await target.cleanUp(namespace, work);
    }
  }

  // ARM 1b, the OVER-BLOCK regression arm. The first version of the parity fix
  // reused `Op::is_destructive`, which is the APPROVAL notion and includes row
  // DML. That made MySQL and SQLite refuse an `update` PostgreSQL applies — not
  // parity but a stricter dialect, and the kind of regression a "make it enforce
  // everywhere" change invites. This arm fails if the posture ever widens past
  // what PostgreSQL denies.
  {
    const { url, namespace, work } = await target.open(null);
    try {
      const made = await cli(work, url, namespace);
      assert.equal(made.code, 0, `${target.name}: the table must be created; ${made.text}`);

      addUpdate(work);
      const bumped = await cli(work, url, namespace);
      assert.equal(
        bumped.code,
        0,
        `${target.name}: a bounded update must APPLY under the default posture, as it ` +
          `does on PostgreSQL — the posture covers object drops, not row DML; ${bumped.text}`,
      );
    } finally {
      await target.cleanUp(namespace, work);
    }
  }

  // ARM 2, the control — `allow` must let the very same drop through, so arm 1 is
  // about the POSTURE and not about drops being broken on this dialect.
  {
    const { url, namespace, work } = await target.open("allow");
    try {
      const made = await cli(work, url, namespace);
      assert.equal(made.code, 0, `${target.name}: the table must be created; ${made.text}`);

      addDrop(work);
      const dropped = await cli(work, url, namespace);
      assert.equal(
        dropped.code,
        0,
        `${target.name}: an explicit allow must apply the drop; ${dropped.text}`,
      );
      assert.equal(
        await target.exists(namespace, work),
        false,
        `${target.name}: and the table must actually be gone`,
      );
    } finally {
      await target.cleanUp(namespace, work);
    }
  }
}

test("PostgreSQL enforces the destructive posture", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; PG destructive-parity arm skipped");
    return;
  }
  await runParity(postgresTarget);
});

test("MySQL enforces the destructive posture", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL destructive-parity arm skipped");
    return;
  }
  await runParity(mysqlTarget);
});

test("SQLite enforces the destructive posture", async () => {
  await runParity(sqliteTarget);
});
