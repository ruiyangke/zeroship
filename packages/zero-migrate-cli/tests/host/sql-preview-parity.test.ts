// Join the TypeScript authoring surface to the hand-written SQL expectations in
// `crates/zero-migrate/tests/sql_preview.rs`.
//
// WHY THIS EXISTS
// `buildEnvelope` (packages/zero-migrate/src/internal/recorder.ts) is the only
// implementation that turns an authored TypeScript migration into the IR envelope
// the Rust engine consumes, and it sits on the production path. Every other
// authoring test asserts what that same function emitted, so a self-consistent
// recorder bug is invisible to them. `sql_preview.rs` pairs INDEPENDENTLY WRITTEN
// Rust IR literals with EXACT expected SQL strings that were typed by hand against
// the SQL, never generated from the recorder. This test re-authors those same
// migrations through the PUBLIC DSL (`table()`, `view()`, `t.*`), runs them through
// `buildEnvelope`, renders the envelope with the addon's `previewSql` verb, and
// asserts the SAME strings. A recorder that emits a subtly wrong column type,
// identity, generated expression, constraint, index, or view flag therefore fails
// against an expectation nobody derived from the recorder.
//
// WHAT IT PROVES
// For the three ported cases only: the DSL + recorder produce an IR envelope whose
// rendered SQL carries the exact type spellings, identity, generated expression,
// foreign key with its referential action, named index, view replaceability and
// projection, bind placeholders, and existence-guard labelling that the Rust test
// already pins by hand.
//
// WHAT IT DOES NOT PROVE
//   - Nothing about the ops it does not port. This is three cases out of the
//     renderable op surface, not a corpus.
//   - Nothing about nullability or column defaults. `sql_preview.rs` pins no
//     hand-written SQL for either, so there is nothing to borrow; asserting them
//     here would mean inventing an expectation from recorder output, which is the
//     mirror this test refuses to be. Covering them needs a hand-written SQL
//     expectation added to `sql_preview.rs` first.
//   - Nothing about apply-time behaviour. `previewSql` is the DB-free render path;
//     it opens no connection and executes nothing.
//   - Little about the policy table-shape resolve. `previewSql` DOES run
//     `resolve_create_table_policy` (the engine's preview entry resolves before it
//     validates and lowers, matching apply), so the rendered CREATE TABLE below
//     carries the CONFINED_CHARTER-injected columns, pinned primary key, and
//     injected indexes alongside the author-declared shape. The expectations here
//     are `includes()` checks on the author-declared facets only, because those are
//     what `sql_preview.rs` pins by hand -- the injected shape is asserted on the
//     Rust side, not here. Whole-file comparison against the goldens under
//     `crates/zero-migrate/tests/golden/` is still out of scope: those goldens cover
//     a different migration set, not these three.
//   - It is not a second renderer. The SQL text is produced by the one engine
//     renderer; only the IR reaching it comes from the TypeScript side.
//
// The expectations are transcribed from `sql_preview.rs` deliberately and there is
// no affordance to regenerate them. Re-blessing them from this side would turn an
// independent oracle into a mirror of the recorder, which is the exact failure this
// test exists to prevent. When one of these assertions fails, decide whether the
// recorder or the expectation is wrong, then edit `sql_preview.rs` and this file
// together by hand.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { table, t, view } from "zero-migrate";
import { buildEnvelope } from "zero-migrate/internal/recorder";
import type { MigrationModule } from "zero-migrate/internal/recorder";
import { currentIrVersion, previewSql } from "zero-migrate-cli";

const HERE = dirname(fileURLToPath(import.meta.url));
const CRATE = resolve(HERE, "../../../../crates/zero-migrate");

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

// The label `render_ir_envelope_sql` prints for an op it refuses to render offline
// (`zero_migrate::render::sql_preview::RUNTIME_RESOLVED`).
const RUNTIME_RESOLVED = "-- [runtime-resolved]";

/**
 * Read the charter `sql_preview.rs` renders under, from the Rust test-support
 * module that defines it. Sharing the literal rather than restating it keeps this
 * test on the SAME policy input as the expectations it borrows, so a charter change
 * can never silently make the two sides render different things.
 */
function confinedCharter(): string {
  const source = readFileSync(resolve(CRATE, "tests/support/mod.rs"), "utf8");
  const open = 'pub const CONFINED_CHARTER_TOML: &str = r#"';
  const start = source.indexOf(open);
  assert.notEqual(
    start,
    -1,
    "crates/zero-migrate/tests/support/mod.rs no longer declares CONFINED_CHARTER_TOML " +
      "as a raw string literal; update this reader so the two sides keep sharing one charter",
  );
  const end = source.indexOf('"#;', start + open.length);
  assert.notEqual(end, -1, "CONFINED_CHARTER_TOML literal is unterminated");
  const charter = source.slice(start + open.length, end);
  assert.match(charter, /^policy_version = 1$/m, "extracted charter looks like the TOML document");
  return charter;
}

const CHARTER = confinedCharter();

/**
 * Author -> `buildEnvelope` -> `previewSql`. The same two calls `zero-migrate-cli`
 * makes for `plan --sql` (`packages/zero-migrate-cli/src/cli.ts`), with the render
 * context `sql_preview.rs::opts()` uses: schema `public`, owner `app_preview`.
 */
function renderAuthored(mod: MigrationModule | readonly MigrationModule[], dialect: string): string {
  const modules: readonly MigrationModule[] = Array.isArray(mod) ? mod : [mod];
  const envelopes = modules.map((migration) =>
    JSON.stringify(buildEnvelope(migration, { irVersion: currentIrVersion() })),
  );
  return previewSql({
    envelopes,
    dialect,
    defaultSchema: "public",
    ownerApp: "app_preview",
    charterLayers: [CHARTER],
  }).join("\n");
}

// PORT of `sql_preview.rs::mysql_feature_preview_renders_mysql8_sql` (its
// `MYSQL_FEATURE_IR` literal, re-authored through the DSL). The richest structure
// in that file: identity, a stored generated column over a function call, an
// explicit primary key, a named foreign key with a referential action, a named
// supporting index, a replaceable view with a projection and a predicate, and a
// bound insert. Every expected string below is copied from that Rust test.
test("authored MySQL feature migration renders the SQL sql_preview.rs pins", () => {
  const schemaMigration: MigrationModule = {
    name: "mysql_feature",
    schema() {
      table("teams").create({
        columns: {
          id: t.int().notNull().identity({ always: false }),
          name: t.text().notNull(),
          // Omitted options are a STORED generated column, which is what the Rust
          // fixture's `"generated": { ..., "stored": true }` declares.
          name_lc: t.text().generated((col) => col("name").lower()),
        },
        primaryKey: ["id"],
      });

      table("members").create({
        columns: {
          id: t.int().notNull().identity({ always: false }),
          team_id: t.int().notNull(),
          email: t.text().notNull(),
        },
        primaryKey: ["id"],
        foreignKeys: [
          {
            name: "members_team_fk",
            columns: ["team_id"],
            references: { table: "teams", columns: ["id"] },
            onDelete: "cascade",
          },
        ],
        indexes: [{ name: "members_team_id_idx", on: ["team_id"] }],
      });

      view("active_teams").create({
        replace: true,
        as: (q) =>
          q
            .from("teams")
            .select(["id", "name"])
            .where((col) => col("name").isNotNull()),
      });
    },
  };

  const dataMigration: MigrationModule = {
    name: "mysql_feature_seed",
    data() {
      table("teams").insert({ rows: [{ id: 1, name: "Core" }] });
    },
    inverse() {
      table("teams").delete({ where: (col) => col("id").eq(1) });
    },
  };

  const out = renderAuthored([schemaMigration, dataMigration], "mysql");

  assert.ok(
    !out.includes(RUNTIME_RESOLVED),
    `an FK to an earlier table in the same envelope is fully previewable: ${out}`,
  );
  assert.ok(out.includes("CREATE TABLE `public`.`teams`"), out);
  assert.ok(out.includes("`id` INT AUTO_INCREMENT PRIMARY KEY"), out);
  assert.ok(out.includes("GENERATED ALWAYS AS (lower(`name`)) STORED"), out);
  assert.ok(
    out.includes("KEY `members_team_id_idx` (`team_id`)"),
    `the MySQL supporting index must be visible inline in the CREATE preview: ${out}`,
  );
  assert.ok(
    out.includes(
      "CONSTRAINT `members_team_fk` FOREIGN KEY (`team_id`) " +
        "REFERENCES `public`.`teams` (`id`) ON DELETE CASCADE",
    ),
    out,
  );
  assert.ok(
    out.includes(
      "CREATE OR REPLACE VIEW `public`.`active_teams` AS SELECT `id`, `name` " +
        "FROM `public`.`teams` WHERE (`name` IS NOT NULL)",
    ),
    out,
  );
  assert.ok(out.includes("VALUES (?, ?)"), out);
});

// PORT of `sql_preview.rs::string_length_renders_bounded_varchar_across_dialects`.
// `t.string({ length })` must reach the engine as the bounded `String { length }`
// col type, not as unbounded text: the three dialects spell that differently, so a
// recorder that dropped the bound would miss all three at once.
test("authored bounded string renders the per-dialect spellings sql_preview.rs pins", () => {
  const migration: MigrationModule = {
    name: "bounded",
    schema() {
      table("widgets").create({
        columns: {
          id: t.int().notNull().identity({ always: false }),
          code: t.string({ length: 200 }).notNull(),
        },
        primaryKey: ["id"],
      });
    },
  };

  const pg = renderAuthored(migration, "postgres");
  assert.ok(
    pg.includes("character varying(200)") || pg.includes("varchar(200)"),
    `PG must render a bounded varchar(200):\n${pg}`,
  );

  const mysql = renderAuthored(migration, "mysql");
  assert.ok(
    mysql.includes("VARCHAR(200)"),
    `MySQL must render VARCHAR(200), not the legacy VARCHAR(191) cap:\n${mysql}`,
  );

  const sqlite = renderAuthored(migration, "sqlite");
  assert.ok(
    sqlite.toUpperCase().includes('"CODE" TEXT'),
    `SQLite stores the bounded string as TEXT:\n${sqlite}`,
  );
});

// PORT of `sql_preview.rs::guarded_op_labeled_and_bare_ddl_has_no_fabricated_clause`.
// `{ ifNotExists: true }` must reach the engine as an `existenceGuard`
// -- a runtime catalog probe -- and must NOT become a native `IF NOT EXISTS` clause
// on the statement.
test("authored guarded addColumn renders the bare DDL sql_preview.rs pins", () => {
  const migration: MigrationModule = {
    name: "g",
    schema() {
      table("codes").column("flag").add({ type: t.boolean(), ifNotExists: true });
    },
  };

  const out = renderAuthored(migration, "postgres");

  assert.ok(
    out.includes(RUNTIME_RESOLVED) && out.includes("catalog-probed"),
    `guarded addColumn must carry the catalog-probe label:\n${out}`,
  );
  const alterLine = out.split("\n").find((line) => line.trimStart().startsWith("ALTER TABLE"));
  assert.ok(alterLine !== undefined, `a bare ALTER TABLE ADD COLUMN statement is rendered:\n${out}`);
  assert.ok(
    !alterLine.toUpperCase().includes("IF NOT EXISTS"),
    `guarded addColumn must render BARE DDL (no fabricated IF NOT EXISTS): ${JSON.stringify(alterLine)}`,
  );
  assert.ok(
    out.includes('ALTER TABLE "public"."codes" ADD COLUMN "flag" boolean'),
    `the guarded column keeps its authored name and type:\n${out}`,
  );
});
