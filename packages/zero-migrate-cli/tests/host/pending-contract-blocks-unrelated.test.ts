// An outstanding rename contract blocks a deploy that touches a DIFFERENT table.
//
// WRITTEN TO FAIL WHEN THIS CHANGES, not to bless it. It records today's answer so
// a change lands with a decision attached, the way
// `guard-adoption-blind-spot.test.ts` does for its own open question.
//
// What is settled and correct: a deploy touching the SAME table as an outstanding
// contract is refused. `pending-contract-remediation.test.ts` covers that, and it
// is the behaviour the gate exists for.
//
// What this file measures is the neighbouring case nothing covered — a deploy
// whose new migration creates an unrelated table. Today it is refused too, with
// the same message:
//
//   table `people` has an in-flight online rename (contract pending from a prior
//   deploy, version `mig_…`); apply that contract before authoring further changes
//   to `people`
//
// even though the deploy authored no change to `people` at all. The refusal is
// raised by the bundle-level pre-flight — NO per-file progress is printed before
// it — so it is scoped to the supplied directory rather than to the ops that would
// run.
//
// WHY IT MIGHT BE WRONG. `PendingContractRefusal`'s own doc says it fires "when the
// current deploy's op list touches a table that still has an outstanding
// online-rename contract"; this deploy's op list does not. `docs/troubleshooting.md`
// titles the section "A TABLE is blocked by a pending rename". And the
// expand/contract window spans deploys BY DESIGN — so while it is open, a shared
// migrations directory cannot ship any schema change at all, which is a heavier
// constraint than the table-scoped one the documentation describes.
//
// WHY IT MIGHT BE RIGHT. Fail-closed is the house style, the refusal names a
// concrete remediation, and a bundle-scoped gate cannot be fooled by an op list
// that under-reports what it touches (the `TOUCHES_UNKNOWN` case the engine
// already fails closed on).
//
// Not resolved here: narrowing a safety gate is a judgement call about what
// `apply` promises during a rename window, and it belongs to whoever owns that
// promise.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`. PostgreSQL only.

// INTERACTS WITH AN ORPHANED CONTRACT, and the pair is worse than either alone.
// `orphaned-contract-diagnosis.test.ts` pins that a contract whose creator file
// was deleted can be DIAGNOSED but not discharged from the CLI — `resolve` looks
// identifiers up in the supplied directory, and the file is gone. Combine the two:
// such a contract cannot be cleared, and while it stands this refusal blocks every
// deploy in the directory, not just the renamed table. Whichever is addressed
// first, the other's severity changes, and neither test can see that from where it
// sits.

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

const OWNER_APP = "app_unrelated";

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

const RENAME_MIG = `import { table, t } from "zero-migrate";
export const name = "rename_email";
export default {
  schema() {
    table("people").column("email").rename({ to: "email_address", type: t.text() });
  },
};
`;

const UNRELATED = `import { table, t } from "zero-migrate";
export const name = "create_widgets";
export default {
  schema() { table("widgets").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] }); },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "unrelated-"));
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
  writeFileSync(
    join(work, "registry.json"),
    JSON.stringify({ people: OWNER_APP, widgets: OWNER_APP }),
  );
  writeFileSync(join(work, "migrations", "20260101000000_create_people.ts"), CREATE);
  writeFileSync(join(work, "migrations", "20260102000000_rename_email.ts"), RENAME_MIG);
  return work;
}

function runCli(work: string, schema: string, argv: string[]) {
  return new Promise<{ code: number | null; out: string; err: string }>((done) => {
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
      done({ code, out, err: err.replace(/^WARNING.*$/gm, "").trim() }),
    );
  });
}

test("TODAY an open rename window blocks a deploy on an unrelated table", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("unrelated");
  const work = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const opened = await runCli(work, schema, ["apply", "--approve"]);
    assert.equal(opened.code, 0, `the rename window must open; ${opened.err}`);

    // The new migration names only `widgets`. Nothing in it touches `people`.
    writeFileSync(join(work, "migrations", "20260103000000_create_widgets.ts"), UNRELATED);
    const blocked = await runCli(work, schema, ["apply", "--approve"]);

    // If this assertion starts failing, the gate has been narrowed to the ops
    // that actually run. That is a legitimate improvement - delete this file and
    // keep `pending-contract-remediation.test.ts`, which covers the same-table
    // case that must keep refusing.
    assert.equal(
      blocked.code,
      1,
      `TODAY this is refused; if it now succeeds the gate was narrowed - see the ` +
        `header before deleting this file. stdout=${blocked.out}`,
    );
    assert.match(
      blocked.err,
      /in-flight online rename/,
      `the refusal must still name the cause; got ${blocked.err}`,
    );

    // The refusal is bundle-level: nothing was applied, and no per-file progress
    // was printed before it. That is what makes it scoped to the directory rather
    // than to the pending ops.
    assert.equal(
      blocked.out.trim(),
      "",
      `the refusal must precede any per-file work; got stdout=${blocked.out}`,
    );
    const { rows } = await client.query(
      `SELECT table_name FROM information_schema.tables
        WHERE table_schema = $1 ORDER BY table_name`,
      [schema],
    );
    assert.deepEqual(
      rows.map((r) => r.table_name),
      ["people"],
      "the unrelated table must not have been created by a refused deploy",
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
