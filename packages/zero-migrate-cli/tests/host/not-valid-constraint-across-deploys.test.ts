// Online constraint adoption across two deploys, authored in TypeScript and run
// through the shipped host path against live PostgreSQL.
//
// F411 - the fold omitted the ` NOT VALID` tail that `pg_get_constraintdef` renders
// while `convalidated` is false, so an unvalidated foreign key reported structural
// drift for as long as it stayed unvalidated, and the declarative differ - which
// refuses any foreign-key body change outright - refused the next deploy of that
// table. The fold now records the tail and `validateConstraint` strips it again.
//
// What this file protects is the SERVER behaviour that fix mirrors: the tail is
// rendered while unvalidated, cleared by VALIDATE, and the two bodies are otherwise
// byte-identical. If PostgreSQL ever changed any of that, the fold would be wrong in
// a way no offline test could notice, because every offline test compares the fold
// against the same assumption the fold encodes.
//
// WHAT IT DOES NOT PROTECT, measured rather than assumed. This started out claiming
// to also guard F413 - the arm that strips the tail once reached its table with a
// fatal lookup, so a fold whose VALIDATE named a table an EARLIER artifact created
// failed instead of folding. Restoring that defect and rebuilding the addon, this
// file still PASSED. The reason is `lower_ordered_envelopes_to_plans_inner`
// (`crates/zero-migrate-node/src/lower.rs:588`): the Node apply path seeds its fold
// from the live catalog snapshot, so the table is in the folded map no matter which
// artifact created it, and the lookup cannot miss. That defect is reachable only
// through a bare `fold_ops` over a partial op list with no catalog-seeded base, and
// it is guarded where it lives - `render::fold::tests::
// validate_constraint_is_a_no_op_on_a_table_or_constraint_the_fold_cannot_see`.
//
// The two deploys stay, because the F411 assertions want the validate to run against
// a constraint that is already applied and journaled rather than one folded away in
// the same batch.
//
// GATE: `connectLivePg` (see `live-db.ts`).

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";
import { noInjectPolicy } from "./policy.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_not_valid_adoption";
const PARENT = "adoption_parents";
const CHILD = "adoption_children";
const FK = "adoption_children_parent_fkey";

type NamedMigration = MigrationModule & { readonly name: string };

function authoredMigration(name: string, up: () => void): NamedMigration {
  return { name, default: { up } } as NamedMigration;
}

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function pgIdent(value: string): string {
  return `"${value.replaceAll('"', '""')}"`;
}

/** Both tables plus the foreign key added NOT VALID, so the add skips the
 *  full-table scan and leaves `convalidated` false. */
function createAndAdoptUnvalidated(): NamedMigration {
  return authoredMigration("adoption_create", () => {
    table(PARENT).create({
      columns: { id: t.int().notNull() },
      primaryKey: ["id"],
    });
    table(CHILD).create({
      columns: { id: t.int().notNull(), parent_id: t.int() },
      primaryKey: ["id"],
    });
    table(CHILD)
      .foreignKey(FK)
      .add({
        columns: ["parent_id"],
        references: { table: PARENT, columns: ["id"] },
        notValid: true,
      });
  });
}

/** The validate, on its own so the table it names belongs to an applied artifact
 *  rather than to this migration's own pending ops. */
function validateTheAdoptedKey(): NamedMigration {
  return authoredMigration("adoption_validate", () => {
    table(CHILD).constraint(FK).validate();
  });
}

/** The ownership registry the SECOND deploy needs. The first creates both tables,
 *  so it registers them as it goes; a later migration targeting a table it did not
 *  create is refused fail-closed unless the registry says who owns it. */
const OWNED: Record<string, string> = { [PARENT]: OWNER_APP, [CHILD]: OWNER_APP };

function applyOne(
  migration: NamedMigration,
  projectSchema: string,
  driver: DriverConfig,
  priors: NamedMigration[],
  registry: Record<string, string> = {},
) {
  return apply({
    migration,
    priorMigrations: priors,
    priorNameFallbacks: priors.map((prior) => prior.name),
    ownerApp: OWNER_APP,
    projectSchema,
    driver,
    registry,
    policy: [noInjectPolicy(projectSchema)],
    approved: true,
    appliedBy: "not-valid-adoption-e2e",
    nameFallback: migration.name,
  });
}

test("a NOT VALID foreign key carries the tail until a later deploy validates it", async (ctx) => {
  const admin = await connectLivePg(ctx);
  if (!admin) return;

  const projectSchema = uniqueNamespace("adoption_pg");
  const meta = `${projectSchema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  /** What the catalog says about the adopted key right now. */
  const catalogState = async (): Promise<{ validated: boolean; definition: string }> => {
    const { rows } = await admin.query(
      `SELECT c.convalidated AS validated,
              pg_get_constraintdef(c.oid) AS definition
         FROM pg_catalog.pg_constraint c
         JOIN pg_catalog.pg_class t ON t.oid = c.conrelid
         JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace
        WHERE n.nspname = $1 AND t.relname = $2 AND c.conname = $3`,
      [projectSchema, CHILD, FK],
    );
    assert.equal(rows.length, 1, `the foreign key ${FK} exists in ${projectSchema}`);
    return { validated: rows[0].validated as boolean, definition: rows[0].definition as string };
  };

  try {
    await admin.query(`CREATE SCHEMA ${pgIdent(projectSchema)}`);

    const created = createAndAdoptUnvalidated();
    await applyOne(created, projectSchema, driver, []);

    const adopted = await catalogState();
    assert.equal(
      adopted.validated,
      false,
      "an ADD CONSTRAINT ... NOT VALID must leave convalidated false",
    );
    assert.ok(
      adopted.definition.endsWith(" NOT VALID"),
      `pg_get_constraintdef must render the tail while unvalidated, got ${JSON.stringify(adopted.definition)}`,
    );

    // The second deploy: a VALIDATE naming a table THIS migration never created, so
    // the constraint it validates is applied and journaled rather than folded away
    // in the same batch.
    await applyOne(validateTheAdoptedKey(), projectSchema, driver, [created], OWNED);

    const validated = await catalogState();
    assert.equal(validated.validated, true, "VALIDATE CONSTRAINT must flip convalidated");
    assert.ok(
      !validated.definition.includes("NOT VALID"),
      `pg_get_constraintdef must drop the tail once validated, got ${JSON.stringify(validated.definition)}`,
    );

    // The two bodies differ ONLY by the tail. That is what makes the fold's job a
    // suffix strip rather than a re-render, and what made omitting it a phantom diff
    // against an otherwise byte-identical definition.
    assert.equal(
      adopted.definition,
      `${validated.definition} NOT VALID`,
      "the unvalidated body must be the validated body plus the tail",
    );
  } finally {
    await admin
      .query(
        `DROP SCHEMA IF EXISTS ${pgIdent(projectSchema)} CASCADE;
         DROP SCHEMA IF EXISTS ${pgIdent(meta)} CASCADE`,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});
