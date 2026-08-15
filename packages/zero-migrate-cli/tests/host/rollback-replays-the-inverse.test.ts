// Rolling back a data migration runs the inverse its author recorded.
//
// `inverse()` crosses the IR boundary as `inverse_ops`, is validated and lowered
// through the same parameterized DML path as the forward operation list, and is
// executed as one rollback unit under the forward journal identity.
//
// THE ASSERTION IS THE ROW, not the outcome object. A rollback that reported
// success while leaving the row in place would satisfy any check on
// `rolledBack.length`, and the whole point of an authored inverse is what
// happens to the data.
//
// The control matters as much: a data migration declaring `irreversible` must
// STILL be refused. If this change made every data migration reversible, the
// refusal that protects an operator from a fabricated reverse would be gone, and
// these two arms are the only thing that tells the difference.
//
// GATES: `connectLivePg` (see `live-db.ts`) and `ZERO_MIGRATE_MYSQL_URL`.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { table, t } from "zero-migrate";
import { apply, rollback, status, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";
import { noInjectPolicy } from "./policy.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const OWNER_APP = "app_rollback_inverse";
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;

type NamedMigration = MigrationModule & { readonly name: string };

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function pgIdent(value: string): string {
  return `"${value.replaceAll('"', '""')}"`;
}

function mysqlIdent(value: string): string {
  return `\`${value.replaceAll("`", "``")}\``;
}

/** The table every arm seeds into. */
const created: NamedMigration = {
  name: "create_acct",
  default: {
    schema() {
      table("acct").create({
        columns: { id: t.int().notNull(), label: t.string({ length: 20 }) },
        primaryKey: ["id"],
      });
    },
  },
} as NamedMigration;

/** One row in, exactly reversible: the inverse deletes exactly that row. */
const seeded: NamedMigration = {
  name: "seed_acct",
  default: {
    data() {
      table("acct").insert({ rows: { id: 1, label: "seeded" } });
    },
    inverse() {
      table("acct").delete({ where: (col) => col("id").eq(1) });
    },
  },
} as NamedMigration;

/** The same shape, declaring it cannot be reversed. */
const unreversible: NamedMigration = {
  name: "seed_acct_unreversible",
  default: {
    data() {
      table("acct").insert({ rows: { id: 1, label: "seeded" } });
    },
    irreversible: "the source system no longer holds the pre-image",
  },
} as NamedMigration;

async function withSchema<T>(
  admin: { query: (text: string, values?: unknown[]) => Promise<{ rows: unknown[] }> },
  body: (projectSchema: string) => Promise<T>,
): Promise<T> {
  const projectSchema = uniqueNamespace("rbinv");
  try {
    await admin.query(`CREATE SCHEMA ${pgIdent(projectSchema)}`);
    return await body(projectSchema);
  } finally {
    await admin
      .query(
        `DROP SCHEMA IF EXISTS ${pgIdent(projectSchema)} CASCADE;
         DROP SCHEMA IF EXISTS ${pgIdent(`${projectSchema}_migrations`)} CASCADE`,
      )
      .catch(() => {});
  }
}

test("a recorded inverse is what a rollback runs", async (ctx) => {
  const admin = await connectLivePg(ctx);
  if (!admin) return;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  try {
    await withSchema(admin as never, async (projectSchema) => {
      const base = {
        ownerApp: OWNER_APP,
        projectSchema,
        driver,
        policy: [noInjectPolicy(projectSchema)],
        approved: true,
        appliedBy: "rollback-replays-the-inverse",
      };
      await apply({
        ...base,
        migration: created,
        priorMigrations: [],
        priorNameFallbacks: [],
        registry: {},
        nameFallback: created.name,
      });
      await apply({
        ...base,
        migration: seeded,
        priorMigrations: [created],
        priorNameFallbacks: [created.name],
        registry: { acct: OWNER_APP },
        nameFallback: seeded.name,
      });

      const rows = async (): Promise<unknown[]> =>
        (await admin.query(`SELECT id FROM ${pgIdent(projectSchema)}.acct ORDER BY id`)).rows;
      assert.equal((await rows()).length, 1, "the seed landed");

      const outcome = await rollback({
        migrations: [created, seeded],
        nameFallbacks: [created.name, seeded.name],
        ownerApp: OWNER_APP,
        projectSchema,
        driver,
        registry: { acct: OWNER_APP },
        policy: [noInjectPolicy(projectSchema)],
        target: { kind: "steps", steps: 1 },
        approved: true,
        backupAcknowledged: true,
        appliedBy: "rollback-replays-the-inverse",
      });

      assert.deepEqual(
        outcome.skippedIrreversible,
        [],
        "a data migration that RECORDED an exact inverse is not irreversible",
      );
      assert.deepEqual(
        await rows(),
        [],
        "the inverse deletes exactly the row the forward inserted; asserting the " +
          "ROW rather than the outcome is what separates a real unwind from a " +
          "rollback that reported success and did nothing",
      );

      const afterRollback = await status({
        migrations: [created, seeded],
        nameFallbacks: [created.name, seeded.name],
        ownerApp: OWNER_APP,
        projectSchema,
        driver,
        registry: { acct: OWNER_APP },
        policy: [noInjectPolicy(projectSchema)],
      });
      assert.equal(
        afterRollback.plans?.find((plan) => plan.name === seeded.name)?.state,
        "pending",
        "the rolled_back event makes status report the data plan pending",
      );

      await apply({
        ...base,
        migration: seeded,
        priorMigrations: [created],
        priorNameFallbacks: [created.name],
        registry: { acct: OWNER_APP },
        nameFallback: seeded.name,
      });
      assert.equal((await rows()).length, 1, "the rolled-back data plan can be re-applied");
    });
  } finally {
    await admin.end().catch(() => {});
  }
});

test("CONTROL: a data migration declaring irreversible is still refused", async (ctx) => {
  const admin = await connectLivePg(ctx);
  if (!admin) return;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  try {
    await withSchema(admin as never, async (projectSchema) => {
      const base = {
        ownerApp: OWNER_APP,
        projectSchema,
        driver,
        policy: [noInjectPolicy(projectSchema)],
        approved: true,
        appliedBy: "rollback-replays-the-inverse",
      };
      await apply({
        ...base,
        migration: created,
        priorMigrations: [],
        priorNameFallbacks: [],
        registry: {},
        nameFallback: created.name,
      });
      await apply({
        ...base,
        migration: unreversible,
        priorMigrations: [created],
        priorNameFallbacks: [created.name],
        registry: { acct: OWNER_APP },
        nameFallback: unreversible.name,
      });

      // Without this arm, making every data migration reversible would pass the
      // test above while removing the refusal that stops an operator from
      // getting a reverse the author explicitly disclaimed.
      await assert.rejects(
        () =>
          rollback({
            migrations: [created, unreversible],
            nameFallbacks: [created.name, unreversible.name],
            ownerApp: OWNER_APP,
            projectSchema,
            driver,
            registry: { acct: OWNER_APP },
            policy: [noInjectPolicy(projectSchema)],
            target: { kind: "steps", steps: 1 },
            approved: true,
            appliedBy: "rollback-replays-the-inverse",
          }),
        /irreversible/i,
        "a declared-irreversible data migration must still refuse by default",
      );

      const { rows } = await admin.query(
        `SELECT id FROM ${pgIdent(projectSchema)}.acct ORDER BY id`,
      );
      assert.equal(rows.length, 1, "the refused unwind changed nothing");
    });
  } finally {
    await admin.end().catch(() => {});
  }
});

test("MySQL: a recorded inverse is what a rollback runs", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL rollback-inverse coverage skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({ uri: MYSQL_URL });
  const projectSchema = uniqueNamespace("rbinv_my");
  const driver: DriverConfig = { kind: "mysql", url: MYSQL_URL };

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(projectSchema)}`);
    const base = {
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
      policy: [noInjectPolicy(projectSchema)],
      approved: true,
      appliedBy: "rollback-replays-the-inverse",
    };
    await apply({
      ...base,
      migration: created,
      priorMigrations: [],
      priorNameFallbacks: [],
      registry: {},
      nameFallback: created.name,
    });
    await apply({
      ...base,
      migration: seeded,
      priorMigrations: [created],
      priorNameFallbacks: [created.name],
      registry: { acct: OWNER_APP },
      nameFallback: seeded.name,
    });

    const rows = async (): Promise<unknown[]> => {
      const [result] = await admin.query(
        `SELECT id FROM ${mysqlIdent(projectSchema)}.acct ORDER BY id`,
      );
      return result as unknown[];
    };
    assert.equal((await rows()).length, 1, "the MySQL seed landed");

    const outcome = await rollback({
      migrations: [created, seeded],
      nameFallbacks: [created.name, seeded.name],
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
      registry: { acct: OWNER_APP },
      policy: [noInjectPolicy(projectSchema)],
      target: { kind: "steps", steps: 1 },
      approved: true,
      backupAcknowledged: true,
      appliedBy: "rollback-replays-the-inverse",
    });

    assert.deepEqual(
      outcome.skippedIrreversible,
      [],
      "a MySQL data migration that RECORDED an exact inverse is not irreversible",
    );
    assert.deepEqual(
      await rows(),
      [],
      "the parameterized MySQL inverse deletes exactly the row the forward inserted",
    );

    const afterRollback = await status({
      migrations: [created, seeded],
      nameFallbacks: [created.name, seeded.name],
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
      registry: { acct: OWNER_APP },
      policy: [noInjectPolicy(projectSchema)],
    });
    assert.equal(
      afterRollback.plans?.find((plan) => plan.name === seeded.name)?.state,
      "pending",
      "the MySQL rolled_back event makes status report the data plan pending",
    );

    await apply({
      ...base,
      migration: seeded,
      priorMigrations: [created],
      priorNameFallbacks: [created.name],
      registry: { acct: OWNER_APP },
      nameFallback: seeded.name,
    });
    assert.equal((await rows()).length, 1, "the MySQL data plan can be re-applied");
  } finally {
    await admin
      .query(`DROP DATABASE IF EXISTS ${mysqlIdent(projectSchema)}`)
      .catch(() => {});
    await admin
      .query(`DROP DATABASE IF EXISTS ${mysqlIdent(`${projectSchema}_migrations`)}`)
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});

test("SQLite: a recorded inverse is what a rollback runs", async () => {
  // The third backend. E1 claimed all three and pinned two: PostgreSQL and
  // MySQL had arms, SQLite had none, so the behaviour was true by measurement
  // and untrue by coverage. SQLite runs in-process through rusqlite rather than
  // over the host-driver seam, which is exactly the kind of separate path where
  // "the other two work" is not evidence.
  const dir = mkdtempSync(join(HERE, "rbinv-sqlite-"));
  const migrations = join(dir, "migrations");
  mkdirSync(migrations);
  writeFileSync(join(dir, "policy.toml"), noInjectPolicy("main"));
  writeFileSync(join(dir, "registry.json"), JSON.stringify({ acct: OWNER_APP }));
  writeFileSync(
    join(migrations, "20260101000000_create_acct.ts"),
    `import { table, t } from "zero-migrate";
export const name = "create_acct";
export default {
  schema() {
    table("acct").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`,
  );
  writeFileSync(
    join(migrations, "20260101000001_seed_acct.ts"),
    `import { table } from "zero-migrate";
export const name = "seed_acct";
export default {
  data() { table("acct").insert({ rows: { id: 1 } }); },
  inverse() { table("acct").delete({ where: (col) => col("id").eq(1) }); },
};
`,
  );

  const appPath = join(dir, "app.sqlite");
  const run = (args: readonly string[]) =>
    spawnSync(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, ...args,
        "--dir", migrations,
        "--policy", join(dir, "policy.toml"),
        "--registry", join(dir, "registry.json"),
        "--owner-app", OWNER_APP,
        "--database-url", `sqlite:${appPath}`,
      ],
      { cwd: dir, encoding: "utf8", env: { ...process.env, DATABASE_URL: "" } },
    );

  try {
    const applied = run(["apply", "--approve"]);
    assert.equal(applied.status, 0, `apply: ${applied.stdout}\n${applied.stderr}`);

    const rolled = run(["rollback", "--steps", "1", "--approve"]);
    assert.equal(rolled.status, 0, `rollback: ${rolled.stdout}\n${rolled.stderr}`);

    // The ROW, not the outcome line. A rollback reporting success while leaving
    // the row in place would satisfy any check on what it printed.
    const sqlite = await import("node:sqlite");
    const db = new sqlite.DatabaseSync(appPath);
    const rows = db.prepare("SELECT id FROM acct").all();
    db.close();
    assert.deepEqual(
      rows,
      [],
      "the recorded inverse must delete exactly the row the forward inserted",
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
