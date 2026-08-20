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
/// Re-exported as `crate::schema::query::renderer`, the path all eight of its
/// callers already use, so this move changed no call site.
pub fn renderer(dialect: SqlDialect) -> &'static dyn SchemaRenderer {
    match dialect {
        SqlDialect::Postgres => &postgres::RENDERER,
        SqlDialect::Sqlite => &sqlite::RENDERER,
        SqlDialect::Mysql => &mysql::RENDERER,
    }
}
