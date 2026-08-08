// Live-DB proof that `genArtifacts` folds under the PROJECT'S REAL target dialect.
//
// One authored history carries an op-level `dialect({ pg, mysql })` leg, so the
// column set the migration actually produces DIFFERS per target. The migration is
// applied to a real server, the live catalog is read back, and the generated
// `schema.runtime.json` field set is compared against that ground truth. An
// artifact folded under the wrong dialect names a column the database does not
// have, and misses the one it does.
//
// The MySQL arm is the one the dialect threading fixes; the Postgres arm is the
// CONTROL that already passed under the old hard-coded Postgres fold, so a failure
// on both arms means the harness is broken rather than the fold.
//
// NOT covered here: the SQLite target (no host-driver apply path - it runs
// in-process), the descriptor/manual `genArtifacts` source (a declared
// `CollectionDescriptor` set cannot express a dialectal leg), and expression-level
// `dialect()` legs (this exercises the op-level leg only).

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import { dialect, table, t } from "zero-migrate";
import { buildEnvelope } from "zero-migrate/internal/recorder";
import { apply, currentIrVersion } from "zero-migrate-cli";
import { noInjectPolicy } from "./policy.js";
import { connectLivePg, pgUrl } from "./live-db.js";

const HERE = dirname(fileURLToPath(import.meta.url));

if (!process.env.ZERO_MIGRATE_ADDON_PATH) {
  const { platform, arch } = process;
  const abi = platform === "linux" ? "-gnu" : "";
  process.env.ZERO_MIGRATE_ADDON_PATH = join(
    HERE,
    `../../../../crates/zero-migrate-node/zero-migrate-node.${platform}-${arch}${abi}.node`,
  );
}

interface GenArtifactsReply {
  ok: boolean;
  envDbTs?: string;
  runtimeJson?: string;
  error?: string;
}

interface GenArtifactsAddon {
  genArtifacts(source: {
    envelopes?: unknown[];
    projectSchema?: string;
    charterLayers: string[];
    dialect: string;
  }): GenArtifactsReply;
}

// The raw `.node` is loaded directly: `zero-migrate-cli` re-exports the apply/status
// verbs but not the DB-free artifact emitter, so there is no CLI seam to ride here.
const addon = createRequire(import.meta.url)(
  process.env.ZERO_MIGRATE_ADDON_PATH as string,
) as GenArtifactsAddon;

const PG_URL = pgUrl();
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;

/** A fresh, unique name so parallel runs / reruns never collide. MySQL identifiers
 *  cap at 64 chars and `<db>_migrations` must also fit, so keep it short. */
function uniqueName(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** `widgets(label)` plus a per-dialect column: `pg_only` on Postgres, `mysql_only`
 *  on MySQL. The two legs are the observable divergence - nothing else about the
 *  table differs between targets. */
const migration = {
  name: "widgets_dialectal",
  up() {
    table("widgets").create({ columns: { label: t.text() } });
    dialect({
      pg: () => table("widgets").column("pg_only").add({ type: t.text() }),
      mysql: () => table("widgets").column("mysql_only").add({ type: t.text() }),
    });
  },
};

/** Every column either leg can contribute. The one the target did NOT take must be
 *  absent from both artifacts, and `live` decides which that is. */
const DIALECTAL_LEG_COLUMNS = ["pg_only", "mysql_only"] as const;

/** Generate both artifacts for `target` and assert each describes exactly the live
 *  column set. `schema.runtime.json` is compared structurally; `env.db.ts` is a
 *  rendered `t.*()` source, so it is probed for the column keys rather than parsed -
 *  that pins WHICH columns it declares, not their rendered builder calls. */
function assertArtifactsMatchLive(projectSchema: string, target: string, live: Set<string>): void {
  const envelope = buildEnvelope(migration, { irVersion: currentIrVersion() });
  const reply = addon.genArtifacts({
    envelopes: [envelope],
    projectSchema,
    charterLayers: [noInjectPolicy(projectSchema)],
    dialect: target,
  });
  assert.ok(reply.ok, `genArtifacts(${target}) ok: ${reply.error}`);

  const parsed = JSON.parse(reply.runtimeJson as string) as {
    collections: Record<string, { fields: Record<string, unknown> }>;
  };
  assert.deepEqual(
    Object.keys(parsed.collections.widgets.fields).sort(),
    [...live].sort(),
    `the ${target} schema.runtime.json field set equals the live column set`,
  );

  const source = reply.envDbTs as string;
  for (const column of live) {
    assert.match(source, new RegExp(`\\b${column}:`), `${target} env.db.ts declares ${column}`);
  }
  for (const column of DIALECTAL_LEG_COLUMNS.filter((c) => !live.has(c))) {
    assert.doesNotMatch(
      source,
      new RegExp(`\\b${column}:`),
      `${target} env.db.ts omits ${column}, which the live database does not have`,
    );
  }
}

test("genArtifacts folds the MySQL target's own dialectal leg (matches the live MySQL catalog)", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset - live-MySQL artifact-dialect arm skipped");
    return;
  }

  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  const database = uniqueName("gad_my");
  const meta = `${database}_migrations`;

  try {
    await admin.query(`CREATE DATABASE \`${database}\``);
    await apply({
      migration: migration as never,
      ownerApp: "app_gen_artifacts_dialect",
      projectSchema: database,
      driver: { kind: "mysql", url: MYSQL_URL },
      registry: {},
      policy: [noInjectPolicy(database)],
      approved: false,
      appliedBy: "gen-artifacts-dialect",
      nameFallback: "widgets_dialectal",
    });

    const [rows] = await admin.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = ? AND table_name = 'widgets'`,
      [database],
    );
    const live = new Set(
      (rows as Array<{ column_name?: string; COLUMN_NAME?: string }>).map(
        (r) => (r.column_name ?? r.COLUMN_NAME) as string,
      ),
    );
    // Guard the harness itself: if MySQL did not take the MySQL leg there is no
    // divergence left to observe and the arm proves nothing.
    assert.ok(live.has("mysql_only"), `live MySQL widgets has mysql_only (got ${[...live]})`);
    assert.ok(!live.has("pg_only"), `live MySQL widgets has no pg_only (got ${[...live]})`);

    assertArtifactsMatchLive(database, "mysql", live);
  } finally {
    await admin
      .query(`DROP DATABASE IF EXISTS \`${database}\`; DROP DATABASE IF EXISTS \`${meta}\``)
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});

test("genArtifacts folds the Postgres target's own dialectal leg (matches the live Postgres catalog)", async (ctx) => {
  // `connectLivePg` owns the connect, so it can tell "this machine has no database"
  // (skip) from "a database was configured and did not work" (throw); the client it
  // hands back is closed here, and on a skip there is no client to close.
  const admin = await connectLivePg(ctx);
  if (!admin) return;
  const schema = uniqueName("gad_pg");

  try {
    await admin.query(`CREATE SCHEMA "${schema}"`);
    await apply({
      migration: migration as never,
      ownerApp: "app_gen_artifacts_dialect",
      projectSchema: schema,
      driver: { kind: "postgres", url: PG_URL },
      registry: {},
      policy: [noInjectPolicy(schema)],
      approved: false,
      appliedBy: "gen-artifacts-dialect",
      nameFallback: "widgets_dialectal",
    });

    const res = await admin.query<{ column_name: string }>(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'widgets'`,
      [schema],
    );
    const live = new Set(res.rows.map((r) => r.column_name));
    assert.ok(live.has("pg_only"), `live Postgres widgets has pg_only (got ${[...live]})`);
    assert.ok(!live.has("mysql_only"), `live Postgres widgets has no mysql_only (got ${[...live]})`);

    assertArtifactsMatchLive(schema, "postgres", live);
  } finally {
    await admin.query(`DROP SCHEMA IF EXISTS "${schema}" CASCADE`).catch(() => {});
    await admin.query(`DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`).catch(() => {});
    await admin.end().catch(() => {});
  }
});
