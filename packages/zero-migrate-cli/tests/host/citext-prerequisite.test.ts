// The portable case-insensitive column, on a PostgreSQL that has not installed
// `citext`.
//
// `t.text({ caseSensitive: false })` is what BOTH the documentation and the engine's
// own refusal message tell an author to write. `docs/dialects.md` says it "renders
// `citext`/`COLLATE NOCASE`/`utf8mb4_0900_ai_ci` respectively"; the validate error
// for a bounded case-insensitive string says "declare the column as
// `t.text({ caseSensitive: false })`". Neither mentions that `citext` is a contrib
// extension PostgreSQL does not install by default.
//
// On a stock database it therefore fails at APPLY, mid-deploy, with a raw server
// error rather than an authoring one:
//
//   migration <name> failed to apply: type "public.citext" does not exist
//
// Nothing caught this because every existing suite that authors a case-insensitive
// column runs under a charter that creates the extension first
// (`support::no_inject_with_extensions(schema, &["citext"])`). The prerequisite is
// real, it is just always already satisfied where it is tested.
//
// This arm asserts the CURRENT behaviour, and it is written to fail loudly when that
// behaviour improves rather than to bless it. If the engine learns to refuse this at
// validate, or to emit the extension itself under a `code.extension` grant, the
// assertion below stops matching and this file is the place to record which of those
// shipped.
//
// GATE: `connectLivePg` (see `live-db.ts`). This arm also holds the `citext` claim
// (`extension-claim.ts`) for its whole body, because an absence is what it measures and
// an extension is a database-wide object no sibling run can be renamed away from.

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { claim, release } from "./extension-claim.js";
import { connectLivePg, pgUrl } from "./live-db.js";
import { noInjectPolicy } from "./policy.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_citext_prereq";

/** The extension whose ABSENCE this arm is about, and whose claim it takes. */
const EXTENSION = "citext";

type NamedMigration = MigrationModule & { readonly name: string };

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function pgIdent(value: string): string {
  return `"${value.replaceAll('"', '""')}"`;
}

function caseInsensitiveMigration(): NamedMigration {
  return {
    name: "case_insensitive_email",
    default: {
      schema() {
        table("contacts").create({
          columns: { email: t.text({ caseSensitive: false }) },
        });
      },
    },
  } as NamedMigration;
}

test("the documented case-insensitive spelling fails at apply when citext is not installed", async () => {
  const admin = await connectLivePg();

  const projectSchema = uniqueNamespace("citext_prereq");
  const meta = `${projectSchema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  try {
    // THIS ARM'S SUBJECT IS AN ABSENCE, which makes it the one shape a per-run name
    // cannot protect: an extension is installed per DATABASE, so a sibling gate run
    // that installs `citext` for its own case turns this arm's question into one it
    // cannot ask. It takes the same claim the installers take (see
    // `extension-claim.ts`), which both excludes them for the duration AND drops any
    // leftover, so the absence below is established rather than hoped for.
    //
    // There used to be a `ctx.skip` here for the installed case. It was the honest
    // thing to write without the claim and the wrong thing to keep with it: a skip and
    // a pass print the same exit code, so a suite that quietly stopped asking this
    // question would report exactly like one that asked it and got the answer.
    await claim(admin, EXTENSION);

    const { rows: installed } = await admin.query(
      `SELECT 1 FROM pg_catalog.pg_extension WHERE extname = $1`,
      [EXTENSION],
    );
    assert.equal(
      installed.length,
      0,
      `${EXTENSION} is installed despite this arm holding the claim that clears it, so ` +
        `the prerequisite gap below cannot be observed and this run would assert nothing`,
    );

    await admin.query(`CREATE SCHEMA ${pgIdent(projectSchema)}`);

    await assert.rejects(
      apply({
        migration: caseInsensitiveMigration(),
        priorMigrations: [],
        priorNameFallbacks: [],
        ownerApp: OWNER_APP,
        projectSchema,
        driver,
        registry: {},
        policy: [noInjectPolicy(projectSchema)],
        approved: true,
        appliedBy: "citext-prerequisite",
        nameFallback: "case_insensitive_email",
      }),
      /type "public\.citext" does not exist/,
      "the recommended spelling currently fails at apply, not at validate",
    );

    // Where it failed matters as much as that it failed: this is a server error
    // raised while executing the migration, so an operator sees a partially-run
    // deploy rather than a refusal before anything started.
    const { rows } = await admin.query(
      `SELECT 1 FROM pg_catalog.pg_class c
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = $1 AND c.relname = 'contacts'`,
      [projectSchema],
    );
    assert.equal(rows.length, 0, "the failed create left no table behind");
  } finally {
    await release(admin, EXTENSION);
    await admin
      .query(
        `DROP SCHEMA IF EXISTS ${pgIdent(projectSchema)} CASCADE;
         DROP SCHEMA IF EXISTS ${pgIdent(meta)} CASCADE`,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});
