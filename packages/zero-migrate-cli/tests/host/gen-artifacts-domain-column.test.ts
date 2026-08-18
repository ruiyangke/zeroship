// The reproduction, run end to end through the REAL addon: a column typed by a
// domain over `int` must not be described to the runtime as a string.
//
// `ColType::Domain` shared a `col_type_to_token` match arm with `ColType::Enum`, so
// every domain column reported `{"type":"string"}` in `runtimeJson` - the artifact a
// deployed app installs `env.db` from - while the database stored the domain's BASE
// type on every dialect. `envDbTs` kept `t.domain("positive_number")` all along, so
// the two artifacts of one `genArtifacts` call disagreed.
//
// This arm goes through the recorder (`buildEnvelope`, the same op image
// `recordMigrationsDir` writes) and the raw `.node`, so nothing between the authored
// `t.domain(...)` and the emitted artifacts is stubbed. No live database: the defect
// and its fix are entirely inside the DB-free fold, and the DDL the servers run was
// never at fault - it stored the base type correctly the whole time.
//
// ASSERTED ON CONTENT, never on `ok`. A column silently described as text also returns
// `ok: true`. The controls carry the weight: `t.int()` proves the token survives when
// it is NOT behind a domain, `t.text()` proves `"string"` is still reachable, and a
// SECOND domain over `varchar(40)` proves the answer is resolved rather than hardcoded.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

import { domain, table, t } from "zero-migrate";
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

/** Two domains over DIFFERENT base types, plus the two plain controls. */
const migration = {
  name: "amounts_domain",
  schema() {
    domain("positive_number").create({ as: t.int(), check: (v) => v.gt(0) });
    domain("short_code").create({ as: t.string({ length: 40 }) });
    table("amounts").create({
      columns: {
        amount: t.domain("positive_number").notNull(),
        code: t.domain("short_code").notNull(),
        weight: t.int().notNull(),
        note: t.text().notNull(),
      },
    });
  },
};

interface FieldDef {
  type?: string;
  maxLength?: number;
  min?: number;
  max?: number;
}

function fieldsFor(dialect: string): {
  fields: Record<string, FieldDef>;
  envDbTs: string;
  raw: string;
} {
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
  return { fields: parsed.collections.amounts.fields, envDbTs: reply.envDbTs as string, raw };
}

// All three dialects can express a domain column: PostgreSQL has a native
// `CREATE DOMAIN`, and SQLite/MySQL inline the base type plus the constraint into the
// column's storage. The descriptor was wrong on all three.
for (const dialect of ["sqlite", "postgres", "mysql"]) {
  test(`genArtifacts reports a domain column's base type (${dialect})`, () => {
    const { fields, envDbTs, raw } = fieldsFor(dialect);

    // The half that was already right.
    assert.match(envDbTs, /t\.domain\("positive_number"\)/, "env.db.ts keeps the domain builder");

    // THE SUBJECT: the database stores an integer.
    assert.equal(fields.amount.type, "int", `a domain over int is an integer:\n${raw}`);

    // The SECOND domain, over a different base. A single case cannot tell a fix from a
    // coincidence, and `varchar(40)` also proves the token is not the whole type.
    assert.equal(fields.code.type, "string", `a domain over varchar(40) is string-shaped:\n${raw}`);
    assert.equal(fields.code.maxLength, 40, `and carries the base type's length:\n${raw}`);

    // Control A: the token survives when the column is not behind a domain.
    assert.equal(fields.weight.type, "int", "a plain t.int() is unchanged");
    // Control B: `string` is still reachable, so the fix did not move every column off it.
    assert.equal(fields.note.type, "string", "a plain t.text() is unchanged");
    assert.notDeepEqual(
      fields.amount,
      fields.note,
      `an integer domain and free text must not be indistinguishable:\n${raw}`,
    );

    // Still dropped, pinned as current behaviour: the domain's own `CHECK (VALUE > 0)`
    // reaches no descriptor slot. `min`/`max` are INCLUSIVE, so `min: 0` would tell the
    // runtime to accept a row the database rejects.
    assert.equal(fields.amount.min, undefined, "an exclusive domain CHECK is not faked as a min");
    assert.equal(fields.amount.max, undefined, "nor as a max");
  });
}
