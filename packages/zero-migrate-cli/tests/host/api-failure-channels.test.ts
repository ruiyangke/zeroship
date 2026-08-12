// `validate()` reports failure through TWO channels, and `history()` returns a
// value that plain `JSON.stringify` cannot serialize.
//
// Both are documented in `docs/node-api.md`, and both are the kind of contract a
// caller gets wrong in a way that only shows up in production.
//
// THE TWO CHANNELS. "Invalid migration structure returns `{ ok: false, error }`.
// A missing `up()`, an exception thrown by `up()`, or a runtime setup failure
// throws normally." So a caller who writes only
//
//     const report = validate(opts);
//     if (!report.ok) throw new Error(report.error);
//
// crashes on a migration file that forgot to export `up()` - an ordinary
// authoring mistake - because that arrives as a thrown exception, not as
// `ok: false`. A caller who writes only `try { validate(opts) } catch {}` sails
// past a genuinely invalid migration, because that one returns rather than
// throws. Nothing pinned that both channels exist, so a change collapsing them
// into one would break whichever callers guessed the other way, silently.
//
// THE BIGINT. `eventSeq` is a JavaScript `bigint` "so large values remain
// exact", and `docs/node-api.md` warns that plain `JSON.stringify(audit)` throws
// and supplies a replacer. If `eventSeq` ever became a `number`, the throw would
// stop happening, the documented replacer would become pointless, and the loss of
// precision it exists to prevent would arrive silently - a passing test suite the
// whole way. Pinning the THROW is what makes that change loud.
//
// The history arm needs real events, which is why it applies a migration first.
// An empty journal serializes fine and would make the assertion vacuous - the
// first draft of this file made exactly that mistake.
//
// GATE: the history arm needs `ZERO_MIGRATE_TEST_PG_URL`; the validate arms are
// offline.

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "zero-migrate";
import { apply, history, validate } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_failure_channels";

function charter(scopeName: string): string {
  const scope = `{ include = [${JSON.stringify(scopeName)}] }`;
  return `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = ${scope}

[[grant]]
key = "schema.create_table"
value = true
scope = ${scope}
`;
}

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function validateOptions(migration: MigrationModule) {
  return {
    migration,
    ownerApp: OWNER_APP,
    projectSchema: "public",
    dialect: "postgres" as const,
    registry: {},
    policy: [charter("public")],
    nameFallback: "m",
  };
}

test("an invalid migration RETURNS ok:false rather than throwing", () => {
  // An aggregate in a DML predicate is refused by the offline validator, so it
  // is a genuine structural failure rather than a dialect capability one - a
  // capability refusal happens later, at lower time, and would return ok:true
  // here. The first draft of this file used one and measured nothing.
  const report = validate(
    validateOptions({
      name: "m",
      default: {
        up() {
          table("events").update({
            set: { tag: 1 },
            where: (col) => col("tag").count().gt(0),
          });
        },
      },
    } as MigrationModule),
  );

  assert.equal(report.ok, false, "a structurally invalid migration must RETURN, not throw");
  assert.match(
    report.error ?? "",
    /aggregate/i,
    `the returned error must say what was wrong; got ${report.error}`,
  );
});

test("a missing up() and a throwing up() THROW rather than returning ok:false", () => {
  assert.throws(
    () =>
      validate(validateOptions({ name: "m", default: {} } as MigrationModule)),
    /exports no `up\(\)` function/,
    "a module with no up() must throw, which is the channel a caller must also handle",
  );

  assert.throws(
    () =>
      validate(
        validateOptions({
          name: "m",
          default: {
            up() {
              throw new Error("boom from up");
            },
          },
        } as MigrationModule),
      ),
    /boom from up/,
    "an exception from up() must propagate unchanged",
  );
});

test("control: a valid migration returns ok:true, so neither channel fires by default", () => {
  // Without this, the two tests above also pass on a build where validate()
  // always threw, or always returned ok:false.
  const report = validate(
    validateOptions({
      name: "m",
      default: {
        up() {
          table("t").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
        },
      },
    } as MigrationModule),
  );
  assert.equal(report.ok, true);
  assert.equal(report.opCount, 1, "the authored op must have been recorded");
});

test("history() returns bigint eventSeq, so plain JSON.stringify throws", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const projectSchema = uniqueNamespace("failure_channels");
  const policy = [charter(projectSchema)];
  const driver = { kind: "postgres" as const, url: pgUrl() };

  try {
    await client.query(`CREATE SCHEMA "${projectSchema}"`);
    await apply({
      migration: {
        name: "create_t",
        default: {
          up() {
            table("t").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
          },
        },
      } as MigrationModule,
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
      registry: {},
      policy,
      approved: true,
      appliedBy: "api-failure-channels",
      nameFallback: "create_t",
    });

    const audit = await history({ ownerApp: OWNER_APP, projectSchema, driver, policy });

    // Non-vacuity first: an empty journal serializes fine, so without a real
    // event the throw below would prove nothing.
    assert.ok(audit.events.length > 0, "the applied migration must have produced an event");
    assert.equal(typeof audit.events[0].eventSeq, "bigint", "eventSeq must be a bigint");

    assert.throws(
      () => JSON.stringify(audit),
      /BigInt/,
      "plain JSON.stringify must throw, which is what the documented replacer exists for",
    );

    // The documented workaround has to actually work.
    const json = JSON.stringify(
      audit,
      (_key, value) => (typeof value === "bigint" ? value.toString() : value),
      2,
    );
    assert.match(json, /"eventSeq": "\d+"/, "the documented replacer must emit a decimal string");
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${projectSchema}" CASCADE;
         DROP SCHEMA IF EXISTS "${projectSchema}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
});
