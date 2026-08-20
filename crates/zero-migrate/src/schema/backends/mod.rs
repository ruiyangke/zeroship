//! The per-backend schema/DDL **spelling** modules — one module per shipping
//! dialect.
//!
//! The sibling of [`crate::render::backends`], for the OTHER renderer registry.
//! Read that module's header first: it states the backend-boundary rule, the
//! one-dialect-literal rule, and the measurements behind both, and every word of
//! it applies here unchanged. This file records only what is different.
//!
//! | here                          | there                                 |
//! |-------------------------------|---------------------------------------|
//! | `schema::query` (the trait)   | `zero-migrate-backend` (the contract) |
//! | `schema::backends::postgres`  | `zero-migrate-postgres`               |
//! | `schema::backends::sqlite`    | `zero-migrate-sqlite`                 |
//! | `schema::backends::mysql`     | `zero-migrate-mysql`                  |
//! | `schema::backends` (this)     | the facade's `register(..)` table      |
//!
//! # Why this module did not exist until now
//!
//! `docs/proposals/pluggable-backends.md` step 4 wants each vendor in its own
//! crate, and the blocker is that core resolves a vendor BY DIALECT from inside
//! core. A spike measured that as TWO registries in incomparable states:
//! `render::backends::renderer -> &dyn DmlRenderer` had its vendors already
//! extracted and 33 lookups left to invert, while
//! `schema::query::renderer -> &dyn SchemaRenderer` had 9 lookups and vendors
//! NOT EXTRACTED AT ALL — three bare structs and three `impl` blocks in the
//! middle of a 6194-line file. There was nothing to invert because there was
//! nowhere for the vendors to be. This module is that somewhere; the inversion is
//! a separate, later edit.
//!
//! Nine lookups became eight before the move, and that is worth reading as part
//! of it: `SchemaRenderer::canonical_type` was not a spelling at all. Its
//! PostgreSQL arm was the identity, its other two arms delegated to core folds,
//! and what it answered was "do these two type spellings MEAN the same" — a drift
//! comparison, which the boundary rule keeps in core, dialect-parameterized. It is
//! now [`crate::schema::query::canonical_type_for_dialect`], which took the trait
//! from 8 methods to 7 and cleared `render::existence_probe` — the registry's only
//! caller outside `schema::query` — of renderer lookups entirely.
//!
//! # The cross-stack edge, which the `renderer(` grep cannot see
//!
//! These vendors spell identifiers through
//! [`crate::schema::query::quote_ident_for_dialect`], which forwards to
//! [`crate::render::dml::escape_quote_ident_for_dialect`], which resolves through
//! `render::backends`. So a SchemaRenderer vendor reaches its DmlRenderer sibling
//! through a core forwarder, one layer away from anything a grep of this directory
//! can show.
//!
//! That is deliberate and it must STAY one forwarder. The alternative — each
//! vendor here spelling its own identifiers — would put a second physical home of
//! the quoting bytes back in the tree, which is exactly the defect
//! `render::backends`'s header measured and removed (125 and 39 disjoint red
//! tests, two sets that could not see each other). When these become crates, the
//! forwarder is the seam that moves; do not fix it by copying the escape.
//!
//! # A near-collision that must not be tidied
//!
//! `SchemaRenderer::current_timestamp_expr` and `DmlRenderer::synth_now` answer
//! the same-sounding question and are NOT the same function. SQLite spells both
//! `CURRENT_TIMESTAMP` and MySQL spells both `CURRENT_TIMESTAMP(6)`, so two of the
//! three vendors agree — and PostgreSQL does not: `NOW()` here, `now()` there.
//! Agreeing on two of three is precisely the shape that makes a fold look safe and
//! makes the divergence invisible to a reader skimming for duplication. Emitted
//! bytes are the contract; leave both.

mod mysql;
mod postgres;
mod sqlite;

use crate::schema::query::{SchemaRenderer, SqlDialect};

/// The ONE exhaustive dispatch match over the shipping schema backends.
///
/// This is the facade's registry: the only place in the schema tree that knows
/// WHICH backends exist. A fourth [`SqlDialect`] variant breaks the match here at
/// compile time until its module is written and wired, which is the property worth
/// keeping — the alternative is a default arm that silently spells the new vendor
/// like PostgreSQL.
///
/// Re-exported as `crate::schema::query::renderer`, the path every caller uses.
///
/// # Who is allowed to call this
///
/// It had eight callers and every one of them was a POINT-OF-USE lookup: a core
/// emitter deep in a call chain, holding a `dialect: SqlDialect` parameter, asking
/// the registry for a vendor at the moment it needed one spelling. One
/// `CREATE TABLE` emit went through the registry five separate times for the same
/// dialect.
///
/// That is now zero. `schema::query`'s private emitters take
/// `backend: &'static dyn SchemaRenderer` instead of `dialect: SqlDialect`, and the
/// resolution happens ONCE per entry point. What remains here is only:
///
/// * **Boundaries** — `build_create_table_with_fks_for_dialect_scoped_statements`
///   and `def_to_column_type_for_dialect`, the `pub` surfaces whose callers hand in
///   a dialect. The dialect becomes a backend exactly where it crosses in.
/// * **Caller-fixed targets** — `build_add_foreign_key` and `def_to_constraints`
///   name PostgreSQL because they ARE PostgreSQL (`pg_quote_ident` throughout), and
///   the tests that name the vendor under test. Naming a fixed target is not a
///   decision, so it is not a lookup.
///
/// The distinction is what step 4 turns on: a boundary resolution survives the move
/// to per-vendor crates by becoming the facade's `register(..)` table, while a
/// point-of-use lookup cannot — it is core reaching for a vendor list core will no
/// longer have. Adding one back inside an emitter re-creates the blocker.
///
/// Note that the emitters still hold a `SqlDialect`; they derive it with
/// [`SchemaRenderer::dialect`] rather than receive it. That is deliberate and it is
/// NOT a lookup in disguise: identifier quoting, FK-action folding and the SQLite
/// scope test are core NORMALIZATION keyed by dialect, which the boundary rule keeps
/// in core, parameterized. Five such derivations exist; each one marks a place where
/// core needs to know which vendor it is talking to for a reason that is not
/// spelling, and shrinking that number is a different piece of work from this one.
pub fn renderer(dialect: SqlDialect) -> &'static dyn SchemaRenderer {
    match dialect {
        SqlDialect::Postgres => &postgres::RENDERER,
        SqlDialect::Sqlite => &sqlite::RENDERER,
        SqlDialect::Mysql => &mysql::RENDERER,
    }
}
