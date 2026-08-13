// Editing a migration that has already been applied, measured against a live server.
//
// `docs/security-model.md` states it as a guarantee of apply: "a reused identity
// with a different checksum fails". That is the tamper property. It is what stops
// an edited migration from being replayed against a database that already ran the
// original, whether the edit came from a bad rebase, a hand-fix on a hotfix
// branch, or someone changing history deliberately.
//
// WHAT WAS ALREADY COVERED, AND WHY IT IS NOT THIS. `e2e-pg.test.ts` applies a
// MODIFIED artifact into a FRESH schema and asserts it folds a different checksum
// anchor. That proves the checksum is a function of the ops; it never re-applies
// against the schema that holds the original, so it cannot show that apply
// REFUSES. `cli.test.ts` asserts the "checksum mismatch" wording, but over a
// fabricated status object with no database. A build that computed a perfect
// checksum and then ignored it on apply would keep both of those green.
//
// THREE ARMS, and the first is the control:
//
//   1. re-applying the UNCHANGED migration succeeds - so arm 2 is about the EDIT,
//      not about re-applying, and not about some unrelated second-run failure;
//   2. re-applying the EDITED migration under the same filename (same version
//      identity) is refused, naming both checksums;
//   3. `status --strict` reports the same thing, since that is the CI gate an
//      operator actually puts in front of a deploy.
//
// The load-bearing assertion is none of the exit codes: it is that the edited
// migration's new column is ABSENT afterwards. A refusal that had already applied
// half the edit would exit 1 too, and would be far worse than no check at all.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`.

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

const OWNER_APP = "app_tamper";
/** The identity. It is the FILENAME that fixes the version, so writing a different
 *  body to this same path is precisely "a reused identity with a different
 *  checksum" - the case the security model says must fail. */
const MIGRATION_FILE = "20260101000000_create_things.ts";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function body(extraColumn: string): string {
  return `import { table, t } from "zero-migrate";
export const name = "create_things";
export default {
  up() {
    table("things").create({
      columns: { id: t.int().notNull()${extraColumn} },
      primaryKey: ["id"],
    });
  },
};
`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "tamper-"));
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
`,
  );
  writeFileSync(join(work, "registry.json"), JSON.stringify({ things: OWNER_APP }));
  writeFileSync(join(work, "migrations", MIGRATION_FILE), body(""));
  return work;
}

function cli(
  work: string,
  schema: string,
  argv: string[],
): Promise<{ code: number | null; text: string }> {
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

test("an edited migration that was already applied is refused, and lands nothing", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("tamper_pg");
  const meta = `${schema}_migrations`;
  const work = project(schema);

  const columns = async (): Promise<string[]> => {
    const { rows } = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'things' ORDER BY column_name`,
      [schema],
    );
    return rows.map((row) => row.column_name as string);
  };

  try {
    await client.query(`CREATE SCHEMA "${schema}"`);

    const applied = await cli(work, schema, ["apply", "--approve"]);
    assert.equal(applied.code, 0, `the original must apply; ${applied.text}`);
    assert.deepEqual(await columns(), ["id"], "the original shape lands");

    // ARM 1, the control — the same bytes re-applied are a no-op, so a refusal in
    // arm 2 cannot be "re-applying is refused" or a second run failing generally.
    const unchanged = await cli(work, schema, ["apply", "--approve"]);
    assert.equal(
      unchanged.code,
      0,
      `re-applying the UNCHANGED migration must succeed, or arm 2 proves nothing; ${unchanged.text}`,
    );

    // ARM 2 — the edit. Same filename, so the same version identity, different body.
    writeFileSync(join(work, "migrations", MIGRATION_FILE), body(", sneaky: t.text()"));
    const tampered = await cli(work, schema, ["apply", "--approve"]);

    assert.equal(tampered.code, 1, `the edited migration must be refused; ${tampered.text}`);
    // By CONTENT: an unrelated failure would also exit 1.
    assert.match(
      tampered.text,
      /checksum drift/,
      `the refusal must be the checksum gate: ${tampered.text}`,
    );
    // Both sides, so an operator can see it is a disagreement rather than a
    // corrupted journal, and can tell which artifact is the odd one out.
    assert.match(
      tampered.text,
      /journal has [0-9a-f]{16}/,
      `the refusal must quote the journal's checksum: ${tampered.text}`,
    );
    assert.match(
      tampered.text,
      /set has [0-9a-f]{16}/,
      `and the supplied set's checksum: ${tampered.text}`,
    );

    // THE ASSERTION THAT MATTERS. Exit 1 with the edit half-applied would be worse
    // than no gate: the schema would carry a change no journal entry describes.
    assert.deepEqual(
      await columns(),
      ["id"],
      "the edited migration must not have applied any part of itself",
    );

    // ARM 3 — the CI gate reports it too. An operator who never runs `apply`
    // manually meets this through `status --strict`.
    const strict = await cli(work, schema, ["status", "--strict"]);
    assert.equal(strict.code, 1, `strict status must fail on a tampered set; ${strict.text}`);
    assert.match(
      strict.text,
      /checksum mismatch: create_things/,
      `and must name the migration: ${strict.text}`,
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
