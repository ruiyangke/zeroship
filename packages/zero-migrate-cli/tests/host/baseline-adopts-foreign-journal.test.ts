// `zero-migrate baseline`: adopt a database whose schema already exists and whose
// journal was written by a DIFFERENT tool, driven end to end through the shipped
// CLI against live PostgreSQL.
//
// The situation this verb exists for is not "a database with no journal". It is a
// database whose journal is FULL, under migration ids the CLI can never derive.
// `MigrationId::derive` (crates/zeroship-migrate-ir/src/migration.rs:83) stamps the
// high 48 bits with `0xFF` x 6 so a derived id "never collides with a versioned
// id"; `migration_id_for_version` (same file, :133-141) puts a numeric file version
// in exactly those bits. The two families are disjoint BY CONSTRUCTION, so pointing
// the CLI at a journal written under the versioned family reports every one of its
// rows as `unexpectedJournal` drift and every authored migration as pending -- while
// the tables those migrations create are already sitting in the catalog.
//
// The journal is append-only by trigger
// (crates/zeroship-migrate-postgres/src/backend/journal_sql.rs, `migration journal is
// append-only (no UPDATE/DELETE)`), so the foreign rows cannot be rewritten. Only an
// ADDITIVE event can reinterpret them, which is what `baseline` writes: one
// records-not-run `completed` event per authored step, and supersession edges over
// the rows the migration set does not account for.
//
// GATE: `connectLivePg` (see `live-db.ts`). Runs under `node --import tsx --test`.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { connectLivePg, pgUrl } from "./live-db.js";
import { noInjectPolicy } from "./policy.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zeroship-migrate-node/zeroship-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

/** The base62 alphabet `typed_id` encodes a UUID image with
 *  (crates/zeroship-migrate-ir/src/id.rs:22). Sorted so lexicographic order matches
 *  numeric order in the high bits. */
const BASE62 = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/**
 * The id family a VERSIONED migration runner journals under, reproduced exactly.
 *
 * `migration_id_for_version` (crates/zeroship-migrate-ir/src/migration.rs:133-141)
 * copies the low 48 bits of the file version into `bytes[0..6]` and leaves the
 * remaining ten bytes zero, then base62-encodes the 128-bit image to 22 chars. As a
 * big-endian integer that image is `version * 2^80`.
 *
 * Reproduced here rather than imported because the point of the test is that this
 * family is unreachable from the CLI: nothing the CLI exports can mint one.
 */
function migrationIdForVersion(version: bigint): string {
  let image = version << 80n;
  const digits: string[] = [];
  for (let i = 0; i < 22; i++) {
    digits.push(BASE62[Number(image % 62n)]);
    image /= 62n;
  }
  return `mig_${digits.reverse().join("")}`;
}

function spawnCli(args: readonly string[], cwd: string) {
  return spawnSync(process.execPath, ["--import", "tsx", CLI_BIN, ...args], {
    encoding: "utf8",
    cwd,
    env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
  });
}

function temporaryDirectory(prefix: string): string {
  return mkdtempSync(join(HERE, prefix));
}

function uniqueSchema(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/**
 * One migration file authoring exactly one `create table`, hence one journal step.
 *
 * `guarded` adds `ifNotExists`, which decides whether `status` can RUN at all on
 * the database this file is about -- see the two arms below. It is a parameter
 * rather than a constant because both shapes are real: the platform corpus is
 * unguarded, and the guarded form is the only one whose reconciliation is
 * observable before adoption.
 */
function writeStep(
  dir: string,
  filename: string,
  name: string,
  tableName: string,
  options: { guarded: boolean },
): void {
  const create = options.guarded
    ? `{ columns: { id: t.int() }, ifNotExists: true }`
    : `{ columns: { id: t.int() } }`;
  writeFileSync(
    join(dir, filename),
    `import { table, t } from "zero-migrate";
export const name = ${JSON.stringify(name)};
export function schema() {
  table(${JSON.stringify(tableName)}).create(${create});
}
`,
  );
}

interface StatusJson {
  applied: string[];
  pending: string[];
  unexpectedJournal: Array<{ version: string; state: string }>;
  plans?: Array<{
    version: string;
    name: string;
    state: string;
    steps?: Array<{ version: string; name: string; state: string }>;
  }>;
}

/** The `--json` shape of a `baseline` reply (napi camelCases the DTO fields). */
interface BaselineJson {
  recorded: Array<{ version: string; name: string; kind: string }>;
  alreadyRecorded: string[];
  unmatched: string[];
  superseded: string[];
  wrote: boolean;
}

const TABLES = ["alpha", "beta", "gamma"] as const;

/** The versioned file ordinals a peer runner would have journaled these under. */
const FOREIGN_ORDINALS = [20260101000001n, 20260101000002n, 20260101000003n];

function statusArgs(schema: string): string[] {
  return [
    "status",
    "--dir=.",
    `--database-url=${pgUrl()}`,
    `--schema=${schema}`,
    "--policy=policy.toml",
    "--strict",
    "--json",
  ];
}

test("baseline adopts a live schema whose journal is in a foreign id family", async () => {
  const client = await connectLivePg();
  const cwd = temporaryDirectory(".cli-baseline-");
  const schema = uniqueSchema("zm_baseline");
  const meta = `${schema}_migrations`;
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    writeFileSync(join(cwd, "policy.toml"), noInjectPolicy(schema));
    // GUARDED (`ifNotExists`) on purpose. `status` lowers the PENDING migrations
    // onto the live catalog, and an unguarded `createTable` over a table that
    // already exists fails that projection outright -- so on an unguarded corpus
    // there is no before-reconciliation to read. That case is its own test below;
    // this one isolates the id-family disjointness, which is what the verb is for.
    TABLES.forEach((tableName, index) => {
      writeStep(
        cwd,
        `2026080100000${index + 1}_m${index + 1}_${tableName}.ts`,
        `m${index + 1}_${tableName}`,
        tableName,
        { guarded: true },
      );
    });

    // 1. The database the operator actually has: the tables exist, applied by a
    // peer runner, and the journal records them under the VERSIONED id family.
    for (const tableName of TABLES) {
      await client.query(`CREATE TABLE "${schema}"."${tableName}" (id integer)`);
    }
    // `history` is the one verb that bootstraps the journal without lowering the
    // authored set, so the seed does not depend on the very reconciliation under
    // test.
    const bootstrap = spawnCli(
      ["history", `--database-url=${pgUrl()}`, `--schema=${schema}`, "--policy=policy.toml"],
      cwd,
    );
    assert.equal(bootstrap.status, 0, bootstrap.stderr);

    const foreignVersions = FOREIGN_ORDINALS.map(migrationIdForVersion);
    for (const [index, version] of foreignVersions.entries()) {
      await client.query(
        `INSERT INTO "${meta}".schema_migrations
           (event_kind, version, name, checksum, "by", exec_ms, phase, outcome, kind)
         VALUES ('applied', $1, $2, $3, 'peer-runner', 0, 'completed', 'success', 'apply')`,
        [version, `create_${TABLES[index]}`, `${index}`.repeat(64)],
      );
    }

    // 2. THE BUG. Every authored migration reads as pending and every journal row
    // reads as drift, on a database that is already fully migrated.
    const before = spawnCli(statusArgs(schema), cwd);
    assert.equal(
      before.status,
      1,
      `strict status must refuse this database\n${before.stdout}\n${before.stderr}`,
    );
    const beforeReply = JSON.parse(before.stdout) as StatusJson;
    assert.equal(
      beforeReply.pending.length,
      TABLES.length,
      `every authored migration reads as pending\n${before.stdout}`,
    );
    assert.deepEqual(
      beforeReply.unexpectedJournal.map((entry) => entry.version).sort(),
      [...foreignVersions].sort(),
      `every foreign journal row reads as drift\n${before.stdout}`,
    );
    assert.deepEqual(beforeReply.applied, [], before.stdout);

    // 3. Adopt. `--supersede-unmatched` is what authorises the supersession edges
    // over the foreign rows, and `--approve` is what authorises writing at all.
    const adopt = spawnCli(
      [
        "baseline",
        "--dir=.",
        `--database-url=${pgUrl()}`,
        `--schema=${schema}`,
        "--policy=policy.toml",
        "--supersede-unmatched",
        "--approve",
      ],
      cwd,
    );
    assert.equal(adopt.status, 0, `${adopt.stdout}\n${adopt.stderr}`);
    // It must SAY what it superseded, by version, before an operator can believe it.
    for (const version of foreignVersions) {
      assert.ok(
        adopt.stdout.includes(version),
        `baseline must name every superseded version; ${version} is absent\n${adopt.stdout}`,
      );
    }

    // 4. The database now reconciles clean: nothing pending, no journal drift.
    const after = spawnCli(statusArgs(schema), cwd);
    assert.equal(after.status, 0, `${after.stdout}\n${after.stderr}`);
    const afterReply = JSON.parse(after.stdout) as StatusJson;
    assert.deepEqual(afterReply.pending, [], after.stdout);
    assert.deepEqual(afterReply.unexpectedJournal, [], after.stdout);
    assert.equal(afterReply.applied.length, TABLES.length, after.stdout);

    // The foreign rows are still there -- the journal is append-only, and adoption
    // REINTERPRETS them rather than erasing them.
    const surviving = await client.query(
      `SELECT version FROM "${meta}".schema_migrations WHERE "by" = 'peer-runner'`,
    );
    assert.equal((surviving.rows as unknown[]).length, foreignVersions.length);
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(cwd, { recursive: true, force: true });
  }
});

/**
 * The realistic corpus -- the platform's own shape -- is UNGUARDED, and on the
 * database this verb exists for that leaves `status` unable to answer at all.
 *
 * `status` projects the PENDING migrations onto the live catalog before it
 * reconciles anything. Every authored step reads as pending (their ids are in the
 * derived family, the journal's are in the versioned one), so every `createTable`
 * is folded onto a catalog that already holds its table and the projection is
 * refused outright:
 *
 *   failed to project pending schema after envelope "m1_alpha":
 *   fold: table `alpha` already exists
 *
 * (crates/zeroship-migrate-node/src/lower.rs:735). The refusal is CAUSED by the
 * pending-ness, so the verb that is supposed to report the problem cannot run
 * until the problem is gone -- there is no "34 orphans and 34 pending" report to
 * read on the real corpus, only a hard stop.
 *
 * `baseline` is not subject to it, and that is deliberate: it lowers against
 * `SchemaSnapshot::default()` rather than the live catalog, precisely because the
 * lowering it would need is the one the adopted state breaks
 * (crates/zeroship-migrate-node/src/verbs.rs:1169-1184). Once it has written,
 * nothing is pending, nothing is projected, and `status` answers.
 */
test("baseline repairs a database whose status verb cannot even reconcile", async () => {
  const client = await connectLivePg();
  const cwd = temporaryDirectory(".cli-baseline-unguarded-");
  const schema = uniqueSchema("zm_baseline_unguarded");
  const meta = `${schema}_migrations`;
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    writeFileSync(join(cwd, "policy.toml"), noInjectPolicy(schema));
    TABLES.forEach((tableName, index) => {
      writeStep(
        cwd,
        `2026080100000${index + 1}_m${index + 1}_${tableName}.ts`,
        `m${index + 1}_${tableName}`,
        tableName,
        { guarded: false },
      );
    });
    for (const tableName of TABLES) {
      await client.query(`CREATE TABLE "${schema}"."${tableName}" (id integer)`);
    }
    const bootstrap = spawnCli(
      ["history", `--database-url=${pgUrl()}`, `--schema=${schema}`, "--policy=policy.toml"],
      cwd,
    );
    assert.equal(bootstrap.status, 0, bootstrap.stderr);
    const foreignVersions = FOREIGN_ORDINALS.map(migrationIdForVersion);
    for (const [index, version] of foreignVersions.entries()) {
      await client.query(
        `INSERT INTO "${meta}".schema_migrations
           (event_kind, version, name, checksum, "by", exec_ms, phase, outcome, kind)
         VALUES ('applied', $1, $2, $3, 'peer-runner', 0, 'completed', 'success', 'apply')`,
        [version, `create_${TABLES[index]}`, `${index}`.repeat(64)],
      );
    }

    // THE BUG, in its realistic form: no reconciliation at all.
    const before = spawnCli(statusArgs(schema), cwd);
    assert.equal(before.status, 1, `${before.stdout}\n${before.stderr}`);
    assert.match(before.stderr, /failed to project pending schema/, before.stderr);
    assert.match(before.stderr, /already exists/, before.stderr);
    // Not merely non-zero: there is no JSON document, so a pipeline reading this
    // gets a parse error rather than a verdict.
    assert.equal(before.stdout, "", `status must produce no reply at all: ${before.stdout}`);

    const adopt = spawnCli(
      [
        "baseline",
        "--dir=.",
        `--database-url=${pgUrl()}`,
        `--schema=${schema}`,
        "--policy=policy.toml",
        "--supersede-unmatched",
        "--approve",
      ],
      cwd,
    );
    assert.equal(adopt.status, 0, `${adopt.stdout}\n${adopt.stderr}`);

    const after = spawnCli(statusArgs(schema), cwd);
    assert.equal(after.status, 0, `${after.stdout}\n${after.stderr}`);
    const afterReply = JSON.parse(after.stdout) as StatusJson;
    assert.deepEqual(afterReply.pending, [], after.stdout);
    assert.deepEqual(afterReply.unexpectedJournal, [], after.stdout);
    assert.equal(afterReply.applied.length, TABLES.length, after.stdout);
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(cwd, { recursive: true, force: true });
  }
});

/**
 * The recorded checksum is the value a REAL apply would have written, and it is
 * the value the later drift check reads.
 *
 * `BaselineRecord.checksum` is documented "so the drift check compares correctly
 * later" (crates/zeroship-migrate-backend/src/journal.rs:533). Two facts make that
 * true rather than asserted, and this arm measures both:
 *
 *   ORACLE   -- the same corpus applied for real to an empty schema journals the
 *               same (version, checksum) pairs, byte for byte. So adoption is not
 *               recording some self-consistent value of its own; it is recording
 *               what running the migrations would have recorded.
 *   CONTROL  -- append a LATER `applied` event for one adopted version carrying a
 *               different checksum (the journal is append-only, so the newest
 *               event is the net state) and `status` flips that step to `drifted`.
 *               Without the control, "status is clean" would also hold for a
 *               build that never compared checksums at all.
 */
test("the checksum baseline records is the one a real apply writes, and drift reads it", async () => {
  const client = await connectLivePg();
  const cwd = temporaryDirectory(".cli-baseline-checksum-");
  const adopted = uniqueSchema("zm_ck_adopt");
  const fresh = uniqueSchema("zm_ck_fresh");
  const adoptedMeta = `${adopted}_migrations`;
  const freshMeta = `${fresh}_migrations`;
  try {
    for (const schema of [adopted, fresh]) {
      await client.query(`CREATE SCHEMA "${schema}"`);
    }
    // ONE policy naming both schemas would widen the charter; each run gets its own.
    writeFileSync(join(cwd, "policy.adopted.toml"), noInjectPolicy(adopted));
    writeFileSync(join(cwd, "policy.fresh.toml"), noInjectPolicy(fresh));
    TABLES.forEach((tableName, index) => {
      writeStep(
        cwd,
        `2026080100000${index + 1}_m${index + 1}_${tableName}.ts`,
        `m${index + 1}_${tableName}`,
        tableName,
        { guarded: true },
      );
    });

    // The database to adopt: schema already built, journal in the foreign family.
    for (const tableName of TABLES) {
      await client.query(`CREATE TABLE "${adopted}"."${tableName}" (id integer)`);
    }
    const bootstrap = spawnCli(
      [
        "history",
        `--database-url=${pgUrl()}`,
        `--schema=${adopted}`,
        "--policy=policy.adopted.toml",
      ],
      cwd,
    );
    assert.equal(bootstrap.status, 0, bootstrap.stderr);
    for (const [index, version] of FOREIGN_ORDINALS.map(migrationIdForVersion).entries()) {
      await client.query(
        `INSERT INTO "${adoptedMeta}".schema_migrations
           (event_kind, version, name, checksum, "by", exec_ms, phase, outcome, kind)
         VALUES ('applied', $1, $2, $3, 'peer-runner', 0, 'completed', 'success', 'apply')`,
        [version, `create_${TABLES[index]}`, `${index}`.repeat(64)],
      );
    }
    const adopt = spawnCli(
      [
        "baseline",
        "--dir=.",
        `--database-url=${pgUrl()}`,
        `--schema=${adopted}`,
        "--policy=policy.adopted.toml",
        "--supersede-unmatched",
        "--approve",
      ],
      cwd,
    );
    assert.equal(adopt.status, 0, `${adopt.stdout}\n${adopt.stderr}`);

    // The oracle: the same corpus, actually applied, on an empty schema.
    const applied = spawnCli(
      [
        "apply",
        "--dir=.",
        `--database-url=${pgUrl()}`,
        `--schema=${fresh}`,
        "--policy=policy.fresh.toml",
        "--approve",
      ],
      cwd,
    );
    assert.equal(applied.status, 0, `${applied.stdout}\n${applied.stderr}`);

    // Everything the ENGINE wrote, in event order. Filtered by what it is NOT
    // (the seeded peer rows) rather than by an actor label: `apply` records
    // `by = 'host'` (the host default) and `baseline` records `by = 'cli'`, so an
    // actor filter would silently compare one journal against an empty set.
    const journal = async (meta: string) => {
      const rows = await client.query(
        `SELECT version, name, checksum FROM "${meta}".schema_migrations
          WHERE event_kind = 'applied' AND "by" <> 'peer-runner' ORDER BY event_seq`,
      );
      return rows.rows as Array<{ version: string; name: string; checksum: string }>;
    };
    const recorded = await journal(adoptedMeta);
    const ran = await journal(freshMeta);
    assert.equal(ran.length, TABLES.length, `the oracle apply must have journaled: ${JSON.stringify(ran)}`);
    assert.equal(recorded.length, TABLES.length, JSON.stringify(recorded));
    assert.deepEqual(
      recorded.map((row) => [row.version, row.name, row.checksum]),
      ran.map((row) => [row.version, row.name, row.checksum]),
      "adoption must journal exactly the identities and checksums a real apply does",
    );

    // The control: change ONLY the recorded checksum for one adopted version.
    const target = recorded[1];
    await client.query(
      `INSERT INTO "${adoptedMeta}".schema_migrations
         (event_kind, version, name, checksum, "by", exec_ms, phase, outcome, kind)
       VALUES ('applied', $1, $2, $3, 'tamper', 0, 'completed', 'success', 'baseline')`,
      [target.version, target.name, "f".repeat(64)],
    );
    const drifted = spawnCli(
      [
        "status",
        "--dir=.",
        `--database-url=${pgUrl()}`,
        `--schema=${adopted}`,
        "--policy=policy.adopted.toml",
        "--strict",
        "--json",
      ],
      cwd,
    );
    assert.equal(drifted.status, 1, `${drifted.stdout}\n${drifted.stderr}`);
    const driftedReply = JSON.parse(drifted.stdout) as StatusJson;
    const driftedSteps = (driftedReply.plans ?? [])
      .flatMap((plan) => plan.steps ?? [])
      .filter((step) => step.state === "drifted")
      .map((step) => step.version);
    assert.deepEqual(
      driftedSteps,
      [target.version],
      `only the version whose recorded checksum changed may drift\n${drifted.stdout}`,
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${adopted}" CASCADE;
         DROP SCHEMA IF EXISTS "${adoptedMeta}" CASCADE;
         DROP SCHEMA IF EXISTS "${fresh}" CASCADE;
         DROP SCHEMA IF EXISTS "${freshMeta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(cwd, { recursive: true, force: true });
  }
});

test("baseline refuses to write without --approve, and prints what it would record", async () => {
  const client = await connectLivePg();
  const cwd = temporaryDirectory(".cli-baseline-dry-");
  const schema = uniqueSchema("zm_baseline_dry");
  const meta = `${schema}_migrations`;
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    writeFileSync(join(cwd, "policy.toml"), noInjectPolicy(schema));
    writeStep(cwd, "20260801000001_m1_alpha.ts", "m1_alpha", "alpha", { guarded: false });
    await client.query(`CREATE TABLE "${schema}"."alpha" (id integer)`);

    const refused = spawnCli(
      [
        "baseline",
        "--dir=.",
        `--database-url=${pgUrl()}`,
        `--schema=${schema}`,
        "--policy=policy.toml",
      ],
      cwd,
    );
    assert.equal(refused.status, 1, `${refused.stdout}\n${refused.stderr}`);
    assert.match(refused.stderr, /--approve/, refused.stderr);
    // The refusal is also the preview: it names the step it would have recorded.
    assert.match(refused.stdout, /create_table_alpha/, refused.stdout);

    // Nothing was written.
    const rows = await client.query(`SELECT count(*)::int AS n FROM "${meta}".schema_migrations`);
    assert.equal((rows.rows as Array<{ n: number }>)[0].n, 0, "a refused baseline writes nothing");
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(cwd, { recursive: true, force: true });
  }
});

test("baseline refuses unmatched journal rows unless --supersede-unmatched is given", async () => {
  const client = await connectLivePg();
  const cwd = temporaryDirectory(".cli-baseline-unmatched-");
  const schema = uniqueSchema("zm_baseline_unmatched");
  const meta = `${schema}_migrations`;
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    writeFileSync(join(cwd, "policy.toml"), noInjectPolicy(schema));
    writeStep(cwd, "20260801000001_m1_alpha.ts", "m1_alpha", "alpha", { guarded: false });
    await client.query(`CREATE TABLE "${schema}"."alpha" (id integer)`);

    const bootstrap = spawnCli(
      ["history", `--database-url=${pgUrl()}`, `--schema=${schema}`, "--policy=policy.toml"],
      cwd,
    );
    assert.equal(bootstrap.status, 0, bootstrap.stderr);
    const foreign = migrationIdForVersion(20260101000001n);
    await client.query(
      `INSERT INTO "${meta}".schema_migrations
         (event_kind, version, name, checksum, "by", exec_ms, phase, outcome, kind)
       VALUES ('applied', $1, 'create_alpha', $2, 'peer-runner', 0, 'completed', 'success', 'apply')`,
      [foreign, "a".repeat(64)],
    );

    const refused = spawnCli(
      [
        "baseline",
        "--dir=.",
        `--database-url=${pgUrl()}`,
        `--schema=${schema}`,
        "--policy=policy.toml",
        "--approve",
      ],
      cwd,
    );
    assert.equal(refused.status, 1, `${refused.stdout}\n${refused.stderr}`);
    assert.match(refused.stderr, /--supersede-unmatched/, refused.stderr);
    assert.ok(refused.stderr.includes(foreign), refused.stderr);

    const rows = await client.query(`SELECT count(*)::int AS n FROM "${meta}".schema_migrations`);
    assert.equal(
      (rows.rows as Array<{ n: number }>)[0].n,
      1,
      "only the seeded foreign row: the refusal wrote nothing",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(cwd, { recursive: true, force: true });
  }
});

test("baseline refuses a database whose tables the migration set has not created", async () => {
  const client = await connectLivePg();
  const cwd = temporaryDirectory(".cli-baseline-empty-");
  const schema = uniqueSchema("zm_baseline_empty");
  const meta = `${schema}_migrations`;
  try {
    // The schema exists but is EMPTY: nothing here has ever been migrated, so
    // recording "applied" for these migrations would strand the database forever.
    await client.query(`CREATE SCHEMA "${schema}"`);
    writeFileSync(join(cwd, "policy.toml"), noInjectPolicy(schema));
    writeStep(cwd, "20260801000001_m1_alpha.ts", "m1_alpha", "alpha", { guarded: false });

    const refused = spawnCli(
      [
        "baseline",
        "--dir=.",
        `--database-url=${pgUrl()}`,
        `--schema=${schema}`,
        "--policy=policy.toml",
        "--approve",
      ],
      cwd,
    );
    assert.equal(refused.status, 1, `${refused.stdout}\n${refused.stderr}`);
    assert.match(refused.stderr, /alpha/, refused.stderr);

    const rows = await client.query(`SELECT count(*)::int AS n FROM "${meta}".schema_migrations`);
    assert.equal((rows.rows as Array<{ n: number }>)[0].n, 0, "nothing was recorded");
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(cwd, { recursive: true, force: true });
  }
});

/**
 * THE PEER ENVIRONMENT: every table the corpus builds exists, and the COLUMNS are a
 * migration behind.
 *
 * This is the misuse a table-name presence check cannot see, and it is the likely
 * one: `DATABASE_URL` still exported from a staging shell, or a production database
 * a few migrations behind, where the trailing migrations only ALTER. Every table
 * name matches. Adoption journals the trailing migrations as applied, they will
 * never run, `apply` reports nothing pending, `status --strict` is green, and the
 * journal is append-only so the only repair is authoring new migrations that
 * duplicate effects the journal already claims.
 *
 * TWO SCHEMAS, ONE VARIABLE. Both are hand-built from the same DDL and differ in
 * exactly the column migration 2 adds. The CURRENT one must still adopt - a check
 * that refuses every real database is worse than none, because the operator reaches
 * for a bypass - and the BEHIND one must be refused BY NAME. "Schema does not
 * match" is useless to an operator holding two similar databases; naming
 * `users.mfa_secret` tells them which one they are pointed at.
 */
test("baseline refuses a database whose columns are behind the migration set", async () => {
  const client = await connectLivePg();
  const cwd = temporaryDirectory(".cli-baseline-peer-");
  const behind = uniqueSchema("zm_peer_behind");
  const current = uniqueSchema("zm_peer_current");
  const behindMeta = `${behind}_migrations`;
  const currentMeta = `${current}_migrations`;
  try {
    for (const schema of [behind, current]) {
      await client.query(`CREATE SCHEMA "${schema}"`);
    }
    writeFileSync(join(cwd, "policy.behind.toml"), noInjectPolicy(behind));
    writeFileSync(join(cwd, "policy.current.toml"), noInjectPolicy(current));
    // GUARDED (`ifNotExists`), for the same reason arm 1 is: it makes the repair the
    // refusal points at - re-running `apply` against the database that is behind -
    // actually available, so the last assertion below can measure it rather than
    // assert it. An unguarded create over an existing table fails outright, which
    // would prove only that the corpus is unguarded.
    writeFileSync(
      join(cwd, "20260801000001_m1_users.ts"),
      `import { table, t } from "zero-migrate";
export const name = "m1_users";
export function schema() {
  table("users").create({ columns: { id: t.int() }, ifNotExists: true });
}
`,
    );
    // The trailing migration: an ALTER, which leaves the TABLE NAME unchanged and is
    // therefore invisible to a presence-only check.
    writeFileSync(
      join(cwd, "20260801000002_m2_mfa.ts"),
      `import { table, t } from "zero-migrate";
export const name = "m2_mfa";
export function schema() {
  table("users").column("mfa_secret").add({ type: t.text() });
}
`,
    );

    // The peer environment: migration 1 applied, migration 2 not.
    await client.query(`CREATE TABLE "${behind}"."users" (id integer)`);
    // The database the corpus really describes.
    await client.query(`CREATE TABLE "${current}"."users" (id integer, mfa_secret text)`);

    const adoptArgs = (schema: string, policy: string) => [
      "baseline",
      "--dir=.",
      `--database-url=${pgUrl()}`,
      `--schema=${schema}`,
      `--policy=${policy}`,
      "--approve",
    ];

    // CONTROL: the up-to-date database still adopts. Run first, so a refusal here
    // reads as "the check is wrong" rather than as the case below passing.
    const control = spawnCli(adoptArgs(current, "policy.current.toml"), cwd);
    assert.equal(
      control.status,
      0,
      `an up-to-date database must still adopt\n${control.stdout}\n${control.stderr}`,
    );
    const controlRows = await client.query(
      `SELECT count(*)::int AS n FROM "${currentMeta}".schema_migrations WHERE event_kind = 'applied'`,
    );
    assert.equal(
      (controlRows.rows as Array<{ n: number }>)[0].n,
      2,
      "the control adoption journals both steps",
    );

    // THE CASE: --approve is genuine consent, and the database is still wrong.
    const refused = spawnCli(adoptArgs(behind, "policy.behind.toml"), cwd);
    assert.equal(
      refused.status,
      1,
      `a database whose columns are behind must be refused\n${refused.stdout}\n${refused.stderr}`,
    );
    assert.ok(
      refused.stderr.includes("users.mfa_secret"),
      `the refusal must NAME the column that differs\n${refused.stderr}`,
    );

    // Nothing was journaled, so the database is still repairable by `apply`.
    const rows = await client.query(
      `SELECT count(*)::int AS n FROM "${behindMeta}".schema_migrations`,
    );
    assert.equal((rows.rows as Array<{ n: number }>)[0].n, 0, "a refused adoption writes nothing");

    // And the repair really is available: `apply` runs the trailing migration and
    // the column arrives. Without this the refusal could be merely obstructive.
    //
    // `--registry` is needed here and not on the adoptions above, and the difference
    // is the basis each lowers against. Adoption folds from an EMPTY schema, so the
    // `createTable` in migration 1 registers `users` in the same run and migration 2
    // inherits that ownership. `apply` lowers onto the LIVE catalog, where the
    // guarded create is a no-op and migration 2's `addColumn` meets a table with no
    // registry entry - refused fail-closed. That is the ownership guard working, not
    // an artifact of this test.
    writeFileSync(join(cwd, "registry.json"), JSON.stringify({ users: "app_cli" }));
    const applied = spawnCli(
      [
        "apply",
        "--dir=.",
        `--database-url=${pgUrl()}`,
        `--schema=${behind}`,
        "--policy=policy.behind.toml",
        "--registry=registry.json",
        "--approve",
      ],
      cwd,
    );
    assert.equal(applied.status, 0, `${applied.stdout}\n${applied.stderr}`);
    const columns = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'users' ORDER BY column_name`,
      [behind],
    );
    assert.deepEqual(
      (columns.rows as Array<{ column_name: string }>).map((row) => row.column_name),
      ["id", "mfa_secret"],
      "apply is the repair the refusal points at",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${behind}" CASCADE;
         DROP SCHEMA IF EXISTS "${behindMeta}" CASCADE;
         DROP SCHEMA IF EXISTS "${current}" CASCADE;
         DROP SCHEMA IF EXISTS "${currentMeta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(cwd, { recursive: true, force: true });
  }
});

/**
 * A PREVIEW AND A WRITE ARE ONE SHAPE, and `wrote` is the only field that separates
 * them.
 *
 * That is what `BaselineReply` claims of itself
 * (crates/zeroship-migrate-node/src/wire.rs, the type's own doc) and what the host
 * contract claims of `dryRun` (packages/zero-migrate-cli/src/index.ts, `baseline`).
 * The supersession edges are the one part of an adoption with no undo at all - the
 * journal is append-only, so an edge, once written, stands for as long as its
 * carrier does - so a preview that lists them as `[]` while carrying a
 * `kind: "squash"` event was hiding exactly the part an operator is asked to approve.
 *
 * Asserted on `--json`, not on the human lines, because the reply is the only thing
 * a machine consumer sees. The human formatter used to be handed the
 * `--supersede-unmatched` flag separately and could describe the preview correctly
 * from that; a gate reading `superseded` out of the JSON to decide whether to
 * proceed had no such second source and read an empty list on every preview. The
 * flag argument is gone now, so this arm covers both surfaces at once.
 */
test("a baseline preview reports the supersession edges the write would record", async () => {
  const client = await connectLivePg();
  const cwd = temporaryDirectory(".cli-baseline-preview-");
  const schema = uniqueSchema("zm_baseline_preview");
  const meta = `${schema}_migrations`;
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    writeFileSync(join(cwd, "policy.toml"), noInjectPolicy(schema));
    TABLES.forEach((tableName, index) => {
      writeStep(
        cwd,
        `2026080100000${index + 1}_m${index + 1}_${tableName}.ts`,
        `m${index + 1}_${tableName}`,
        tableName,
        { guarded: false },
      );
    });
    for (const tableName of TABLES) {
      await client.query(`CREATE TABLE "${schema}"."${tableName}" (id integer)`);
    }
    const bootstrap = spawnCli(
      ["history", `--database-url=${pgUrl()}`, `--schema=${schema}`, "--policy=policy.toml"],
      cwd,
    );
    assert.equal(bootstrap.status, 0, bootstrap.stderr);
    const foreignVersions = FOREIGN_ORDINALS.map(migrationIdForVersion);
    for (const [index, version] of foreignVersions.entries()) {
      await client.query(
        `INSERT INTO "${meta}".schema_migrations
           (event_kind, version, name, checksum, "by", exec_ms, phase, outcome, kind)
         VALUES ('applied', $1, $2, $3, 'peer-runner', 0, 'completed', 'success', 'apply')`,
        [version, `create_${TABLES[index]}`, `${index}`.repeat(64)],
      );
    }

    const baselineArgs = (approve: boolean) => [
      "baseline",
      "--dir=.",
      `--database-url=${pgUrl()}`,
      `--schema=${schema}`,
      "--policy=policy.toml",
      "--supersede-unmatched",
      "--json",
      ...(approve ? ["--approve"] : []),
    ];

    // The preview. It exits 1 (no `--approve`) but still prints the reply first.
    const preview = spawnCli(baselineArgs(false), cwd);
    assert.equal(preview.status, 1, `${preview.stdout}\n${preview.stderr}`);
    const previewReply = JSON.parse(preview.stdout) as BaselineJson;
    assert.equal(previewReply.wrote, false, preview.stdout);
    assert.equal(
      previewReply.recorded[0]?.kind,
      "squash",
      `the first event carries the edges\n${preview.stdout}`,
    );
    assert.deepEqual(
      [...previewReply.superseded].sort(),
      [...foreignVersions].sort(),
      `a preview must list the edges the write would record\n${preview.stdout}`,
    );
    // Nothing was written: the preview is a preview.
    const afterPreview = await client.query(
      `SELECT count(*)::int AS n FROM "${meta}".schema_migrations WHERE "by" <> 'peer-runner'`,
    );
    assert.equal((afterPreview.rows as Array<{ n: number }>)[0].n, 0, "the preview wrote nothing");

    // The write. One shape: the two replies differ in `wrote` and in nothing else.
    const wrote = spawnCli(baselineArgs(true), cwd);
    assert.equal(wrote.status, 0, `${wrote.stdout}\n${wrote.stderr}`);
    const wroteReply = JSON.parse(wrote.stdout) as BaselineJson;
    assert.equal(wroteReply.wrote, true, wrote.stdout);
    assert.deepEqual(
      { ...previewReply, wrote: true },
      wroteReply,
      `the thing an operator approves must be the thing that happens\n${preview.stdout}\n${wrote.stdout}`,
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(cwd, { recursive: true, force: true });
  }
});

/**
 * THE ADOPTION THE DRIFT CHECK MUST NOT BREAK, measured rather than argued.
 *
 * The refusal above compares a fold of the corpus from an empty schema against a
 * live PostgreSQL catalog. Those two sides are produced by completely different
 * machinery - an offline renderer on one, `pg_catalog` on the other - and the
 * places they are known to disagree are catalogued at length in
 * `crates/zeroship-migrate-core/src/apply/drift.rs` (deparsed CHECK bodies, index
 * expression keys, generated-column expressions, view bodies). A structural check
 * that reported any of those as drift would refuse the ONE adoption that is
 * unambiguously legitimate, and an operator who cannot adopt a database their own
 * migrations built will go looking for a bypass.
 *
 * So this arm builds the database the honest way - `apply`, for real - then throws
 * the journal away, which is the situation adoption exists for (a peer runner's
 * history, a lost meta schema), and requires the adoption to go through. The corpus
 * carries the facets most likely to diverge: a bounded string, NOT NULL, a
 * create-time literal default, an ALTER-added column and a secondary index.
 *
 * It is the control for the whole feature: without it, "the peer environment is
 * refused" is equally satisfied by a check that refuses everything.
 */
test("baseline adopts a database this corpus itself applied", async () => {
  const client = await connectLivePg();
  const cwd = temporaryDirectory(".cli-baseline-selfbuilt-");
  const schema = uniqueSchema("zm_selfbuilt");
  const meta = `${schema}_migrations`;
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    writeFileSync(join(cwd, "policy.toml"), noInjectPolicy(schema));
    writeFileSync(join(cwd, "registry.json"), JSON.stringify({ gadgets: "app_cli" }));
    writeFileSync(
      join(cwd, "20260801000001_m1_gadgets.ts"),
      `import { table, t } from "zero-migrate";
export const name = "m1_gadgets";
export function schema() {
  table("gadgets").create({
    columns: {
      sku: t.string({ length: 64 }).notNull(),
      kind: t.string({ length: 32 }).notNull().default("widget"),
    },
  });
}
`,
    );
    writeFileSync(
      join(cwd, "20260801000002_m2_price.ts"),
      `import { table, t } from "zero-migrate";
export const name = "m2_price";
export function schema() {
  table("gadgets").column("price").add({ type: t.int() });
  table("gadgets").index("gadgets_sku_idx").add({ on: ["sku"] });
}
`,
    );

    const cliArgs = (verb: string) => [
      verb,
      "--dir=.",
      `--database-url=${pgUrl()}`,
      `--schema=${schema}`,
      "--policy=policy.toml",
      "--registry=registry.json",
      "--approve",
    ];

    const applied = spawnCli(cliArgs("apply"), cwd);
    assert.equal(applied.status, 0, `${applied.stdout}\n${applied.stderr}`);
    const ran = await client.query(
      `SELECT version, name, checksum FROM "${meta}".schema_migrations
        WHERE event_kind = 'applied' ORDER BY event_seq`,
    );
    const journaled = ran.rows as Array<{ version: string; name: string; checksum: string }>;
    assert.equal(journaled.length, 3, `three steps really ran: ${JSON.stringify(journaled)}`);

    // Lose the journal. The schema stays exactly as `apply` left it.
    await client.query(`DROP SCHEMA "${meta}" CASCADE`);

    const adopt = spawnCli(cliArgs("baseline"), cwd);
    assert.equal(
      adopt.status,
      0,
      `a database this corpus built must adopt\n${adopt.stdout}\n${adopt.stderr}`,
    );
    const readopted = await client.query(
      `SELECT version, name, checksum FROM "${meta}".schema_migrations
        WHERE event_kind = 'applied' ORDER BY event_seq`,
    );
    assert.deepEqual(
      (readopted.rows as typeof journaled).map((row) => [row.version, row.name, row.checksum]),
      journaled.map((row) => [row.version, row.name, row.checksum]),
      "adoption reconstructs the journal the apply wrote, step for step",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(cwd, { recursive: true, force: true });
  }
});
