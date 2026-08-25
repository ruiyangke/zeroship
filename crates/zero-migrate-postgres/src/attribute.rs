//! The vendor attributes PostgreSQL declares.
//!
//! Each entry is one knob from PostgreSQL's own `CREATE TABLE` grammar — a storage
//! parameter or a table-level clause — that the neutral [`Op`](zero_migrate_ir::ir::Op)
//! set does not model and should not.
//!
//! This is a STARTER SET at [`AttrScope::Table`], not the full grammar. The point of the
//! mechanism is that widening it is a change to this file alone: adding a knob here adds
//! it to validation, to the refusal messages and to the generated TypeScript surface,
//! and touches no neutral crate and no `match` anywhere.
//!
//! # The one that is already in the neutral IR
//!
//! [`IndexStorageParams`](zero_migrate_ir::ir::IndexStorageParams) holds `fillfactor` and
//! `pages_per_range` as named fields in `zero-migrate-ir` — PostgreSQL storage parameters
//! living in the crate whose purpose is to name no vendor. They are the reason this
//! mechanism exists. Moving them here is an [`AttrScope::Index`] change with a wire
//! format to migrate, so it is deliberately NOT part of this first table-scoped slice;
//! the table-level `fillfactor` below is a separate parameter that was never modelled.

use zero_migrate_backend::attribute::{AttrDef, AttrShape, AttributeVocabulary};
use zero_migrate_ir::attribute::{AttrKey, AttrScope};

/// PostgreSQL's declared attributes.
pub static VOCABULARY: AttributeVocabulary = AttributeVocabulary::new(DEFS);

static DEFS: &[AttrDef] = &[
    AttrDef {
        key: AttrKey::from_static("postgres.fillfactor"),
        scope: AttrScope::Table,
        shape: AttrShape::Int { min: 10, max: 100 },
        docs: "Percentage of each page left free for later updates, so a row can be \
               updated in place. 100 packs pages fully and suits an insert-only table.",
    },
    AttrDef {
        key: AttrKey::from_static("postgres.tablespace"),
        scope: AttrScope::Table,
        shape: AttrShape::Text,
        docs: "The tablespace the table is created in. Must already exist on the server.",
    },
    AttrDef {
        key: AttrKey::from_static("postgres.autovacuum_enabled"),
        scope: AttrScope::Table,
        shape: AttrShape::Bool,
        docs: "Whether autovacuum runs on this table. Disabling it makes vacuuming the \
               operator's problem and is rarely right.",
    },
    AttrDef {
        key: AttrKey::from_static("postgres.toast_tuple_target"),
        scope: AttrScope::Table,
        shape: AttrShape::Int {
            min: 128,
            max: 8160,
        },
        docs: "Row length above which PostgreSQL tries to move columns out of line into \
               TOAST storage.",
    },
    AttrDef {
        key: AttrKey::from_static("postgres.parallel_workers"),
        scope: AttrScope::Table,
        shape: AttrShape::Int { min: 0, max: 1024 },
        docs: "How many workers a parallel scan of this table should ask for. 0 disables \
               parallel scans of it.",
    },
];
