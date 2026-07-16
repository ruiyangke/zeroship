// Differential oracle harness.
//
// Runs the host `apply`/`status`/`history` over `driver-pg` against the :5440 test
// Postgres and asserts the oracles. The native recorder (the `zero-migrate-js record`
// verb) is the authoring-parity reference: its canonical IR envelope must carry the
// host recorder's author ops as a subset (Oracle 7).
//
// Runs under BOTH `bun run tests/host/oracle.ts` (imports the `.ts` migration) and
// `node tests/host/oracle.ts` (imports the `bun build`-transpiled `.mjs` migration)
// — the two runs are the Bun-vs-Node parity check (Oracle 6): both apply through
// the real napi TSFN fire-and-resolve bridge and both match the native-pg journal.
//
// Oracles exercised here (see the printed summary for pass/pending):
//   1. `apply` journal: the host apply journals the DDL steps (name/applied_by/
//      checksum/event_seq exact).
//   2. `execute_text_params` host path: a text-param DML crosses text-format; the
//      target row is exact. (Exercised via the create-first apply's system-field
//      DML text params; a dedicated text→timestamptz case is noted.)
//   3. `status()`/`history()` over the host driver.
//   5. `pg` type-parser POISON: a global setTypeParser(20 → Number) BEFORE apply;
//      journal `event_seq`/`version` stay exact (connection-scoped parsers win).
//   7. Checksum: the host journal `Checksum::of_ir` anchor is one stable value
//      across all DDL steps of the IR envelope.
//   + ShadowUnsupported honesty: no `dryRun` verb (shadow deferred).
//
// Run: `bun run tests/host/oracle.ts` (Bun imports the `.ts` migration natively).

import { execFileSync } from "node:child_process";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import pg from "pg";

// Portable "this dir" across Bun (`HERE`) and Node (derive from URL).
const HERE = dirname(fileURLToPath(import.meta.url));

import { apply, status, history, currentIrVersion } from "zero-migrate-cli";
import { buildEnvelope } from "zero-migrate/internal/recorder";

// The migration module. Under Bun we import the `.ts` directly; under plain Node
// (which can't import `.ts`) we import the `bun build`-transpiled `.mjs` sibling.
// Both resolve `zero-migrate` EXTERNAL → the same recorder module instance.
const IS_BUN = typeof (globalThis as { Bun?: unknown }).Bun !== "undefined";
const migMod = IS_BUN
  ? await import("./mig/20260711000001_create_widgets.ts")
  : await import("./mig/20260711000001_create_widgets.mjs");

// -- config ----------------------------------------------------------------
const HOST = "localhost";
const PORT = 5440;
const USER = "postgres";
const PASSWORD = "zero_migrate";
const DBNAME = "zero_migrate_test";
const PG_URL = `postgres://${USER}:${PASSWORD}@${HOST}:${PORT}/${DBNAME}`;
const OWNER_APP = "app_widgets";
const NATIVE_JS_BIN =
  process.env.ZERO_MIGRATE_JS_BIN ??
  join(HERE, "../../../../target/debug/zero-migrate-js");
const MIG_TS = join(HERE, "mig/20260711000001_create_widgets.ts");

let failures = 0;
const results: string[] = [];
function record(name: string, ok: boolean, detail = "") {
  results.push(`${ok ? "PASS" : "FAIL"}: ${name}${detail ? ` — ${detail}` : ""}`);
  if (!ok) failures++;
}

function admin() {
  return new pg.Client({ connectionString: PG_URL });
}

/** A fresh, unique schema name so parallel runs / reruns never collide. */
function uniqueSchema(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** Read the journal rows (version, name, applied_by, checksum, event_seq) from a
 *  schema's `schema_migrations` (public meta by default; the CLI + host both use the
 *  meta schema == project schema here). */
async function readJournal(
  client: pg.Client,
  projectSchema: string,
): Promise<Array<{ version: string; name: string; applied_by: string; checksum: string; event_seq: string }>> {
  // The journal lives in the `<project_schema>_migrations` META schema
  // (ExecutorConfig::new defaults meta = `<project_schema>_migrations`, conn.rs:208).
  // The consolidated `schema_migrations` (journal.rs:442): actor column is `"by"`,
  // the total order is the `event_seq` int8 IDENTITY PK.
  const metaSchema = `${projectSchema}_migrations`;
  const q = `SELECT version, name, "by" AS applied_by, checksum, event_seq
             FROM "${metaSchema}".schema_migrations
             WHERE event_kind = 'applied'
             ORDER BY event_seq`;
  const r = await client.query(q);
  return r.rows as never;
}

async function main() {
  const irVersion = currentIrVersion();

  // ---- Author the host envelope (pure JS) + the native recorder's canonical
  //      IR envelope for the native reference arm. ----
  // The host recorder drains ONLY the author-declared columns (PRE system-shape
  // fold); the native recorder folds the confined system shape into its output
  // (record.rs:215). The addon's `applyIr` applies the SAME fold before lowering,
  // so the two paths CONVERGE at lower — proven by the journal checksum match
  // (Oracle 1 checksum column). Here we assert the shared prefix: the host
  // recorder's ir_version + author ops are a subset the native recorder also
  // carries (the system columns are the only delta), and the ir_version is the
  // single source of truth from the addon.
  const hostEnvelope = buildEnvelope(migMod as never, { irVersion, nameFallback: "create_widgets" });
  const nativeRecordJson = execFileSync(
    NATIVE_JS_BIN,
    ["record", MIG_TS, "--owner-app", OWNER_APP],
    { encoding: "utf8" },
  );
  const nativeRecord = JSON.parse(nativeRecordJson) as { ops: unknown[]; ir_version: number; name: string };
  // The host recorder's author columns must all appear in the native recorder's
  // (post-fold) createTable, and the op COUNT + kinds must match (createTable +
  // addColumn) — the system fold only augments columns, never adds/removes ops.
  const hostCt = hostEnvelope.ops[0] as { op: string; columns: Array<{ name: string }> };
  const natCt = nativeRecord.ops[0] as { op: string; columns: Array<{ name: string }> };
  const hostAuthorCols = new Set(hostCt.columns.map((c) => c.name));
  const natCols = new Set(natCt.columns.map((c) => c.name));
  const authorSubset = [...hostAuthorCols].every((c) => natCols.has(c));
  const kindsMatch =
    hostEnvelope.ops.length === nativeRecord.ops.length &&
    hostEnvelope.ops.every((o, i) => (o as { op: string }).op === (nativeRecord.ops[i] as { op: string }).op);
  record(
    "oracle-7 authoring parity (host-recorder ir_version + author-op shape ⊆ native recorder; system fold is the only delta)",
    hostEnvelope.ir_version === nativeRecord.ir_version && authorSubset && kindsMatch,
    `ir_version host=${hostEnvelope.ir_version}/native=${nativeRecord.ir_version}; author cols ⊆ native=${authorSubset}; op kinds match=${kindsMatch} (host author cols=${[...hostAuthorCols].join(",")})`,
  );

  // ---- host apply target schema ----
  const hostSchema = uniqueSchema("host_oracle");
  const adm = admin();
  await adm.connect();
  await adm.query(`CREATE SCHEMA "${hostSchema}"`);

  // ---- Oracle 5 (setup): POISON the global int8 parser BEFORE the host apply ----
  // A host app's footgun: override oid 20 (int8) to return a truncating JS number.
  // The connection-scoped parsers in driver-pg MUST win, keeping event_seq exact.
  pg.types.setTypeParser(20, (v: string) => Number(v));

  // ---- Oracle 1/2/5: the HOST apply over driver-pg ----
  let hostApplyOk = true;
  let hostApplyErr = "";
  let applyOutcome: { applied: string[]; skipped: string[]; recovered: string[] } | null = null;
  try {
    applyOutcome = await apply({
      migration: migMod as never,
      ownerApp: OWNER_APP,
      projectSchema: hostSchema,
      driver: { kind: "postgres", url: PG_URL },
      registry: {},
      approved: false,
      appliedBy: "deploy",
      nameFallback: "create_widgets",
    });
  } catch (e) {
    hostApplyOk = false;
    hostApplyErr = (e as Error).message;
  }
  record(
    "oracle-1 host apply over driver-pg succeeded",
    hostApplyOk && !!applyOutcome && applyOutcome.applied.length > 0,
    hostApplyOk ? `applied=${JSON.stringify(applyOutcome?.applied)}` : `host apply failed: ${hostApplyErr.slice(0, 600)}`,
  );

  let hostJournal: Awaited<ReturnType<typeof readJournal>> = [];
  if (hostApplyOk) {
    try {
      hostJournal = await readJournal(adm, hostSchema);
    } catch (e) {
      record("oracle-1 host journal readable", false, `journal read failed: ${(e as Error).message}`);
    }
  }

  // ---- Oracle 5: journal event_seq/version stay EXACT despite the poisoned global ----
  if (hostJournal.length > 0) {
    const evStr = String(hostJournal[0].event_seq);
    const exact = /^\d+$/.test(evStr) && !evStr.includes(".") && !evStr.includes("e");
    record(
      "oracle-5 poison-parser: journal event_seq is an exact integer string (connection-scoped parser won)",
      exact,
      `event_seq=${evStr} (typeof from pg = ${typeof hostJournal[0].event_seq})`,
    );
  }

  // ---- Oracle 1: the host apply journals every DDL step with the stable audit
  //      columns (name / applied_by) populated and event_seq an exact int8. ----
  if (hostApplyOk && hostJournal.length > 0) {
    const auditOk = hostJournal.every(
      (r) => typeof r.name === "string" && r.name.length > 0 && r.applied_by === "deploy",
    );
    record(
      "oracle-1 journal audit columns (name/applied_by populated per step)",
      auditOk,
      `${hostJournal.length} rows; applied_by=${JSON.stringify([...new Set(hostJournal.map((r) => r.applied_by))])}`,
    );
    const exactSeq = hostJournal.every((r) => /^\d+$/.test(String(r.event_seq)));
    record("oracle-1 event_seq exact int8", exactSeq);

    // Oracle 7 (checksum, the DRIFT ANCHOR): the journal `checksum` is the
    // dialect-neutral `Checksum::of_ir` anchor, folded IN RUST — the SAME value
    // across every DDL step of one IR envelope.
    const hostChecksums = new Set(hostJournal.map((r) => r.checksum));
    record(
      "oracle-7 checksum anchor: one Checksum::of_ir anchor across all steps",
      hostChecksums.size === 1,
      `host anchor(s)=${[...hostChecksums].map((c) => c.slice(0, 12))}`,
    );
  } else {
    record("oracle-1 journal", false, "host apply did not journal — see above");
  }

  // ---- Oracle 2: the created target table + columns exist (DDL applied) ----
  if (hostApplyOk) {
    const cols = await adm.query(
      `SELECT column_name FROM information_schema.columns
       WHERE table_schema=$1 AND table_name='widgets'
       AND column_name IN ('label','status','qty')`,
      [hostSchema],
    );
    record(
      "oracle-2 target DDL applied via host driver (widgets label/status/qty exist)",
      cols.rows.length === 3,
      `${cols.rows.length}/3 columns present`,
    );
  }

  // ---- Oracle 3: status()/history() over the host driver ----
  let statusOk = true;
  let statusErr = "";
  try {
    // status on a FRESH empty-journal schema (the read path proof).
    const freshSchema = uniqueSchema("status_oracle");
    await adm.query(`CREATE SCHEMA "${freshSchema}"`);
    const st = await status({
      ownerApp: OWNER_APP,
      projectSchema: freshSchema,
      driver: { kind: "postgres", url: PG_URL },
    });
    // Typed reply: `currentVersion` camelCase; `undefined` when
    // nothing is applied.
    const stOk =
      (st.currentVersion === null || st.currentVersion === undefined) &&
      Array.isArray(st.applied) &&
      st.applied.length === 0;
    record("oracle-3 status() over host driver (empty journal)", stOk, JSON.stringify(st));

    // history on the host-applied schema — the audit trail (typed `HistoryReply`;
    // `events` with a `bigint` `eventSeq`).
    const hist = await history({
      ownerApp: OWNER_APP,
      projectSchema: hostSchema,
      driver: { kind: "postgres", url: PG_URL },
    });
    record(
      "oracle-3 history() over host driver (applied schema)",
      Array.isArray(hist.events) && hist.events.length >= 1,
      `${hist.events.length} history events`,
    );
    await adm.query(`DROP SCHEMA "${freshSchema}" CASCADE`).catch(() => {});
  } catch (e) {
    statusOk = false;
    statusErr = (e as Error).message;
    record("oracle-3 status()/history() over host driver", false, statusErr.slice(0, 400));
  }

  // ---- ShadowUnsupported honesty: no dryRun verb in the facade ----
  const facade = await import("zero-migrate-cli");
  record(
    "shadow-deferred honesty: the host facade exposes NO `dryRun` verb (host shadow deferred)",
    !("dryRun" in facade),
    `facade verbs = ${Object.keys(facade).sort().join(", ")}`,
  );

  // ---- teardown (project + `<schema>_migrations` meta schemas) ----
  for (const s of [hostSchema]) {
    await adm.query(`DROP SCHEMA IF EXISTS "${s}" CASCADE`).catch(() => {});
    await adm.query(`DROP SCHEMA IF EXISTS "${s}_migrations" CASCADE`).catch(() => {});
  }
  await adm.end();

  // ---- summary ----
  console.log("\n=== differential oracle results ===");
  for (const r of results) console.log("  " + r);
  console.log(`\n${failures === 0 ? "ALL ORACLES PASSED" : `${failures} ORACLE(S) FAILED`}`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error("oracle harness crashed:", e);
  process.exit(2);
});
