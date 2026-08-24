//! The VENDOR (`zero-migrate`) Postgres render seam.
//!
//! Renders the privileged vendor `Op` variants to **structured**
//! Postgres DDL: identifiers double-quoted via the crate's single quoting seam
//! (`quote_ident_checked`), policy/trigger predicates rendered from the CLOSED
//! [`Expr`](zero_migrate_ir::expr::Expr) AST via the existing inline renderer
//! (`render_predicate_pg`) — **never string concatenation**. The function `body`
//! and the `raw` escape are the two raw-string fields: they are embedded
//! VERBATIM and the WHOLE rendered statement is then `pg_query`-parsed by the
//! guard at the lower seam (so the body is scanned).
//!
//! Every vendor op is `dialect_scope = PgOnly`: this module only renders Postgres,
//! and the lower seam (`crate::render::lower`) hard-rejects a SQLite target before
//! reaching here. The render is pure (no DB, no live schema).
//!
//! # NOT in this module
//!
//! The capability GATE lives in `crate::model::validate`
//! (`VENDOR_OP_DENIED` at load) + the rendered-SQL deny-list (the guard at lower).
//! This module is render-only; it assumes the op already passed both gates.

use crate::dml::IdentQuoteError;
use zero_migrate_ir::dialect::DialectId;

/// A single rendered vendor statement: a name (for the journaled `Migration`), the
/// forward SQL (no trailing `;`), and the reverse SQL (or `None` for an
/// irreversible op). A vendor op renders to ONE OR MORE of these (e.g. a
/// `createRole` with `setSearchPath` renders a `CREATE ROLE` + a follow-on
/// `ALTER ROLE … SET search_path` statement).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorStatement {
    /// A stable, human-readable name for the journaled migration unit.
    pub name: String,
    /// The forward SQL (no trailing `;`).
    pub up: String,
    /// The reverse SQL, or `None` when there is no structural inverse.
    pub down: Option<String>,
}

/// A failure rendering a vendor op to Postgres DDL.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VendorError {
    /// An identifier could not be quoted (empty / NUL).
    #[error("vendor render: identifier not quotable: {0}")]
    Ident(#[from] IdentQuoteError),
    /// A policy `USING`/`WITH CHECK` or trigger `WHEN` predicate could not be
    /// rendered from its closed AST.
    #[error("vendor render: predicate not renderable: {0}")]
    Predicate(String),
    /// A required list was empty (no privileges, no roles, no events, …).
    #[error("vendor render: {what} must be non-empty")]
    EmptyList {
        /// What was empty.
        what: &'static str,
    },
    /// A trigger action is not renderable on Postgres.
    #[error("vendor render: trigger action {kind} is unsupported on Postgres")]
    UnsupportedTriggerAction {
        /// Stable unsupported-kind token.
        kind: &'static str,
    },
    /// `CREATE ROLE IF NOT EXISTS` is synthesized with an opaque PL/pgSQL DO
    /// wrapper; SUPERUSER must never be hidden inside that body.
    #[error("vendor render: createRole cannot combine superuser:true with ifNotExists:true")]
    SuperuserIfNotExistsUnsupported,
    /// A function signature type was not in the conservative type-reference
    /// grammar.
    #[error("vendor render: unsafe function type reference in {slot}: {value:?}")]
    InvalidTypeRef {
        /// The field being rendered.
        slot: &'static str,
        /// The rejected value.
        value: String,
    },
    /// A reserved pseudo-role (e.g. `PUBLIC`) was used where a concrete role is
    /// required. `DROP OWNED BY PUBLIC` errors at apply, so reject it at render.
    #[error("vendor render: {what} cannot target the reserved pseudo-role {role:?}")]
    ReservedRole {
        /// What was being rendered.
        what: &'static str,
        /// The rejected pseudo-role.
        role: String,
    },
    /// Two mutually-exclusive instructions were supplied in one op (e.g. an
    /// `alterRole` carrying both `setSearchPath` and `resetSearchPath:true`).
    #[error("vendor render: {what}")]
    ContradictoryArgs {
        /// A human description of the contradiction.
        what: &'static str,
    },
    /// This vendor renders NO vendor ops at all, and says so itself.
    ///
    /// The privileged op kinds — roles, grants, RLS, policies, functions,
    /// extensions, schemas and the raw escape — are every one of them
    /// `dialect_scope = PgOnly`. Two of the three shipping vendors have no
    /// counterpart to render and never had one, so this is the whole of their
    /// answer to [`crate::renderer::DmlRenderer::render_vendor_op`].
    ///
    /// It carries the refusing vendor's own dialect, read from that module's
    /// `DIALECT` const rather than written as a literal, so the one-dialect-literal
    /// rule is unaffected.
    ///
    /// Reaching this is defence in depth rather than the live refusal path: the
    /// engine's lower seam already refuses a target that lacks
    /// `Capability::PrivilegedCatalogObjects` BEFORE it asks a renderer, and it
    /// refuses with `IrLowerError::VendorUnsupported`, which names the op kind. This
    /// variant is what a vendor returns when something reaches it anyway.
    ///
    /// # Why this carries a `DialectId` and not the enum
    ///
    /// This is PROVENANCE — data recording WHICH backend refused — and it never
    /// dispatches on the value. Typing it as the former closed dialect enum meant a
    /// fourth backend could not state its own refusal at all: it has no variant
    /// to name itself with, so the required method it must implement had no
    /// value it could legally return. The stub in
    /// `tests/a_fourth_backend_names_itself.rs` failed to compile on exactly
    /// that, which is the cheapest possible demonstration that the type was
    /// wrong rather than the newcomer.
    #[error("vendor render: {0} renders no vendor ops (every vendor op is PgOnly)")]
    VendorOpsUnsupported(DialectId),
}
