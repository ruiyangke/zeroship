// `plan` must describe the same runnable set as `apply`. In particular, a later
// migration touching a table with an outstanding online-rename contract is
// blocked even when it has no explicit dependency on the rename.
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

const OWNER_APP = "app_plan_blocked_rename";

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

const RENAME = `import { table, t } from "zero-migrate";
export const name = "rename_email";
export default {
  schema() {
    table("people").column("email").rename({ to: "email_address", type: t.text() });
  },
};
`;

const ADD_NOTE = `import { table, t } from "zero-migrate";
export const name = "add_note";
export default {
  schema() {
    table("people").column("note").add({ type: t.text() });
  },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "planblocked-"));
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
  return work;
}

function runCli(
  work: string,
  schema: string,
  argv: string[],
): Promise<{ code: number | null; out: string; err: string }> {
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import",
        "tsx",
        CLI_BIN,
        ...argv,
        "--dir",
        join(work, "migrations"),
        "--database-url",
        pgUrl(),
        "--policy",
        join(work, "policy.toml"),
        "--registry",
        join(work, "registry.json"),
        "--schema",
        schema,
        "--owner-app",
        OWNER_APP,
      ],
      {
        cwd: work,
        env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
      },
    );
    let err = "";
    let out = "";
    child.stderr.on("data", (chunk) => (err += chunk));
    child.stdout.on("data", (chunk) => (out += chunk));
    child.on("close", (code) =>
      resolvePromise({
        code,
        out: out.trim(),
        err: err.replace(/^WARNING.*$/gm, "").trim(),
      }),
    );
  });
}

test("plan separates work blocked by an outstanding online rename", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("f662");
  const work = project(schema);
  const renamePath = join(work, "migrations", "20260102000000_rename_email.ts");
  const addNotePath = join(work, "migrations", "20260103000000_add_note.ts");
  try {
    // Applying into a non-public namespace does not create the target schema.
    await client.query(`CREATE SCHEMA "${schema}"`);

    const created = await runCli(work, schema, ["apply", "--approve"]);
    assert.equal(created.code, 0, `initial apply must succeed; ${created.err}`);

    // CONTROL: before the rename contract exists, the later migration is ordinary
    // runnable work and keeps the existing plan count and SQL rendering.
    writeFileSync(addNotePath, ADD_NOTE);
    const control = await runCli(work, schema, ["plan"]);
    assert.equal(control.code, 0, `control plan must succeed; ${control.err}`);
    assert.match(control.out, /would apply 1 migration\b/);
    assert.match(control.out, /-- plan "add_note"\s+\(dialect: postgres\)/);
    assert.match(control.out, /ALTER TABLE[\s\S]*ADD COLUMN[\s\S]*note/i);
    assert.doesNotMatch(
      control.out,
      /^blocked:/m,
      "ordinary plan output gains no blocked lines",
    );
    rmSync(addNotePath);

    // Open the online-rename window, then restore the exact migration previewed by
    // the control. The contract is the only state variable.
    writeFileSync(renamePath, RENAME);
    const expanded = await runCli(work, schema, ["apply", "--approve"]);
    assert.equal(expanded.code, 0, `rename expand must succeed; ${expanded.err}`);
    writeFileSync(addNotePath, ADD_NOTE);

    const human = await runCli(work, schema, ["plan"]);
    assert.equal(human.code, 0, `plan remains a successful description; ${human.err}`);
    assert.match(human.out, /would apply 0 migrations\b/);
    assert.doesNotMatch(human.out, /^-- plan /m, "no non-runnable plan is rendered");
    assert.doesNotMatch(human.out, /ADD COLUMN[\s\S]*note/i);
    assert.match(human.out, /^blocked:/m);
    assert.match(human.out, /add_note/);

    const json = await runCli(work, schema, ["plan", "--json"]);
    assert.equal(json.code, 0, `JSON plan remains successful; ${json.err}`);
    const parsed = JSON.parse(json.out) as {
      count: number;
      pending: Array<{ version: string; name: string; sql: string }>;
      blocked: Array<{ version: string; name: string; reason: string }>;
    };
    assert.equal(parsed.count, 0, "the count is the runnable pending count");
    assert.deepEqual(parsed.pending, [], "blocked work is not runnable pending work");
    assert.equal(parsed.blocked.length, 1, "blocked work must not disappear from JSON");
    assert.equal(parsed.blocked[0].name, "add_note");
    assert.match(parsed.blocked[0].version, /^mig_/);
    assert.match(parsed.blocked[0].reason, /table `people` has an in-flight online rename/);

    const refused = await runCli(work, schema, ["apply", "--approve"]);
    assert.notEqual(refused.code, 0, "apply must still refuse the blocked migration");
    const applyMessage = `${refused.out}\n${refused.err}`;
    assert.ok(
      applyMessage.includes(parsed.blocked[0].reason),
      `plan and apply must report the identical reason; apply printed: ${applyMessage}`,
    );
    assert.ok(
      human.out.includes(parsed.blocked[0].reason),
      "human and JSON plan output must report the identical reason",
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
