//! The vendor attributes MySQL declares.
//!
//! Each entry is one option from MySQL's own `CREATE TABLE` / `ALTER TABLE` table-options
//! grammar that the neutral [`Op`](zero_migrate_ir::ir::Op) set does not model.
//!
//! This is a STARTER SET, not the full grammar. Widening it is a
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
use zero_migrate_backend::declare_attributes;
use zero_migrate_ir::attribute::{
    CreatePartitionAttributes, CreateTableAttributes, SetTableOptionsAttributes,
};

/// MySQL's declared attributes.
pub static VOCABULARY: AttributeVocabulary = AttributeVocabulary::new(DEFS);

static DEFS: &[AttrDef] = declare_attributes! {
    dialect: "mysql";

    /// The storage engine. Anything other than InnoDB gives up transactional
    /// DDL-adjacent guarantees this tool otherwise relies on. HEAP is a synonym
    /// for MEMORY, MRG_MyISAM for MERGE, and NDBCLUSTER for NDB. Spell it as the
    /// manual does: the comparison is exact, while the server itself accepts any
    /// casing.
    engine on [CreateTableAttributes, CreatePartitionAttributes, SetTableOptionsAttributes]
        = AttrShape::Enum {
            // The manual's own table of storage engines, in its spelling, INCLUDING the
            // documented aliases. An earlier version listed only the first five, which
            // REFUSED five engines MySQL accepts — an over-restrictive enum is not a
            // conservative choice here, it is a wrong answer that blocks a legal
            // migration at plan time.
            variants: &[
                "InnoDB",
                "MyISAM",
                "MEMORY",
                "CSV",
                "ARCHIVE",
                "EXAMPLE",
                "FEDERATED",
                "HEAP",
                "MERGE",
                "MRG_MyISAM",
                "NDB",
                "NDBCLUSTER",
            ],
        };

    /// How rows are physically stored. DYNAMIC and COMPRESSED allow longer index
    /// keys over variable-length columns than REDUNDANT or COMPACT.
    row_format on [CreateTableAttributes, CreatePartitionAttributes, SetTableOptionsAttributes]
        = AttrShape::Enum {
            variants: &[
                "DEFAULT",
                "DYNAMIC",
                "FIXED",
                "COMPRESSED",
                "REDUNDANT",
                "COMPACT",
            ],
        };

    /// The next value the table's AUTO_INCREMENT column will hand out. MySQL's own
    /// ceiling is an unsigned 64-bit integer, which is wider than this attribute
    /// can carry, so values above 2^63-1 are refused here despite being legal.
    auto_increment
        on [CreateTableAttributes, CreatePartitionAttributes, SetTableOptionsAttributes]
        = AttrShape::Int {
            // `min: 0` is not decoration: the manual warns that AUTO_INCREMENT "works
            // properly only if it contains only positive values" and that inserting a
            // negative "is regarded as inserting a very large positive number" — a
            // silent wrong answer, so it is refused here instead.
            min: 0,
            // A KNOWN under-statement. MySQL's ceiling is an UNSIGNED BIGINT
            // (18446744073709551615), which is larger than `i64::MAX` and therefore not
            // representable by `AttrShape::Int` at all. A value between the two is
            // refused here though the server would take it. Widening the shape to cover
            // unsigned 64-bit is a change to the shared contract, not to this file.
            max: i64::MAX,
        };

    /// The table's DEFAULT CHARACTER SET, inherited by character columns that do
    /// not name their own.
    charset on [CreateTableAttributes, CreatePartitionAttributes, SetTableOptionsAttributes]
        = AttrShape::Text;

    /// The table's default collation. Decides comparison and sort order, and
    /// therefore whether a unique key treats two spellings as one value.
    collate on [CreateTableAttributes, CreatePartitionAttributes, SetTableOptionsAttributes]
        = AttrShape::Text;
};
