// `docs/troubleshooting.md` §"A later file fails after an earlier file succeeds"
// makes three claims about what survives a failed deploy. Two were documented and
// unpinned; the third is the one operators most need and the docs never state.
//
//   1. INTER-FILE   "Earlier files can remain applied when a later file fails."
//   2. INTRA-FILE   "A single file can also contain several database changes that
//                   commit separately, so an error later in that file can leave
//                   earlier changes applied."
//   3. RESUMABLE    (undocumented) re-running the SAME deploy after repairing the
//                   cause skips the completed steps and runs only the failed one.
//
// Claim 2 is the hard one to reach, and the first attempt at it proved nothing:
// the obvious obstacle (a table the later file re-creates) is caught by the FOLD,
// which projects the pending schema before anything executes. That run failed with
// `failed to project pending schema after envelope "file_b"` and left NONE of the
// file's ops applied — a pre-flight refusal, not a partial application. The fold
// being that good is why claim 2 needs a failure the fold cannot foresee.
//
// So the obstacle here is DATA: a unique index over a column holding duplicate
// rows. The fold sees schema, not rows, so it passes the file through, op 0
// commits, and op 1 fails at the server. That is the only shape that exercises
// what the sentence describes.
//
// Claim 3 matters because the docs steer the reader the other way — "stop
// automatic retries ... use a new forward migration". Stopping a blind retry LOOP
// is sound advice, but a deliberate retry after fixing the cause is the supported
// recovery, and this pins it: the retry does not fail with `survivor already
// exists`, it resumes.
//
// The failure this is shaped to catch is a partially-applied file that WEDGES —
// where the completed step is replayed on retry and dies on its own object, so the
// only way out is hand-editing the journal.
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

const OWNER_APP = "app_partial";

// The base pair creates `dupes`, then seeds two rows sharing a `tag`. The
// duplicate is invisible to the offline fold and fatal to a unique index.
const FILE_A_SCHEMA = `import { table, t } from "zero-migrate";
export const name = "create_dupes";
export default {
  schema() {
    table("dupes").create({
      columns: { id: t.int().notNull(), tag: t.string({ length: 32 }) },
      primaryKey: ["id"],
    });
  },
};
`;

const FILE_A_DATA = `import { table } from "zero-migrate";
export const name = "seed_dupes";
export default {
  data() {
    table("dupes").insert({ rows: [{ id: 1, tag: "same" }, { id: 2, tag: "same" }] });
  },
  inverse() {
    table("dupes").delete({ where: (col) => col("id").in([1, 2]) });
  },
};
`;

// File B is ONE file with TWO changes. Op 0 creates a fresh table and commits.
// Op 1 builds a unique index over the duplicated column and fails at the server.
const FILE_B = `import { table, t } from "zero-migrate";
export const name = "survivor_then_unique";
export default {
  schema() {
    table("survivor").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
    table("dupes").index("dupes_tag_key").add({ on: [{ column: "tag" }], unique: true });
  },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "partial-"));
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
  // Both tables must be registered to this app, or the ownership guard refuses
  // before the data ever gets a chance to fail — a refusal that would look like a
  // pass here while proving nothing.
  writeFileSync(
    join(work, "registry.json"),
    JSON.stringify({ dupes: OWNER_APP, survivor: OWNER_APP }),
  );
  writeFileSync(join(work, "migrations", "20260101000000_create_dupes.ts"), FILE_A_SCHEMA);
  writeFileSync(join(work, "migrations", "20260101000001_seed_dupes.ts"), FILE_A_DATA);
  return work;
}

function run(work: string, schema: string, argv: string[]) {
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
  return new Promise<{ code: number | null; out: string; err: string }>((done) =>
    child.on("close", (code) =>
      done({ code, out: out.trim(), err: err.replace(/^WARNING.*$/gm, "").trim() }),
    ),
  );
}

test("a file that fails partway leaves its earlier work applied, and the retry resumes", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("partial");
  const meta = `${schema}_migrations`;
  const work = project(schema);
  const names = async (): Promise<string[]> => {
    const { rows } = await client.query(
      `SELECT table_name FROM information_schema.tables
        WHERE table_schema = $1 ORDER BY table_name`,
      [schema],
    );
    return rows.map((row) => row.table_name as string);
  };
  const journal = async (): Promise<string[]> => {
    const { rows } = await client.query(
      `SELECT name FROM "${meta}".schema_migrations
        WHERE event_kind = 'applied' AND (phase IS NULL OR phase = 'completed')
        ORDER BY event_seq`,
      [],
    );
    return rows.map((row) => row.name as string);
  };

  try {
    await client.query(`CREATE SCHEMA "${schema}"`);

    const seeded = await run(work, schema, ["apply", "--approve"]);
    assert.equal(seeded.code, 0, `the base schema and seed migrations must apply; ${seeded.err}`);

    // Non-vacuity: the duplicate the whole test rests on must actually be there.
    const { rows: seededRows } = await client.query(
      `SELECT count(*)::int AS n FROM "${schema}".dupes WHERE tag = 'same'`,
    );
    assert.equal(seededRows[0].n, 2, "the duplicate rows must exist, or op 1 would succeed");

    writeFileSync(join(work, "migrations", "20260102000000_survivor.ts"), FILE_B);
    const failed = await run(work, schema, ["apply", "--approve"]);

    assert.equal(failed.code, 1, `file B must fail on the duplicate data; ${failed.err}`);
    // It must fail at the SERVER on the index, not in the offline fold. If this
    // ever starts failing at projection time, the test has stopped exercising
    // claim 2 and needs a new obstacle — the same way the first attempt did.
    assert.match(
      failed.err,
      /unique index|dupes_tag_key/,
      `file B must fail building the index, not before it: ${failed.err}`,
    );
    assert.doesNotMatch(
      failed.err,
      /failed to project pending schema/,
      `the fold pre-empted the failure, so nothing partial was exercised: ${failed.err}`,
    );

    // CLAIM 1, inter-file: the earlier base pair's table is still here, still journalled.
    // CLAIM 2, intra-file: so is `survivor`, op 0 of the file that FAILED.
    const afterFailure = await names();
    assert.ok(afterFailure.includes("dupes"), "the earlier file must remain applied");
    assert.ok(
      afterFailure.includes("survivor"),
      "op 0 of the failed file must remain applied - this is the intra-file claim",
    );

    // The journal must be honest about it rather than silently carrying a step
    // whose object exists. A created-but-unrecorded table is the state that makes
    // the next deploy's pending computation wrong.
    const afterJournal = await journal();
    assert.ok(
      afterJournal.some((name) => name.includes("survivor")),
      `the surviving step must be journalled, got: ${afterJournal.join(", ")}`,
    );
    assert.ok(
      !afterJournal.some((name) => name.includes("dupes_tag_key")),
      `the step that failed must NOT be journalled, got: ${afterJournal.join(", ")}`,
    );

    // CLAIM 3: the operator repairs the cause and re-runs the SAME deploy. The
    // completed step must be skipped, not replayed onto its own table.
    await client.query(`DELETE FROM "${schema}".dupes WHERE id = 2`);
    const retried = await run(work, schema, ["apply", "--approve"]);

    assert.equal(
      retried.code,
      0,
      `the retry must resume rather than wedge on the completed step; ${retried.err}`,
    );
    assert.doesNotMatch(
      retried.err,
      /already exists/i,
      `the retry replayed a completed step instead of skipping it: ${retried.err}`,
    );

    // And it must have finished the job, not just exited zero.
    const { rows: index } = await client.query(
      `SELECT indexname FROM pg_indexes WHERE schemaname = $1 AND indexname = 'dupes_tag_key'`,
      [schema],
    );
    assert.equal(index.length, 1, "the retry must build the index that originally failed");

    const settled = await run(work, schema, ["status"]);
    assert.equal(settled.code, 0, `status must succeed after recovery; ${settled.err}`);
    assert.match(
      settled.out,
      /0 pending/,
      `the recovered deploy must leave nothing pending: ${settled.out}`,
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
