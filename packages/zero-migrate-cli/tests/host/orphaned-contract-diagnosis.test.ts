// Deleting a rename's migration file while its contract is still open.
//
// A real thing people do — revert a PR, tidy a directory — and doing it during an
// expand/contract window strands the rename: both columns and the dual-write
// trigger stay live with nothing in the directory to resolve them.
//
// WHAT WORKS, and is asserted here so it cannot regress:
//
//   * `status --strict` exits 1, so CI catches it;
//   * `status --json` reports the obligation as a DISTINCT state —
//     `pendingContracts: [{ table, pendingVersion, orphaned: true }]` — naming the
//     table and the version rather than burying it in generic drift.
//
// That second point is worth pinning precisely because the human-readable line is
// generic ("unexpected journal entry <version>"). The machine contract is the
// precise one, and `--strict` CI reads the machine contract.
//
// WHAT DOES NOT WORK, and is pinned as today's answer rather than blessed: the CLI
// cannot discharge what it just diagnosed. `resolve` rejects the migration NAME and
// the `pendingVersion` alike — both fail "unknown migration … in <dir>" — because
// it resolves identifiers against the SUPPLIED DIRECTORY, and the file is gone.
//
// So an operator handed `{"orphaned":true,"pendingVersion":"mig_…"}` has no CLI
// route from that diagnosis to a fix — and see the interaction below before judging
// how urgent that is.
//
// HOW BAD THIS IS DEPENDS ON ANOTHER OPEN BEHAVIOUR, so the two should be weighed
// together rather than separately. `pending-contract-blocks-unrelated.test.ts`
// pins that an outstanding contract blocks deploys touching UNRELATED tables. Put
// them side by side: an orphaned contract cannot be discharged from the CLI, and
// while it stands it blocks everything, not just the renamed table. Either fix
// alone reduces the damage — narrow the block, or give `resolve` a route to an
// orphaned version — and neither test can see the other.
// route from that diagnosis to a fix. The remedies are to restore the deleted file,
// or to call the embedding API, whose `ActionPayload` doc says the version is
// "directly accepted by the `resolvePending()` embedding API" — documented, and NOT
// verified here, so it is stated as documentation rather than as measurement.
//
// If `resolve` learns to take an orphaned `pendingVersion` directly, the last
// assertion here starts failing. That is the improvement; update this file then.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`. PostgreSQL only.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, unlinkSync, writeFileSync } from "node:fs";
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

const OWNER_APP = "app_orphan";

const CREATE = `import { table, t } from "zero-migrate";
export const name = "create_people";
export default {
  up() {
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
  up() {
    table("people").column("email").rename({ to: "email_address", type: t.text() });
  },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string): { work: string; renameFile: string } {
  const work = mkdtempSync(join(HERE, "orphan-"));
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
  const renameFile = join(work, "migrations", "20260102000000_rename_email.ts");
  writeFileSync(renameFile, RENAME_MIG);
  return { work, renameFile };
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

test("a deleted rename file leaves an orphaned contract that status names and resolve cannot fix", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("orphan");
  const { work, renameFile } = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const opened = await runCli(work, schema, ["apply", "--approve"]);
    assert.equal(opened.code, 0, `the rename window must open; ${opened.err}`);

    unlinkSync(renameFile);

    // 1. CI catches it.
    const strict = await runCli(work, schema, ["status", "--strict"]);
    assert.equal(strict.code, 1, `--strict must fail on an orphaned contract; ${strict.out}`);

    // 2. And the machine contract names it precisely, rather than only as drift.
    const json = await runCli(work, schema, ["status", "--json"]);
    const parsed = JSON.parse(json.out) as {
      pendingContracts?: Array<{ table: string; pendingVersion: string; orphaned?: boolean }>;
    };
    const orphan = (parsed.pendingContracts ?? []).find((c) => c.orphaned);
    assert.ok(
      orphan,
      `status --json must report the obligation as orphaned; got ${JSON.stringify(parsed.pendingContracts)}`,
    );
    assert.equal(orphan.table, "people", "the orphan must name its table");
    assert.ok(orphan.pendingVersion.startsWith("mig_"), "the orphan must carry its version");

    // 3. TODAY the CLI cannot act on that diagnosis. Both identifiers are looked
    //    up in the supplied directory, and the file is gone.
    const byVersion = await runCli(work, schema, [
      "resolve", orphan.pendingVersion, "--commit", "--approve",
    ]);
    assert.equal(
      byVersion.code,
      1,
      `TODAY resolve cannot take an orphaned pendingVersion; if this now succeeds ` +
        `the gap is closed - see the header. stderr=${byVersion.err}`,
    );
    assert.match(
      byVersion.err,
      /unknown migration/,
      `the refusal should still be the directory lookup; got ${byVersion.err}`,
    );

    // The window is therefore still open, which is what makes the gap matter.
    const { rows } = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'people'
          AND column_name IN ('email', 'email_address')
        ORDER BY column_name`,
      [schema],
    );
    assert.deepEqual(
      rows.map((r) => r.column_name),
      ["email", "email_address"],
      "both columns remain, so the rename is genuinely stranded",
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
