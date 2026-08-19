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
//! rule currently holds by inspection (1 literal per module, its own); no test
//! enforces it yet, because adding one moves a pinned suite count.
//!
//! # The rule does NOT catch implicit coupling, and there is some
//!
//! A backend can still reach another vendor's spelling THROUGH a core helper
//! that hard-codes a dialect, and the grep above cannot see it because the
//! literal lives in core. That is not hypothetical here: `dml::quote_ident` and
//! `dml::quote_ident_checked` both pin `SqlDialect::Postgres`, so all four
//! identifier emissions in `backends/sqlite.rs` are quoted by the POSTGRESQL
//! renderer. It is correct today only because both vendors spell an identifier
//! `"x"`.
//!
//! MEASURED, not reasoned: corrupting `PostgresDmlRenderer::quote_ident` alone
//! fails 397 tests, 38 of which are SQLite-only — including
//! `fold_roundtrip_sqlite`, `ir_apply_sqlite` and
//! `dialect_conformance_live::every_sqlite_row_of_the_dialect_table_answers_to_a_live_database`.
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
