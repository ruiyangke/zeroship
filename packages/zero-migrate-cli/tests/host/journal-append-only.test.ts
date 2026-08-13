// The migration journal is append-only, and the DATABASE enforces it.
//
// `docs/security-model.md` states the property - "Migration history is
// append-only" - and then gives advice beside it: "Do not give migration
// credentials permission to update, delete, or truncate the journal."
//
// Read together, the advice implies the property is a CONVENTION: that holding
// the grant would be enough to rewrite history, and the only thing stopping you
// is not having it. That is not what happens. A trigger refuses the write even
// for the role that owns the table, so the guarantee survives a credential that
// was over-granted by mistake - which is the case the advice is really about.
//
// Nothing measured it. `cli.test.ts` exercises the strict gate against a
// FABRICATED status reply, and the engine suite tampers through its own seam, so
// no test had ever pointed plain SQL at a live journal.
//
// The three arms are separate statements, not one: PostgreSQL treats UPDATE,
// DELETE and TRUNCATE as different privileges, and a guard that covered only the
// first two would leave the fastest way to erase history open.
//
// WHAT THIS DOES NOT CLAIM. The fourth arm records that a forged INSERT was
// rejected, but by the `schema_migrations_event_shape` CHECK - a well-formedness
// constraint, not an authenticity one. It is evidence that a careless forgery
// fails, NOT that forging a properly shaped event is impossible. The manifest
// that `security-model.md` describes for Rust hosts is what addresses that, and
// it is out of this suite's reach.
//
// GATE: `connectLivePg` (see `live-db.ts`).

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "zero-migrate";
import { apply, status, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_journal_append_only";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function charter(schema: string): string {
  const scope = `{ include = [${JSON.stringify(schema)}] }`;
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

test("the journal refuses UPDATE, DELETE and TRUNCATE from plain SQL", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const projectSchema = uniqueNamespace("append_only");
  const meta = `${projectSchema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };
  const policy = [charter(projectSchema)];

  const migration = {
    name: "create_notes",
    default: {
      up() {
        table("notes").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
      },
    },
  } as MigrationModule & { name: string };

  try {
    await client.query(`CREATE SCHEMA "${projectSchema}"`);
    await apply({
      migration,
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
      registry: {},
      policy,
      approved: true,
      appliedBy: "journal-append-only",
      nameFallback: migration.name,
    });

    const journalRows = async (): Promise<number> => {
      const { rows } = await client.query(
        `SELECT count(*)::int AS n FROM "${meta}".schema_migrations`,
      );
      return rows[0].n as number;
    };
    const before = await journalRows();
    assert.ok(before > 0, "the applied migration must have journaled an event");

    // Three separate privileges, three separate arms.
    await assert.rejects(
      client.query(
        `UPDATE "${meta}".schema_migrations SET checksum = 'deadbeef' WHERE event_kind = 'applied'`,
      ),
      /append-only \(no UPDATE\/DELETE\)/,
      "rewriting a journaled checksum must be refused by the database",
    );
    await assert.rejects(
      client.query(`DELETE FROM "${meta}".schema_migrations WHERE event_kind = 'applied'`),
      /append-only \(no UPDATE\/DELETE\)/,
      "erasing a journaled event must be refused by the database",
    );
    await assert.rejects(
      client.query(`TRUNCATE "${meta}".schema_migrations`),
      /append-only \(no UPDATE\/DELETE\)/,
      "truncating the journal must be refused too - it is a distinct privilege",
    );

    // A careless forgery is rejected as malformed. See the header: this is not an
    // authenticity guarantee, only evidence that the event shape is constrained.
    await assert.rejects(
      client.query(
        `INSERT INTO "${meta}".schema_migrations (event_kind, version, name, checksum, "by")
         VALUES ('applied', 'mig_forged', 'forged', 'cafebabe', 'attacker')`,
      ),
      /schema_migrations_event_shape/,
      "a malformed forged event must fail the event-shape check",
    );

    // Nothing moved, and status still reads clean - the refusals left no partial
    // damage behind, which is the half an operator actually depends on.
    assert.equal(await journalRows(), before, "no tampering attempt may change the journal");

    const reply = await status({
      migrations: [migration],
      nameFallbacks: [migration.name],
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
      registry: {},
      policy,
    });
    assert.equal(
      reply.plans?.[0]?.state,
      "applied",
      "the plan must still read applied after four failed tampering attempts",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${projectSchema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
});
