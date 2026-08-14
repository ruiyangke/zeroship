// What a JavaScript migration module can and cannot declare.
//
// `MigrationModule` accepts `name` plus one of the migration protocol phase shapes,
// either at the module root or under `default`. Two IR-level fields that the docs
// describe at length are NOT on that surface, and this file pins the boundary so
// it stops being discoverable only by reading `internal/recorder.ts`:
//
//   * `dependsOn` - described in nine documents, including `getting-started.md`
//     and `README.md`. Three of them give a post-abort recovery instruction
//     ("update the dependency to that new migration identity") that a JavaScript
//     author cannot perform, because there is no dependency to repoint.
//   * `repeatable` - the engine carries a full repeatable pipeline (partition,
//     ordered repeatable phase, per-dialect re-run oracle, the flip-flag tamper
//     guard pinned in `pg_scenarios.rs`) that a JavaScript author cannot reach.
//
// Neither is a defect in the engine. Both are IR-level facilities available to
// Rust embedders through `IrAuthor` and to hand-authored IR envelopes, and the
// gap is that the JavaScript-facing docs described them without saying so.
//
// THE ASSERTIONS ARE DELIBERATELY ABOUT THE BUILT ENVELOPE, not about a thrown
// error. A module carrying `dependsOn` is not rejected - it is accepted and the
// property is silently ignored, which is the part worth pinning: an author who
// writes one gets a green deploy and no dependency ordering. If the authoring
// surface ever grows these fields, this file fails and points whoever added them
// at the docs that already describe the semantics.
//
// GATE: none. `validate()` and `plan()` are offline.

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "zero-migrate";
import { plan, validate } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const CHARTER = `policy_version = 1

[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["public"] }
`;

/** A module that tries every plausible spelling of the two absent fields. */
function overreachingModule(): MigrationModule {
  return {
    name: "with_extras",
    dependsOn: ["mig_AAAA"],
    depends_on: ["mig_BBBB"],
    repeatable: true,
    default: {
      dependsOn: ["mig_CCCC"],
      repeatable: true,
      schema() {
        table("x").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
      },
    },
  } as unknown as MigrationModule;
}

function options(migration: MigrationModule) {
  return {
    migration,
    ownerApp: "app_module_surface",
    projectSchema: "public",
    dialect: "postgres" as const,
    registry: {},
    policy: [CHARTER],
    nameFallback: "with_extras",
  };
}

test("a module declaring dependsOn or repeatable is accepted, and both are ignored", () => {
  const verdict = validate(options(overreachingModule()));
  assert.equal(verdict.ok, true, "the extra properties must not make the module invalid");

  const report = plan(options(overreachingModule()));
  assert.equal(report.ok, true, "planning must succeed too");

  // The envelope is the engine's own record of what it built. Neither field
  // reaches it under any spelling.
  const envelope = JSON.stringify(report);
  assert.equal(
    envelope.match(/depends[_o]/gi),
    null,
    `no dependency list may reach the built envelope; found ${envelope.match(/depends[_o]\w*/gi)}`,
  );
  assert.equal(
    envelope.match(/"repeatable"\s*:\s*true/gi),
    null,
    "no repeatable flag may reach the built envelope",
  );
});

test("control: the fields a module CAN declare do reach the engine", () => {
  // Without this, the assertions above also pass for a build where the module
  // surface was broken entirely and nothing reached the envelope.
  const report = plan(
    options({
      name: "plain",
      default: {
        schema() {
          table("widgets").create({
            columns: { id: t.int().notNull() },
            primaryKey: ["id"],
          });
        },
      },
    } as MigrationModule),
  );
  assert.equal(report.ok, true);
  assert.equal(report.op_count, 1, "the authored op must have been recorded");
  assert.match(
    JSON.stringify(report),
    /widgets/,
    "the authored table name must reach the built envelope",
  );
});
