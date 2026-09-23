// The verb surface names a DRIVER, never a dialect.
//
// There is one `applyIr`, one `statusIr` and one `rollback`. Which side owns the
// database connection is DATA in each request (`driver.kind`), the vendor is data
// beside it (`dialect`), and the addon exports no verb whose name contains a vendor.
// The two are independent axes: SQLite is simply the dialect with no JavaScript
// driver, and a verb named after it bakes a vendor into the transport.
//
// The arms below bind both halves of that:
//  - no verb on the whole exported surface names a vendor, so a dialect-named verb
//    added later fails here rather than being noticed by eye;
//  - each of the three really runs with an in-process driver, against a real file;
//  - `applyIr` applies the WHOLE ordered sequence on that driver, not just its last
//    envelope. That is the one behaviour its host-driven arm does differently (it
//    requires its prefix to be already journalled and applies only the final
//    envelope), so a single request shape serving both is only correct while this
//    arm stays green. `statusIr` and `rollback` have no such asymmetry: both of
//    their drivers read the same complete sequence the same way, which is what lets
//    each carry one `envelopes` field with one meaning;
//  - a driver/dialect pairing the addon cannot serve is REFUSED rather than silently
//    reinterpreted, on every verb;
//  - and each verb's driver carries only the journal credentials that verb writes
//    under, refusing the rest instead of dropping them.
//
// GATE: none. SQLite is an in-process file, so these arms run in a checkout with no
// database containers up.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtempSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { DatabaseSync } from "node:sqlite";

import { table, t } from "@zeroship/migrate";
import { buildEnvelope, type MigrationModule } from "@zeroship/migrate/internal/recorder";
import { currentIrVersion } from "zero-migrate-cli";
import { noInjectPolicy } from "./policy.js";

// The host suite builds and resolves its addon in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const OWNER_APP = "app_driver_is_data";
const SCHEMA = "main";

/** The raw `.node`. These arms are about the addon's own verb surface, so they must
 *  not reach it through the CLI facade that is being asked to stop naming a
 *  dialect. */
const addon = createRequire(import.meta.url)(
  process.env.ZERO_MIGRATE_ADDON_PATH as string,
) as {
  applyIr(hostDriver: unknown, req: Record<string, unknown>): Promise<{
    applied: unknown[];
    skipped: unknown[];
  }>;
  statusIr(hostDriver: unknown, req: Record<string, unknown>): Promise<{
    applied: string[];
    pending: string[];
    plans: unknown[] | null;
  }>;
  rollback(hostDriver: unknown, req: Record<string, unknown>): Promise<{
    rolledBack: string[];
    skippedIrreversible: string[];
  }>;
};

const CREATE_NOTES: MigrationModule = {
  name: "create_notes",
  default: {
    schema() {
      table("notes").create({
        columns: { id: t.int().required(), body: t.string({ length: 64 }).required() },
        primaryKey: ["id"],
      });
    },
  },
};

const SEED_NOTES: MigrationModule = {
  name: "seed_notes",
  default: {
    data() {
      table("notes").insert({ rows: { id: 1, body: "written through applyIr" } });
    },
    inverse() {
      table("notes").delete({ where: (col: (name: string) => { eq(v: unknown): unknown }) => col("id").eq(1) });
    },
  },
};

function envelopes(): unknown[] {
  const irVersion = currentIrVersion();
  return [
    buildEnvelope(CREATE_NOTES, { irVersion, nameFallback: "create_notes" }),
    buildEnvelope(SEED_NOTES, { irVersion, nameFallback: "seed_notes" }),
  ];
}

function baseRequest(extra: Record<string, unknown>): Record<string, unknown> {
  return {
    ownerApp: OWNER_APP,
    projectSchema: SCHEMA,
    dialect: "sqlite",
    registry: { notes: OWNER_APP },
    charterLayers: [noInjectPolicy(SCHEMA)],
    approved: true,
    envelopes: envelopes(),
    ...extra,
  };
}

/** A status request over the same authored set. It carries no `approved` flag and
 *  no rollback target, so the shared base is narrowed rather than reused whole. */
function statusRequest(extra: Record<string, unknown>): Record<string, unknown> {
  return {
    ownerApp: OWNER_APP,
    projectSchema: SCHEMA,
    dialect: "sqlite",
    registry: { notes: OWNER_APP },
    charterLayers: [noInjectPolicy(SCHEMA)],
    envelopes: envelopes(),
    readOnly: false,
    ...extra,
  };
}

/** A rollback request over the same authored set. `appliedBy` sits on the REQUEST
 *  and not on the driver: both drivers journal the `rolled_back` events under it. */
function rollbackRequest(extra: Record<string, unknown>): Record<string, unknown> {
  return {
    ownerApp: OWNER_APP,
    projectSchema: SCHEMA,
    dialect: "sqlite",
    registry: { notes: OWNER_APP },
    charterLayers: [noInjectPolicy(SCHEMA)],
    envelopes: envelopes(),
    target: { kind: "steps", steps: 1 },
    approved: true,
    force: false,
    backupAcknowledged: false,
    appliedBy: "host",
    ...extra,
  };
}

/** A scratch directory inside the test tree: the migrations import `@zeroship/migrate`,
 *  which only resolves from within the workspace. */
function scratch(prefix: string): string {
  return mkdtempSync(join(HERE, prefix));
}

/** A host-driver callback that fails if it is ever reached. Every arm that passes
 *  one is asserting a SYNCHRONOUS refusal decided before any dispatch, so a call
 *  here means the refusal did not happen where it is claimed to. */
function unreachableHostDriver(): () => void {
  return () => {
    throw new Error("the refusal under test must be decided before any dispatch");
  };
}

/** The vendor vocabulary a verb name may not borrow from.
 *
 *  A maintained list, and the control below is what keeps it honest: it asserts the
 *  matcher actually flags the dialect-named spellings this repo has carried, so a
 *  list that stopped matching anything is a red test rather than a green scan. */
const VENDORS = [
  "sqlite",
  "rusqlite",
  "postgres",
  "postgresql",
  "mysql",
  "mariadb",
  "duckdb",
  "mssql",
  "oracle",
];

function vendorIn(name: string): string | undefined {
  const lowered = name.toLowerCase();
  return VENDORS.find((vendor) => lowered.includes(vendor));
}

test("no verb on the addon's surface is named after a vendor", () => {
  const surface = addon as unknown as Record<string, unknown>;
  const verbs = Object.keys(surface).filter((name) => typeof surface[name] === "function");

  // The population is real before it is scanned: a surface that exported nothing,
  // or whose functions were not enumerable, would pass the scan below by vacuity.
  for (const verb of ["applyIr", "statusIr", "rollback"]) {
    assert.ok(verbs.includes(verb), `the driver-neutral ${verb} is on the surface`);
  }
  assert.ok(verbs.length >= 10, `the whole verb surface is scanned; saw ${verbs.length}`);

  // The matcher itself, on the spellings this repo actually carried. Without this a
  // vendor list that matched nothing would report every surface clean.
  for (const named of ["applyIrSqlite", "statusIrSqlite", "rollbackSqlite", "baselinePostgres"]) {
    assert.ok(vendorIn(named) !== undefined, `${named} names a vendor`);
  }

  const offenders = verbs.filter((name) => vendorIn(name) !== undefined);
  assert.deepEqual(offenders, [], `no verb may name a vendor; saw ${offenders.join(",")}`);
});

test("applyIr with an in-process driver deploys the whole ordered sequence to a real file", async () => {
  const work = scratch("apply-driver-");
  try {
    const appPath = join(work, "app.db");

    const reply = await addon.applyIr(
      null,
      baseRequest({ driver: { kind: "inProcess", appPath } }),
    );

    // Both envelopes, not just the last one. The host-driven arm applies only the
    // final envelope and demands the prefix be journalled already; this arm is what
    // proves the shared request shape did not quietly adopt that rule here.
    assert.equal(reply.applied.length, 2, "both authored envelopes applied");
    assert.equal(reply.skipped.length, 0, "nothing was skipped on a fresh file");

    const db = new DatabaseSync(appPath);
    try {
      const rows = db.prepare("SELECT id, body FROM notes ORDER BY id").all() as Array<
        Record<string, unknown>
      >;
      assert.equal(rows.length, 1, "the seeded row is in the file");
      assert.equal(rows[0]?.body, "written through applyIr", "and carries the authored body");
    } finally {
      db.close();
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("statusIr with an in-process driver reconciles the whole sequence against a real file", async () => {
  const work = scratch("status-driver-");
  try {
    const appPath = join(work, "app.db");

    const fresh = await addon.statusIr(
      null,
      statusRequest({ driver: { kind: "inProcess", appPath } }),
    );
    // The whole sequence is pending, and none of it is applied. Both drivers read
    // `envelopes` this way: unlike `applyIr`, no entry in it is the "current" one,
    // so a status over a fresh file reports every authored plan.
    assert.equal(fresh.applied.length, 0, "a fresh file has applied nothing");
    assert.equal(fresh.pending.length, 2, "and every authored plan is pending");

    await addon.applyIr(
      null,
      baseRequest({ driver: { kind: "inProcess", appPath } }),
    );

    const after = await addon.statusIr(
      null,
      statusRequest({ driver: { kind: "inProcess", appPath } }),
    );
    assert.equal(after.applied.length, 2, "the deploy is visible through the same verb");
    assert.equal(after.pending.length, 0, "with nothing left pending");
    assert.equal(after.plans?.length, 2, "plan-aware detail survives the in-process driver");
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("rollback with an in-process driver unwinds through the driver-neutral verb", async () => {
  const work = scratch("rollback-driver-");
  try {
    const appPath = join(work, "app.db");

    await addon.applyIr(
      null,
      baseRequest({ driver: { kind: "inProcess", appPath } }),
    );

    const reply = await addon.rollback(
      null,
      rollbackRequest({ driver: { kind: "inProcess", appPath } }),
    );
    assert.equal(reply.rolledBack.length, 1, "one step was asked for and one was unwound");
    assert.equal(reply.skippedIrreversible.length, 0, "the seeded data declares its inverse");

    const db = new DatabaseSync(appPath);
    try {
      // The authored `inverse()` ran: the row is gone, and the table the first
      // envelope created is still there, because only one step was unwound.
      const rows = db.prepare("SELECT id FROM notes").all() as Array<Record<string, unknown>>;
      assert.equal(rows.length, 0, "the seeded row was removed by the authored inverse");
    } finally {
      db.close();
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

// Every refusal below is asserted as a SYNCHRONOUS throw, not a rejected promise.
//
// That is the shape, not an accident of how the assertion is written: the driver is
// decoded in the handwritten prefix each async verb runs on the napi call thread,
// before any deferred promise exists to reject. So a caller that got the request
// shape wrong is told on the call, and never through an unhandled rejection with no
// engine behind it. Engine faults are the other class and do reject.

test("the in-process driver refuses a dialect it does not serve, on every verb", () => {
  const work = scratch("driver-dialect-");
  try {
    const driver = {
      kind: "inProcess",
      appPath: join(work, "app.db"),
    };
    const unserved = /serves only the sqlite dialect/;

    assert.throws(
      () => addon.applyIr(null, baseRequest({ dialect: "postgres", driver })),
      unserved,
      "the driver and the dialect are independent, so an unserved pairing must refuse",
    );
    assert.throws(
      () => addon.statusIr(null, statusRequest({ dialect: "postgres", driver })),
      unserved,
      "status resolves the same pairing through the same decoder",
    );
    assert.throws(
      () => addon.rollback(null, rollbackRequest({ dialect: "postgres", driver })),
      unserved,
      "and so does rollback",
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("the driver kind and the host-driver argument must agree, on every verb", () => {
  const work = scratch("driver-mismatch-");
  try {
    const appPath = join(work, "app.db");
    const inProcess = { kind: "inProcess", appPath };

    assert.throws(
      () => addon.applyIr(unreachableHostDriver(), baseRequest({ driver: inProcess })),
      /takes no host-driver callback/,
      "an in-process apply with a host callback is a caller confusion, not a preference",
    );
    assert.throws(
      () => addon.statusIr(unreachableHostDriver(), statusRequest({ driver: inProcess })),
      /takes no host-driver callback/,
      "the same confusion on status",
    );
    assert.throws(
      () => addon.rollback(unreachableHostDriver(), rollbackRequest({ driver: inProcess })),
      /takes no host-driver callback/,
      "and on rollback",
    );

    assert.throws(
      () => addon.applyIr(null, baseRequest({ dialect: "postgres", driver: { kind: "host" } })),
      /requires a host-driver callback/,
      "a host-driven apply with no callback has nothing to drive",
    );
    assert.throws(
      () =>
        addon.statusIr(null, statusRequest({ dialect: "postgres", driver: { kind: "host" } })),
      /requires a host-driver callback/,
      "nor has a host-driven status",
    );
    assert.throws(
      () =>
        addon.rollback(null, rollbackRequest({ dialect: "postgres", driver: { kind: "host" } })),
      /requires a host-driver callback/,
      "nor a host-driven rollback",
    );

    assert.throws(
      () => addon.applyIr(null, baseRequest({ driver: { kind: "sqlite", appPath } })),
      /unknown driver kind/,
      "a vendor name is not a driver kind",
    );
    assert.throws(
      () =>
        addon.statusIr(
          null,
          statusRequest({ driver: { kind: "sqlite", appPath } }),
        ),
      /unknown driver kind/,
      "on status either",
    );
    assert.throws(
      () =>
        addon.rollback(
          null,
          rollbackRequest({ driver: { kind: "sqlite", appPath } }),
        ),
      /unknown driver kind/,
      "nor on rollback",
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("each verb's driver refuses the credentials that verb does not journal under", () => {
  const work = scratch("driver-credentials-");
  try {
    const appPath = join(work, "app.db");
    const inProcess = { kind: "inProcess", appPath };

    // A status reconciles and records no row for a label to name. Its driver carries
    // neither credential, and setting one is refused rather than dropped: a caller
    // that believed it had narrowed the identity would never be told otherwise.
    assert.throws(
      () =>
        addon.statusIr(
          unreachableHostDriver(),
          statusRequest({ dialect: "postgres", driver: { kind: "host", appliedBy: "host" } }),
        ),
      /appliedBy/,
      "a status driver carries no audit label",
    );
    assert.throws(
      () =>
        addon.statusIr(
          unreachableHostDriver(),
          statusRequest({
            dialect: "postgres",
            driver: { kind: "host", migratorRole: "migrator" },
          }),
        ),
      /migratorRole/,
      "nor a narrower identity",
    );

    // A rollback's label is on the REQUEST, because both of its drivers journal the
    // `rolled_back` events under it. A second spelling on the driver would let one
    // driver read one of them.
    assert.throws(
      () =>
        addon.rollback(
          unreachableHostDriver(),
          rollbackRequest({ dialect: "postgres", driver: { kind: "host", appliedBy: "host" } }),
        ),
      /not a rollback driver field/,
      "the rollback label rides on the request, not on one driver",
    );

    // And the in-process half is the same rule for every verb: the only connection
    // there is, so no role to narrow to.
    for (const call of [
      () => addon.applyIr(null, baseRequest({ driver: { ...inProcess, migratorRole: "m" } })),
      () =>
        addon.statusIr(null, statusRequest({ driver: { ...inProcess, migratorRole: "m" } })),
      () =>
        addon.rollback(null, rollbackRequest({ driver: { ...inProcess, migratorRole: "m" } })),
    ]) {
      assert.throws(call, /carries neither migratorRole nor appliedBy/);
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
