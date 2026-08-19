//! The per-backend SQL **spelling** modules — one module per shipping dialect.
//!
//! This is the in-crate stand-in for the backend crates of
//! `docs/proposals/pluggable-backends.md` step 4. The mapping is deliberate and
//! one-to-one, so the crate extraction is a `git mv` plus a `Cargo.toml`:
//!
//! | here                        | there                          |
//! |-----------------------------|--------------------------------|
//! | `render::renderer`          | `zero-migrate-backend` (the contract) |
//! | `render::backends::postgres`| `zero-migrate-pg`              |
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
//! `dml::quote_bare_ident` and `dml::quote_ident_checked` all pin
//! `SqlDialect::Postgres`, so all four identifier emissions in
//! `backends/sqlite.rs` used to be quoted by the POSTGRESQL renderer. It was
//! correct only because both vendors spell an identifier `"x"`.
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
//! # What is still coupled, and where
//!
//! Two SQLite render paths still reach the PG-pinned core wrappers, both OUTSIDE
//! these modules and both belonging to `lower.rs`'s own step-3 pass:
//! `render::lower::render_sqlite_trigger_op` and its helpers call the bare
//! `dml::quote_bare_ident` at six sites. `backends/sqlite.rs` delegates its
//! trigger rendering there, so the coupling survives one hop away.
//!
//! STALE DOC, KNOWN, NOT MINE TO EDIT: the enforcing test's own module comment
//! (`backend_modules_name_one_dialect.rs`) still says the `quote_ident` coupling
//! is "TRUE OF THIS TREE while this test passes", and points here for the
//! measurement. That was true when it was written and is no longer. This module
//! is the authority on that question; the test's prose needs the same edit and
//! the change that resolved the coupling could not make it, because that file
//! belongs to a concurrently-edited directory.
//!
//! So when moving a branch here, also grep the CORE helpers the moved code
//! calls for hard-coded dialects. A directory move creates the APPEARANCE of
//! vendor independence; only that second grep tests for the substance.

mod mysql;
mod postgres;
mod sqlite;

use crate::render::renderer::DmlRenderer;
use crate::schema::query::SqlDialect;

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
