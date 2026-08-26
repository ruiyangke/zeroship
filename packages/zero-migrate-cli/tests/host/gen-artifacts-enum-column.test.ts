// The reported reproduction, run end to end through the REAL addon: the two
// artifacts one `genArtifacts` call emits must agree about a `t.enum` column.
//
// `envDbTs` rendered `t.enum("issue_status")` while `runtimeJson` - the artifact a
// deployed app installs `env.db` from - described the same column as a bare
// `{"type":"string"}`, dropping the closed set the database enforces. The two halves
// come out of ONE fold of ONE op stream, so they cannot both be right.
//
// This arm goes through the recorder (`buildEnvelope`, the same op image
// `recordMigrationsDir` writes) and the raw `.node`, so nothing between the authored
// `t.enum(...)` and the emitted artifacts is stubbed. No live database: the defect
// and its fix are entirely inside the DB-free fold, and the DDL the servers run was
// never the thing at fault.
//
// ASSERTED ON CONTENT, never on `ok`. A silently dropped enum also returns
// `ok: true`, which is exactly how this shipped. `t.text()` is the control that makes
// the loss legible - `status` and `summary` were byte-identical in `runtimeJson` -
// and `t.int()` is the control that refutes "the descriptor is coarse on purpose".

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

import { enumType, table, t } from "zero-migrate";
import { buildEnvelope } from "zero-migrate/internal/recorder";
import { currentIrVersion } from "zero-migrate-cli";
import { noInjectPolicy } from "./policy.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

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

const addon = createRequire(import.meta.url)(
  process.env.ZERO_MIGRATE_ADDON_PATH as string,
) as GenArtifactsAddon;

const SCHEMA = "public";
const MEMBERS = ["UNCONFIRMED", "CONFIRMED", "RESOLVED"];

/** The reported schema: the `t.enum` subject plus the two controls. */
const migration = {
  name: "issues_enum",
  schema() {
    enumType("issue_status").create({ values: MEMBERS });
    table("issues").create({
      columns: {
        status: t.enum("issue_status").notNull().default("UNCONFIRMED"),
        summary: t.text().notNull(),
        weight: t.int().notNull().default(0),
      },
    });
  },
};

interface FieldDef {
  type?: string;
  enum?: unknown[];
  default?: unknown;
}

function fieldsFor(dialect: string): { fields: Record<string, FieldDef>; envDbTs: string; raw: string } {
  const envelope = buildEnvelope(migration, { irVersion: currentIrVersion() });
  const reply = addon.genArtifacts({
    envelopes: [envelope],
    projectSchema: SCHEMA,
    charterLayers: [noInjectPolicy(SCHEMA)],
    dialect,
  });
  assert.ok(reply.ok, `genArtifacts(${dialect}) ok: ${reply.error}`);
  const raw = reply.runtimeJson as string;
  const parsed = JSON.parse(raw) as {
    collections: Record<string, { fields: Record<string, FieldDef> }>;
  };
  return { fields: parsed.collections.issues.fields, envDbTs: reply.envDbTs as string, raw };
}

// SQLite enforces the set with `TEXT ... CHECK ("status" IN (...))` and PostgreSQL
// with a native enum type. Different mechanisms, so both are asserted.
for (const dialect of ["sqlite", "postgres"]) {
  test(`genArtifacts keeps the enum membership in BOTH artifacts (${dialect})`, () => {
    const { fields, envDbTs, raw } = fieldsFor(dialect);

    assert.match(envDbTs, /t\.enum\("issue_status"\)/, "env.db.ts keeps the enum builder");

    assert.deepEqual(
      fields.status.enum,
      MEMBERS,
      `schema.runtime.json carries the members in declaration order:\n${raw}`,
    );
    assert.equal(fields.status.type, "string", "the enum column's storage token is unchanged");
    assert.equal(fields.status.default, "UNCONFIRMED", "the enum column keeps its default");

    // Control A: a closed set and free text must stop serializing identically.
    assert.equal(fields.summary.type, "string", "control A stays a plain string");
    assert.equal(fields.summary.enum, undefined, "control A gains no membership");
    assert.notDeepEqual(
      fields.status,
      fields.summary,
      `a closed set and free text must not be indistinguishable:\n${raw}`,
    );

    // Control B: the descriptor does preserve a narrowing it cannot express
    // downstream, so "enum is dropped because the descriptor is coarse" was never
    // the rule.
    assert.equal(fields.weight.type, "int", "control B keeps its integer token");
  });
}
