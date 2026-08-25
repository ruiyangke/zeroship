//! The vendor attributes MySQL declares.
//!
//! Each entry is one option from MySQL's own `CREATE TABLE` / `ALTER TABLE` table-options
//! grammar that the neutral [`Op`](zero_migrate_ir::ir::Op) set does not model.
//!
//! This is a STARTER SET at [`AttrScope::Table`], not the full grammar. Widening it is a
//! change to this file alone.
//!
//! # What is deliberately left out, and why
//!
//! `KEY_BLOCK_SIZE` accepts 0, 1, 2, 4, 8 or 16 — a SET of integers, not a range, and
//! [`AttrShape::Int`] can only express a range. Declaring it as `0..=16` would admit 3
//! and 5, which the server then rejects at apply time: exactly the plan-time-versus-
//! apply-time failure the range exists to prevent. It stays out until the shape can say
//! what it means. An over-permissive declaration is worse than an absent one, because an
//! absent key is refused loudly and a wrong range passes quietly.

use zero_migrate_backend::attribute::{AttrDef, AttrShape, AttributeVocabulary};
use zero_migrate_ir::attribute::{AttrKey, AttrScope};

/// MySQL's declared attributes.
pub static VOCABULARY: AttributeVocabulary = AttributeVocabulary::new(DEFS);

static DEFS: &[AttrDef] = &[
    AttrDef {
        key: AttrKey::from_static("mysql.engine"),
        scope: AttrScope::Table,
        shape: AttrShape::Enum {
            variants: &["InnoDB", "MyISAM", "MEMORY", "CSV", "ARCHIVE"],
        },
        docs: "The storage engine. Anything other than InnoDB gives up transactional \
               DDL-adjacent guarantees this tool otherwise relies on.",
    },
    AttrDef {
        key: AttrKey::from_static("mysql.row_format"),
        scope: AttrScope::Table,
        shape: AttrShape::Enum {
            variants: &[
                "DEFAULT",
                "DYNAMIC",
                "FIXED",
                "COMPRESSED",
                "REDUNDANT",
                "COMPACT",
            ],
        },
        docs: "How rows are physically stored. DYNAMIC and COMPRESSED allow longer index \
               keys over variable-length columns than REDUNDANT or COMPACT.",
    },
    AttrDef {
        key: AttrKey::from_static("mysql.auto_increment"),
        scope: AttrScope::Table,
        shape: AttrShape::Int {
            min: 0,
            max: i64::MAX,
        },
        docs: "The next value the table's AUTO_INCREMENT column will hand out.",
    },
    AttrDef {
        key: AttrKey::from_static("mysql.charset"),
        scope: AttrScope::Table,
        shape: AttrShape::Text,
        docs: "The table's DEFAULT CHARACTER SET, inherited by character columns that do \
               not name their own.",
    },
    AttrDef {
        key: AttrKey::from_static("mysql.collate"),
        scope: AttrScope::Table,
        shape: AttrShape::Text,
        docs: "The table's default collation. Decides comparison and sort order, and \
               therefore whether a unique key treats two spellings as one value.",
    },
];
