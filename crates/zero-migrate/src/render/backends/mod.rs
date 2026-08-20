//! The per-backend SQL **spelling** modules — one module per shipping dialect.
//!
//! This is the in-crate stand-in for the backend crates of
//! `docs/proposals/pluggable-backends.md` step 4. The mapping is deliberate and
//! one-to-one, so the crate extraction is a `git mv` plus a `Cargo.toml`:
//!
//! | here                        | there                          |
//! |-----------------------------|--------------------------------|
//! | `render::renderer`          | `zero-migrate-backend` (the contract) |
//! | `render::backends::postgres`| `zero-migrate-postgres`        |
//! | `render::backends::sqlite`  | `zero-migrate-sqlite`          |
//! | `render::backends::mysql`   | `zero-migrate-mysql`           |
//! | `render::backends` (this)   | the facade's `register(..)` table |
//!
//! # What belongs in a backend module
//!
//! **SPELLING only** — the bytes this vendor wants to read. `"now()"` vs
//! `"CURRENT_TIMESTAMP"`, `"bytea"` vs `"blob"`, `` `x` `` vs `"x"`,
//! `OR REPLACE VIEW` vs a `DROP VIEW IF EXISTS` prelude.
//!
//! **SEMANTICS stays in core, dialect-PARAMETERIZED.** Comparison and
//! normalization ("does this catalog default MEAN the same as that one",
//! "is this authored width equivalent to that reported width") are core's job
//! even when the answer depends on the dialect. `render::value_format` is the
//! worked example: its dialect branches are drift-comparison rules, and moving
//! them here would scatter one comparison across three vendors.
//!
//! The test is the DIRECTION of the arrow. Spelling is core ASKING a vendor how
//! to write something. Semantics is core DECIDING something about a vendor.
//!
//! # The one-dialect-literal rule
//!
//! A backend module names its own dialect exactly ONCE, as its `DIALECT` const,
//! and names no other dialect at all. Everything else reads `DIALECT`. That is
//! what makes step 4 mechanical: deleting the const is the only edit the module
//! needs, because nothing else in it can observe which vendor it is.
//!
//! It is greppable on purpose — `SqlDialect::Sqlite` appearing inside
//! `backends/mysql.rs` is a defect by construction, not a judgement call. The
//! rule is ENFORCED, not merely inspected:
//! `tests/dialect_matrix/backend_modules_name_one_dialect.rs` reads all three
//! modules with `include_str!` and asserts both halves — own dialect exactly
//! once, no other dialect at all, and that the single occurrence IS the
//! `const DIALECT` line rather than just some single line.
//!
//! # The rule does NOT catch implicit coupling, and there was some
//!
//! A backend can still reach another vendor's spelling THROUGH a core helper
//! that hard-codes a dialect, and the grep above cannot see it because the
//! literal lives in core. That was not hypothetical here: `dml::quote_ident`,
//! `dml::quote_bare_ident` and `dml::quote_ident_checked` all pinned
//! `SqlDialect::Postgres`, so all four identifier emissions in
//! `backends/sqlite.rs` used to be quoted by the POSTGRESQL renderer. It was
//! correct only because both vendors spell an identifier `"x"`.
//!
//! Of those three, only `quote_ident_checked` still EXISTS. The other two were
//! deleted rather than left unused once their last callers named a dialect — see
//! `dml::quote_ident_for_dialect`'s header for why an un-named spelling that exists
//! is a trap regardless of whether anything currently falls into it.
//!
//! MEASURED, not reasoned, BEFORE the fix: corrupting
//! `PostgresDmlRenderer::quote_ident` alone failed 7 of the 155 tests in the
//! SQLite-ONLY `sqlite_engine` binary (`ir_apply_sqlite::*`, `hr_sqlite`,
//! `existence_guard_sqlite`), all against a real SQLite database.
//!
//! RESOLVED: every identifier emission in `backends/sqlite.rs` and
//! `backends/postgres.rs` now goes through the `*_for_dialect(.., DIALECT)`
//! seam, the same one `backends/mysql.rs` already used. Re-running the identical
//! neuter afterwards leaves `sqlite_engine` at 155 passed / 0 failed — the same
//! 155 tests, so the SQLite backend stopped reading the PostgreSQL renderer
//! without any emitted byte changing.
//!
//! # The OTHER half of the class: emission that reaches no renderer at all
//!
//! The paragraph above is about a backend reaching ANOTHER vendor's renderer. The
//! larger instance was core reaching NO renderer: `dml::escape_quote_ident` was a
//! `pub(crate)` raw `format!` that any module could call to spell `"x"` without
//! naming a dialect. Correct bytes, absent routing, and invisible to every
//! behaviour test for the same reason as above — two vendors agree on the spelling.
//!
//! RESOLVED, and the fix is the visibility of [`ansi_double_quote_ident`] below.
//! Core cannot name it, so every former caller had to choose a door in
//! `render::dml` and record its vendor. MEASURED at `8710fe39`, on the SQLite-only
//! `sqlite_engine` binary (156 tests), by neutering `SqliteDmlRenderer::quote_ident`
//! and counting what notices:
//!
//! | tree | red | note |
//! |------|-----|------|
//! | before the seam | 51 | the SQLite backend's actual reach |
//! | after, `declarative` routed | 90 | +39, the whole previously-blind set |
//! | after, `apply::backend::sqlite` routed too | 118 | +28 more |
//!
//! ZERO tests were lost at any step, and all 1960 tests across the nine
//! `zero-migrate` binaries pass with byte-identical counts before and after — the
//! seam changed no emitted byte, only who decided it.
//!
//! AND THE CONTROL, WITHOUT WHICH THAT GREEN IS WORTHLESS. Identical pass counts
//! are evidence only if the suites can SEE these bytes at all; a suite blind to the
//! changed path reports the same green either way. So the same nine binaries were
//! re-run with [`ansi_double_quote_ident`] itself corrupted by one token:
//!
//! | binary | red |  | binary | red |
//! |--------|-----|--|--------|-----|
//! | `--lib` | 123 | | `column_shapes` | 74 |
//! | `sqlite_engine` | 118 | | `rename` | 26 |
//! | `pg_engine` | 78 | | `ir_contract` | 18 |
//! | `pg_drift` | 19 | | `dialect_matrix` | 7 |
//! | `mysql_engine` | 1 | | **total** | **464** |
//!
//! Every binary reddens, so every binary's green is load-bearing. `mysql_engine`
//! reddening only ONCE is not a blind spot but a confirmation: MySQL spells
//! identifiers with backticks and never calls this function, and its single red is
//! the PG-shaped constraintdef normal form described below, arriving exactly where
//! that section says it does.
//!
//! # What is still coupled, and where: NOTHING, and that is now a pinned number
//!
//! This section used to name a live instance — `render::lower::render_sqlite_trigger_op`
//! and its helpers calling the bare `dml::quote_bare_ident` at six sites, with
//! `backends/sqlite.rs` delegating its trigger rendering there so the coupling
//! survived one hop away. THAT IS PAST TENSE. Those three functions route every
//! identifier through `quote_bare_ident_for_dialect(.., SQLITE_TRIGGER_DIALECT)`, the
//! wrapper they used no longer exists, and the count is a RATCHET AT ITS STOP rather
//! than a claim in prose: `tests/dialect_matrix/sqlite_trigger_quoting_reaches_postgres.rs`
//! asserts zero pinned calls in `lower.rs` AND zero anywhere else under `src/`.
//!
//! Prefer that test to this paragraph. The reason this section carried a false
//! present-tense claim for as long as it did is that prose cannot go red, and the
//! stale-doc note it used to end with was itself stale by the time anyone read it.
//!
//! And a DELIBERATE non-defect that looks identical to a neuter: the
//! `pg_get_constraintdef` normal form (`declarative::quote_ident_if_needed` /
//! `constraintdef_cols`) is PostgreSQL-spelled ON PURPOSE and is read by the SQLite
//! and MySQL drift comparators. It has its own door,
//! `dml::pg_canonical_ident`, precisely because a red count cannot tell it apart
//! from an unrouted emission. Re-dialecting it would be a regression.
//!
//! The same distinction, re-measured one layer up at `7ca23cdc`, because
//! `render::declarative` is where an audit lands next and its own `quote_ident` still
//! reads as a PostgreSQL helper anything could call. It is not: `SqliteEmitter` spells
//! all fourteen of its identifiers with `sqlite_ident`, and `MysqlEmitter` spells its
//! seventeen with `mysql_quote_ident` and its own `qualified` — neither calls
//! `quote_ident` even once. All thirty-three remaining call sites are either on a
//! PostgreSQL-only path — a `SqlDialect::Postgres` arm, a `PgEmitter` method, a
//! `_pg`-named renderer — or are building/parsing the constraintdef normal form
//! above. Asserting the dialect in
//! `DeclarativeAuthor::qualified`, the choke point the un-arm'd sites all funnel
//! through, and running the 3372-test workspace suite: the `IS Postgres` probe passes
//! 3372 / 0, the inverted CONTROL fails 45, so the green means "no non-PostgreSQL
//! dialect gets here" rather than "nothing looked".
//!
//! So when moving a branch here, also grep the CORE helpers the moved code
//! calls for hard-coded dialects. A directory move creates the APPEARANCE of
//! vendor independence; only that second grep tests for the substance.

mod mysql;
mod postgres;
mod sqlite;

use crate::render::renderer::DmlRenderer;
use crate::schema::query::SqlDialect;

/// The ANSI double-quote identifier spelling: double every embedded `"`, wrap the
/// result in `"`. THE single physical home of that byte-logic, and PRIVATE TO THE
/// BACKENDS.
///
/// Two of the three shipping vendors happen to agree on this spelling, which is
/// exactly why core must not be able to reach it un-named. A core caller that
/// spells these bytes itself is spelling them FOR A VENDOR IT NEVER NAMED, and no
/// assertion about emitted SQL can see the mistake while the two vendors agree —
/// the bytes are right, the routing is absent. Visibility is the detector: core
/// cannot name this function, so it must pick a door in [`crate::render::dml`]
/// instead ([`crate::render::dml::escape_quote_ident_for_dialect`] to EMIT for a
/// named dialect, [`crate::render::dml::pg_canonical_ident`] for the PG-shaped
/// normal form). Both doors resolve back here through [`renderer`], so the bytes
/// are unchanged and the vendor is now on the record.
///
/// This is also why the two `quote_ident` impls below call it DIRECTLY rather than
/// through the `*_for_dialect` seam their sibling methods use: they ARE the
/// dialect's `quote_ident`, so routing through the dispatch would recurse.
pub(in crate::render::backends) fn ansi_double_quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// The ONE exhaustive dispatch match over the shipping backends.
///
/// This is the facade's registry: the only place in the crate that knows WHICH
/// backends exist. A fourth [`SqlDialect`] variant breaks the match here at
/// compile time until its module is written and wired, which is the property
/// worth keeping — the alternative is a default arm that silently spells the
/// new vendor like PostgreSQL.
pub(crate) fn renderer(dialect: SqlDialect) -> &'static dyn DmlRenderer {
    match dialect {
        SqlDialect::Postgres => &postgres::RENDERER,
        SqlDialect::Sqlite => &sqlite::RENDERER,
        SqlDialect::Mysql => &mysql::RENDERER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_returns_expected_dml_renderer() {
        assert_eq!(renderer(SqlDialect::Postgres).synth_now(), "now()");
        assert_eq!(
            renderer(SqlDialect::Sqlite).synth_now(),
            "CURRENT_TIMESTAMP"
        );
        assert_eq!(
            renderer(SqlDialect::Mysql).synth_now(),
            "CURRENT_TIMESTAMP(6)"
        );
    }
}
