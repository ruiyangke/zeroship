//! The vendor attributes SQLite declares.
//!
//! SQLite's `CREATE TABLE` grammar has exactly two table options, and both are here. That
//! makes this the shortest vocabulary in the workspace and a useful shape check on the
//! mechanism: a backend with two knobs writes two entries, not a trait impl.
//!
//! # `sqlite.strict` is NOT `TableStrictness`
//!
//! Read this before touching either. The neutral IR already has a field called
//! `strictness` — [`TableStrictness`](zero_migrate_ir::ir::TableStrictness) on
//! [`TableRuntimeOptions`](zero_migrate_ir::ir::TableRuntimeOptions) — and it is a
//! DIFFERENT THING that happens to share a word:
//!
//! * `TableStrictness` is `Strict | Lenient | Off`: zero-migrate's own DEPLOY-TIME data
//!   validation posture, neutral, and applies on every backend.
//! * `sqlite.strict` is SQLite's `STRICT` keyword: a per-table clause making the engine
//!   enforce declared column types at write time instead of applying type affinity.
//!
//! The key keeps SQLite's own spelling because a vendor attribute should read like the
//! vendor's documentation. The collision is in English only — one is a `TableStrictness`
//! enum in the neutral IR, the other an `AttrKey` owned by this crate — but a reader
//! meeting them a week apart will conflate them, so it is named here rather than left to
//! be rediscovered.

use zero_migrate_backend::attribute::{AttrDef, AttrShape, AttributeVocabulary};
use zero_migrate_ir::attribute::{AttrKey, AttrScope};

/// SQLite's declared attributes.
pub static VOCABULARY: AttributeVocabulary = AttributeVocabulary::new(DEFS);

static DEFS: &[AttrDef] = &[
    AttrDef {
        key: AttrKey::from_static("sqlite.strict"),
        scope: AttrScope::Table,
        shape: AttrShape::Bool,
        docs: "SQLite's STRICT table clause: enforce each column's declared type on write \
               rather than applying type affinity. Unrelated to zero-migrate's own \
               deploy-time `strictness` option.",
    },
    AttrDef {
        key: AttrKey::from_static("sqlite.without_rowid"),
        scope: AttrScope::Table,
        shape: AttrShape::Bool,
        docs: "Store the table as an index over its PRIMARY KEY with no separate rowid. \
               Requires a PRIMARY KEY, and changes what a rowid-dependent query sees.",
    },
];
