//! The vendor attributes PostgreSQL declares.
//!
//! Each entry is one knob from PostgreSQL's own `CREATE TABLE` grammar - a storage
//! parameter or a table-level clause - that the neutral [`Op`](zeroship_migrate_ir::ir::Op)
//! set does not model and should not.
//!
//! This is a STARTER SET, not the full grammar. The point of the
//! mechanism is that widening it is a change to this file alone: adding a knob here adds
//! it to validation, to the refusal messages and to the generated TypeScript surface,
//! and touches no neutral crate and no `match` anywhere.
//!
//! # The two that used to live in the neutral IR
//!
//! `IndexStorageParams` held `fillfactor` and `pages_per_range` as named fields in
//! `zero-migrate-ir` and in the neutral contract crate's `IndexSnapshot` - PostgreSQL
//! storage parameters living in the crates whose purpose is to name no vendor. Core's
//! own drift pass then formatted them BY THOSE TWO SPELLINGS. They were the reason this
//! mechanism exists.
//!
//! That type is now deleted. The `createIndex` declarations below are its whole
//! replacement: they are what the renderer emits, what live introspection FILTERS
//! `reloptions` down to, and what drift compares. The filter used to be two hardcoded
//! field names and is now the vocabulary, so a third index storage parameter is a change
//! to this file and to nothing else.

use zeroship_migrate_backend::attribute::{AttrDef, AttrShape, AttributeVocabulary};
use zeroship_migrate_backend::declare_attributes;
use zeroship_migrate_ir::attribute::{
    CreateIndexAttributes, CreatePartitionAttributes, CreateTableAttributes,
    SetTableOptionsAttributes,
};

/// PostgreSQL's declared attributes.
pub static VOCABULARY: AttributeVocabulary = AttributeVocabulary::new(DEFS);

static DEFS: &[AttrDef] = declare_attributes! {
    dialect: "postgres";

    /// Percentage of each page left free for later updates, so a row can be
    /// updated in place. 100 packs pages fully and suits an insert-only table.
    fillfactor on [CreateTableAttributes, CreatePartitionAttributes, SetTableOptionsAttributes]
        = AttrShape::Int { min: 10, max: 100 };

    /// The tablespace the table is created in. Must already exist on the server.
    tablespace on [CreateTableAttributes, CreatePartitionAttributes, SetTableOptionsAttributes]
        = AttrShape::Text;

    /// Whether autovacuum runs on this table. Disabling it makes vacuuming the
    /// operator's problem and is rarely right.
    autovacuum_enabled
        on [CreateTableAttributes, CreatePartitionAttributes, SetTableOptionsAttributes]
        = AttrShape::Bool;

    /// Row length above which PostgreSQL tries to move columns out of line into
    /// TOAST storage. The upper bound is the server's block size minus its header
    /// (8160 on a default 8kB build); a larger block size accepts more than this
    /// declaration allows.
    toast_tuple_target
        on [CreateTableAttributes, CreatePartitionAttributes, SetTableOptionsAttributes]
        = AttrShape::Int {
            min: 128,
            // CONSERVATIVE, and knowingly so. The manual says "between 128 bytes and the
            // (block size - header), by default 8160 bytes" - the ceiling is derived from
            // the server's BLOCK SIZE, which is a compile-time choice. On a server built
            // with a 16kB or 32kB block this refuses values the server would accept.
            // A declaration cannot ask the server, and refusing a legal value is the
            // failure this errs toward deliberately: it is loud and correctable, whereas
            // admitting an illegal one fails mid-apply.
            max: 8160,
        };

    /// How many workers a parallel scan of this table should ask for. 0 disables
    /// parallel scans of it. The server clamps the effective count against
    /// max_parallel_workers, so a high value here is a request, not a guarantee.
    parallel_workers
        on [CreateTableAttributes, CreatePartitionAttributes, SetTableOptionsAttributes]
        = AttrShape::Int {
            min: 0,
            // The manual states NO upper bound for this storage parameter, so neither
            // does this declaration. An earlier version said 1024, which was invented
            // rather than read - it would have refused a legal value with a bound
            // PostgreSQL never imposed. `i32::MAX` is the storage parameter's own integer
            // ceiling; the server clamps the effective count against `max_parallel_workers`
            // at run time, which is not a plan-time fact.
            max: i32::MAX as i64,
        };

    // ---- `createIndex`: the two knobs `IndexStorageParams` holds today --------------
    //
    // `fillfactor` appears TWICE in this file - once for the table ops above and once
    // for `createIndex` here - and that is the point rather than an oversight. It is
    // legal on both and means the same thing in each. On the op axis this needs no
    // special machinery: a declaration lists the ops it is legal on, and two knobs that
    // share a key but not an op list are simply two rows.

    /// Percentage of each index page left free when the index is built, so a
    /// later insert can go on the right page instead of splitting it.
    // The manual gives the same 10..=100 percentage for an index as for a table. The
    // DEFAULT differs (90 for a B-tree, 100 for a table), but a default is the
    // server's business; only the accepted range is a declaration's.
    fillfactor on [CreateIndexAttributes] = AttrShape::Int { min: 10, max: 100 };

    /// BRIN only: how many table blocks each index entry summarises. A smaller
    /// range makes a larger but more selective index.
    // BRIN only. The manual states 1..=131072, so unlike `parallel_workers` - where
    // an earlier version of this file invented a ceiling - this bound is read, not
    // guessed.
    pages_per_range on [CreateIndexAttributes] = AttrShape::Int { min: 1, max: 131_072 };
};
