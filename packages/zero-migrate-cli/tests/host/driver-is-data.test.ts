// The apply verb names a DRIVER, never a dialect.
//
// There is one `applyIr`. Which side owns the database connection is DATA in the
// request (`driver.kind`), the vendor is data beside it (`dialect`), and the addon
// exports no verb whose name contains a vendor. The two are independent axes: SQLite
// is simply the dialect with no JavaScript driver, and a verb named after it bakes a
// vendor into the transport.
//
// The arms below bind both halves of that:
//  - the in-process driver really applies, to a real file, through `applyIr`;
//  - it applies the WHOLE ordered sequence, not just its last envelope. That is the
//    one behaviour the host-driven arm does differently (it requires its prefix to be
//    already journalled and applies only the final envelope), so a single request
//    shape serving both is only correct while this arm stays green;
//  - a driver/dialect pairing the addon cannot serve is REFUSED rather than
//    silently reinterpreted;
//  - and no dialect-named apply verb is reachable at all.
//
// GATE: none. SQLite is an in-process file, so this arm runs in a checkout with no
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
const OWNER_APP = "app_apply_driver_is_data";
const SCHEMA = "main";

/** The raw `.node`. This arm is about the addon's own verb surface, so it must not
 *  reach it through the CLI facade that is being asked to stop naming a dialect. */
const addon = createRequire(import.meta.url)(
  process.env.ZERO_MIGRATE_ADDON_PATH as string,
) as {
  applyIr(hostDriver: unknown, req: Record<string, unknown>): Promise<{
    applied: unknown[];
    skipped: unknown[];
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

/** A scratch directory inside the test tree: the migrations import `@zeroship/migrate`,
 *  which only resolves from within the workspace. */
function scratch(prefix: string): string {
  return mkdtempSync(join(HERE, prefix));
}

test("the addon exports no apply verb named after a dialect", () => {
  const surface = addon as unknown as Record<string, unknown>;
  assert.equal(typeof surface.applyIr, "function", "the driver-neutral apply verb is the one verb");
  const dialectNamed = Object.keys(surface).filter(
    (name) => /^applyIr./.test(name) && name !== "applyIr",
  );
  assert.deepEqual(dialectNamed, [], `no apply verb may name a vendor; saw ${dialectNamed.join(",")}`);
});

test("applyIr with an in-process driver deploys the whole ordered sequence to a real file", async () => {
  const work = scratch("apply-driver-");
  try {
    const appPath = join(work, "app.db");
    const journalPath = join(work, "app.journal.db");

    const reply = await addon.applyIr(
      null,
      baseRequest({ driver: { kind: "inProcess", appPath, journalPath } }),
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

// Every refusal below is asserted as a SYNCHRONOUS throw, not a rejected promise.
//
// That is the shape, not an accident of how the assertion is written: the driver is
// decoded in the handwritten prefix each async verb runs on the napi call thread,
// before any deferred promise exists to reject. So a caller that got the request
// shape wrong is told on the call, and never through an unhandled rejection with no
// engine behind it. Engine faults are the other class and do reject.

test("the in-process driver refuses a dialect it does not serve", () => {
  const work = scratch("apply-driver-dialect-");
  try {
    assert.throws(
      () =>
        addon.applyIr(
          null,
          baseRequest({
            dialect: "postgres",
            driver: {
              kind: "inProcess",
              appPath: join(work, "app.db"),
              journalPath: join(work, "app.journal.db"),
            },
          }),
        ),
      /serves only the sqlite dialect/,
      "the driver and the dialect are independent, so an unserved pairing must refuse",
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("the driver kind and the host-driver argument must agree", () => {
  const work = scratch("apply-driver-mismatch-");
  try {
    const appPath = join(work, "app.db");
    const journalPath = join(work, "app.journal.db");

    assert.throws(
      () =>
        addon.applyIr(
          () => {
            throw new Error("the in-process driver must never call a host driver");
          },
          baseRequest({ driver: { kind: "inProcess", appPath, journalPath } }),
        ),
      /takes no host-driver callback/,
      "an in-process apply with a host callback is a caller confusion, not a preference",
    );

    assert.throws(
      () => addon.applyIr(null, baseRequest({ dialect: "postgres", driver: { kind: "host" } })),
      /requires a host-driver callback/,
      "a host-driven apply with no callback has nothing to drive",
    );

    assert.throws(
      () => addon.applyIr(null, baseRequest({ driver: { kind: "sqlite", appPath, journalPath } })),
      /unknown apply driver kind/,
      "a vendor name is not a driver kind",
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
