// The `resolve` verb, driven end to end against a live PostgreSQL.
//
// A PostgreSQL online rename runs in two deploys: `apply` opens the coexistence
// window, leaving the old and new columns side by side, and `resolve` closes it
// in one of two directions. Until now the only coverage of `resolve` was
// argument parsing and its behaviour under lock contention. The lifecycle
// itself - does committing keep the right column, does the DATA come with it,
// does rolling back put things back - was untested from the front door.
//
// The data assertion is the load-bearing one in both directions. A rename that
// kept the right column name but lost the values would satisfy every
// column-shape check here and still be the worst possible outcome.
//
// WHAT THE ERROR MESSAGES USED TO SAY. Every one of these settled states was
// reported as `migration "x" is not pending`:
//
//   resolved by --commit    -> state applied
//   resolved by --rollback  -> state aborted
//   applied, never had a rename
//
// True, and misleading in the one direction that costs something. Everywhere
// else in this CLI "pending" means NOT YET APPLIED, so an operator retrying a
// resolve - a replayed pipeline step, a second pair of hands - reads "is not
// pending" as "the rename never happened" and goes looking for a lost deploy.
// The states that actually reach this branch are the opposite of that.
//
// So each is now named for what it is. The commit and no-rename cases share
// wording deliberately: a resolved contract leaves no trace in the status reply,
// so from here they are indistinguishable, and the sentence has to be true of
// both.
//
// The never-applied case already had an accurate message and still does - it is
// the one state in this branch where "pending" carries its usual meaning.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`. PostgreSQL only; `resolve` refuses other
// dialects, since only PostgreSQL has the online rename.

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

const OWNER_APP = "app_resolve_lifecycle";

/** A seeded row, so the rename can be checked to carry its VALUES across. */
const CREATE = `import { table, t } from "zero-migrate";
export const name = "create_users";
export default {
  up() {
    table("users").create({
      columns: { id: t.int().notNull(), display_name: t.text() },
      primaryKey: ["id"],
    });
    table("users").insert({ rows: { id: 1, display_name: "ada" } });
  },
};
`;

const RENAME = `import { table, t } from "zero-migrate";
export const name = "rename_display_name";
export default {
  schema() {
    table("users").column("display_name").rename({ to: "full_name", type: t.text() });
  },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "resolvelife-"));
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
  // A rename rewrites a table, so the deploying app must own it.
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

/** Column names of the `users` table, sorted, so shape assertions are stable. */
async function columnsOf(
  client: { query: (sql: string, params: unknown[]) => Promise<{ rows: Array<Record<string, unknown>> }> },
  schema: string,
): Promise<string[]> {
  const { rows } = await client.query(
    `SELECT column_name FROM information_schema.columns
      WHERE table_schema = $1 AND table_name = 'users' ORDER BY column_name`,
    [schema],
  );
  return rows.map((row) => row.column_name as string);
}

type Body = (work: string, schema: string) => Promise<void>;

/** Fresh schema + project per scenario, torn down whatever happens. */
async function scenario(ctx: Parameters<typeof connectLivePg>[0], body: Body): Promise<void> {
  const client = await connectLivePg(ctx);
  if (!client) return;
  const schema = uniqueNamespace("resolvelife");
  const work = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    await body(work, schema);
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
}

test("--commit keeps the new column, drops the old one, and carries the values", async (ctx) => {
  await scenario(ctx, async (work, schema) => {
    const client = await connectLivePg(ctx);
    if (!client) return;
    try {
      const applied = await runCli(work, schema, ["apply", "--approve"]);
      assert.equal(applied.code, 0, `apply must succeed; ${applied.err}`);

      // The coexistence window: both columns live side by side until resolved.
      assert.deepEqual(
        await columnsOf(client, schema),
        ["display_name", "full_name", "id"],
        "apply must open the window with both columns present",
      );

      const committed = await runCli(work, schema, [
        "resolve", "rename_display_name", "--commit", "--approve",
      ]);
      assert.equal(committed.code, 0, `resolve --commit must succeed; ${committed.err}`);

      assert.deepEqual(
        await columnsOf(client, schema),
        ["full_name", "id"],
        "--commit keeps the new column and drops the old one",
      );
      const { rows } = await client.query(
        `SELECT full_name FROM "${schema}".users WHERE id = 1`,
        [],
      );
      assert.equal(
        rows[0]?.full_name,
        "ada",
        "the value must come across with the rename - a renamed but empty column is the worst outcome",
      );
    } finally {
      await client.end().catch(() => {});
    }
  });
});

test("--rollback keeps the old column, drops the new one, and leaves the values", async (ctx) => {
  await scenario(ctx, async (work, schema) => {
    const client = await connectLivePg(ctx);
    if (!client) return;
    try {
      const applied = await runCli(work, schema, ["apply", "--approve"]);
      assert.equal(applied.code, 0, `apply must succeed; ${applied.err}`);

      const rolled = await runCli(work, schema, [
        "resolve", "rename_display_name", "--rollback", "--approve",
      ]);
      assert.equal(rolled.code, 0, `resolve --rollback must succeed; ${rolled.err}`);

      assert.deepEqual(
        await columnsOf(client, schema),
        ["display_name", "id"],
        "--rollback keeps the old column and drops the new one",
      );
      const { rows } = await client.query(
        `SELECT display_name FROM "${schema}".users WHERE id = 1`,
        [],
      );
      assert.equal(rows[0]?.display_name, "ada", "rolling back must not disturb the values");
    } finally {
      await client.end().catch(() => {});
    }
  });
});

test("a settled rename is named for the state it settled into, not called 'not pending'", async (ctx) => {
  await scenario(ctx, async (work, schema) => {
    const applied = await runCli(work, schema, ["apply", "--approve"]);
    assert.equal(applied.code, 0, `apply must succeed; ${applied.err}`);
    const committed = await runCli(work, schema, [
      "resolve", "rename_display_name", "--commit", "--approve",
    ]);
    assert.equal(committed.code, 0, `resolve --commit must succeed; ${committed.err}`);

    // A retried pipeline step, and an operator reaching for the other direction.
    for (const action of ["--commit", "--rollback"]) {
      const again = await runCli(work, schema, [
        "resolve", "rename_display_name", action, "--approve",
      ]);
      assert.equal(again.code, 1, `resolving a settled rename must fail; ${again.err}`);
      assert.match(
        again.err,
        /is fully applied; there is no outstanding online rename to resolve/,
        `${action} after a commit must say the migration is applied`,
      );
      // The regression: this used to read as though the rename never ran.
      assert.doesNotMatch(
        again.err,
        /is not pending$/m,
        `${action} must not describe an applied migration as "not pending"`,
      );
    }
  });
});

test("a rename that was rolled back says so, rather than reporting it never ran", async (ctx) => {
  await scenario(ctx, async (work, schema) => {
    const applied = await runCli(work, schema, ["apply", "--approve"]);
    assert.equal(applied.code, 0, `apply must succeed; ${applied.err}`);
    const rolled = await runCli(work, schema, [
      "resolve", "rename_display_name", "--rollback", "--approve",
    ]);
    assert.equal(rolled.code, 0, `resolve --rollback must succeed; ${rolled.err}`);

    const again = await runCli(work, schema, [
      "resolve", "rename_display_name", "--commit", "--approve",
    ]);
    assert.equal(again.code, 1, `resolving an aborted rename must fail; ${again.err}`);
    assert.match(
      again.err,
      /had its online rename rolled back already/,
      "an aborted rename must be named as aborted, not as 'not pending'",
    );
  });
});

test("the never-applied case keeps its own message, where 'pending' still means pending", async (ctx) => {
  await scenario(ctx, async (work, schema) => {
    // Nothing applied at all, so `create_users` IS pending in the ordinary sense.
    // This is the one state reaching this area where the old wording was right,
    // and it is the control for the three assertions above: without it, those
    // could pass because every message had been rewritten indiscriminately.
    const never = await runCli(work, schema, [
      "resolve", "create_users", "--commit", "--approve",
    ]);
    assert.equal(never.code, 1, `resolving an unapplied migration must fail; ${never.err}`);
    assert.match(
      never.err,
      /has no pending online rename/,
      "a genuinely pending migration without a rename keeps the accurate original message",
    );
  });
});

test("an applied migration that never had a rename is not described as pending either", async (ctx) => {
  await scenario(ctx, async (work, schema) => {
    const applied = await runCli(work, schema, ["apply", "--approve"]);
    assert.equal(applied.code, 0, `apply must succeed; ${applied.err}`);

    const wrong = await runCli(work, schema, ["resolve", "create_users", "--commit", "--approve"]);
    assert.equal(wrong.code, 1, `resolving a migration with no rename must fail; ${wrong.err}`);
    assert.match(
      wrong.err,
      /is fully applied; there is no outstanding online rename to resolve/,
      "an applied migration with no rename must be described as applied",
    );
  });
});
