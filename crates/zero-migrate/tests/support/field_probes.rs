//! **The FIELD PROBE inventory**: for every field of every snapshot type, a mutation
//! that changes ONLY that field.
//!
//! Two tests need the same list, and giving them one list is the point. `tests/support/
//! carriers.rs` proved the technique for the rename carrier inventory - exhaustive
//! destructuring with no `..`, every binding routed through a classifier that demands
//! something from the author - and this is that technique carried across to its sibling
//! problem rather than reinvented beside it.
//!
//! * `structural_equality_field_sensitivity.rs` asks **"does `==` notice this field?"**,
//!   which measures the FAILURE DIRECTION: a field nothing compares is a field a future
//!   change breaks silently.
//! * `support::model_equivalence` asks **"do the named comparator and the `eq` it
//!   replaced answer the same for this field?"**, which is what makes each comparator's
//!   individual TERMS falsifiable. Without per-field probes that check is weak, and the
//!   weakness is specific and worth recording: any two DISTINCT real columns differ in
//!   `name`, so a pairwise sweep over real objects never reaches the later terms of a
//!   comparator, and a comparator that DROPPED a term would still pass. Mutating one
//!   field of a real object is what closes that.
//!
//! Adding a field to any of these types is a compile error here until a mutation exists
//! for it, so neither consumer can silently stop covering it.

use zero_migrate::{
    ColumnCollationSnapshot, ColumnSnapshot, ConstraintSnapshot, GeneratedColumnSnapshot,
    GeneratedKindSnapshot, IdDefaultSnapshot, IdentityCol, IndexElementSnapshot, IndexSnapshot,
    IndexStorageParams, MysqlPhysicalType, MysqlTextStorageSnapshot, TableSnapshot, ValueFormat,
};

/// One field, and a mutation that changes ONLY that field.
pub struct Probe<T> {
    /// The field path, e.g. `ColumnSnapshot::collation`.
    pub field: &'static str,
    /// Applies the mutation in place.
    pub mutate: fn(&mut T),
}

/// The accumulator. Every binding of an exhaustive destructure has to reach
/// [`Self::probe`], which is what makes the list a checklist rather than a sample.
pub struct ProbeSet<T> {
    /// The declared probes, in declaration order.
    pub probes: Vec<Probe<T>>,
}

impl<T> ProbeSet<T> {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self { probes: Vec::new() }
    }

    /// Declare one field. `_binding` is the destructured field itself and is unused on
    /// purpose: passing it is the proof that the binding was routed, exactly as
    /// `support::carriers`' three classifiers demand.
    pub fn probe<B>(&mut self, field: &'static str, _binding: B, mutate: fn(&mut T)) {
        self.probes.push(Probe { field, mutate });
    }
}

/// A base index with one plain key column.
#[must_use]
pub fn base_index_snapshot() -> IndexSnapshot {
    IndexSnapshot::btree("t_a_idx", false, vec!["a".to_string()])
}

/// A base CHECK constraint.
#[must_use]
pub fn base_constraint_snapshot() -> ConstraintSnapshot {
    ConstraintSnapshot {
        name: "t_a_check".to_string(),
        kind: "CHECK".to_string(),
        definition: "CHECK ((a > 0))".to_string(),
        comment: None,
        cascade_columns: None,
    }
}

/// A base empty table.
#[must_use]
pub fn base_table_snapshot() -> TableSnapshot {
    TableSnapshot {
        columns: Vec::new(),
        indexes: Vec::new(),
        constraints: Vec::new(),
        runtime_options: zero_migrate::TableRuntimeOptions::default(),
        partition_by: None,
        comment: None,
        stored_create_sql: None,
    }
}

/// Every field of [`ColumnSnapshot`], with a mutation each.
#[must_use]
pub fn column_snapshot_probes() -> ProbeSet<ColumnSnapshot> {
    let mut set: ProbeSet<ColumnSnapshot> = ProbeSet::new();
    // EXHAUSTIVE, no `..`: a new `ColumnSnapshot` field breaks this line.
    let ColumnSnapshot {
        name,
        data_type,
        nullable,
        default,
        ddl_type_override,
        inline_checks,
        generated,
        generated_kind,
        identity,
        sqlite_rowid,
        value_format,
        catalog_uuid_format_check,
        id_default,
        mysql_default_generated,
        case_sensitive,
        collation,
        mysql_text_storage,
        mysql_physical_type,
        encryption_sentinel,
        comment_sentinel,
        comment,
    } = ColumnSnapshot::default();

    set.probe("ColumnSnapshot::name", name, |c| {
        c.name = "renamed".to_string();
    });
    set.probe("ColumnSnapshot::data_type", data_type, |c| {
        c.data_type = "bigint".to_string();
    });
    set.probe("ColumnSnapshot::nullable", nullable, |c| c.nullable = true);
    set.probe("ColumnSnapshot::default", default, |c| {
        c.default = Some("now()".to_string());
    });
    set.probe(
        "ColumnSnapshot::ddl_type_override",
        ddl_type_override,
        |c| c.ddl_type_override = Some("public.mood".to_string()),
    );
    set.probe("ColumnSnapshot::inline_checks", inline_checks, |c| {
        c.inline_checks = vec!["CHECK (x > 0)".to_string()];
    });
    set.probe("ColumnSnapshot::generated", generated, |c| {
        c.generated = Some(GeneratedColumnSnapshot {
            expr: "(a + 1)".to_string(),
            source: None,
            stored: true,
        });
    });
    set.probe("ColumnSnapshot::generated_kind", generated_kind, |c| {
        c.generated_kind = Some(GeneratedKindSnapshot::Stored);
    });
    set.probe("ColumnSnapshot::identity", identity, |c| {
        c.identity = Some(IdentityCol { always: true });
    });
    set.probe("ColumnSnapshot::sqlite_rowid", sqlite_rowid, |c| {
        c.sqlite_rowid = true;
    });
    set.probe("ColumnSnapshot::value_format", value_format, |c| {
        c.value_format = Some(ValueFormat::Ulid);
    });
    set.probe(
        "ColumnSnapshot::catalog_uuid_format_check",
        catalog_uuid_format_check,
        |c| c.catalog_uuid_format_check = true,
    );
    set.probe("ColumnSnapshot::id_default", id_default, |c| {
        c.id_default = Some(IdDefaultSnapshot::Absent);
    });
    set.probe(
        "ColumnSnapshot::mysql_default_generated",
        mysql_default_generated,
        |c| c.mysql_default_generated = Some(true),
    );
    set.probe("ColumnSnapshot::case_sensitive", case_sensitive, |c| {
        c.case_sensitive = Some(false);
    });
    set.probe("ColumnSnapshot::collation", collation, |c| {
        c.collation = Some(ColumnCollationSnapshot {
            schema: None,
            name: "C".to_string(),
        });
    });
    set.probe(
        "ColumnSnapshot::mysql_text_storage",
        mysql_text_storage,
        |c| {
            c.mysql_text_storage = Some(MysqlTextStorageSnapshot {
                character_set: "utf8mb4".to_string(),
                collation: "utf8mb4_bin".to_string(),
            });
        },
    );
    set.probe(
        "ColumnSnapshot::mysql_physical_type",
        mysql_physical_type,
        |c| {
            c.mysql_physical_type = Some(MysqlPhysicalType::Character {
                fixed: false,
                length: 40,
            });
        },
    );
    set.probe(
        "ColumnSnapshot::encryption_sentinel",
        encryption_sentinel,
        |c| c.encryption_sentinel = Some("/* zero-migrate:enc */".to_string()),
    );
    set.probe("ColumnSnapshot::comment_sentinel", comment_sentinel, |c| {
        c.comment_sentinel = Some("zero-migrate:mask:kind=email".to_string());
    });
    set.probe("ColumnSnapshot::comment", comment, |c| {
        c.comment = Some("a note".to_string());
    });

    set
}

/// Every field of [`TableSnapshot`], with a mutation each.
#[must_use]
pub fn table_snapshot_probes() -> ProbeSet<TableSnapshot> {
    let mut table: ProbeSet<TableSnapshot> = ProbeSet::new();
    // EXHAUSTIVE, no `..`.
    let TableSnapshot {
        columns,
        indexes,
        constraints,
        runtime_options,
        partition_by,
        comment,
        stored_create_sql,
    } = TableSnapshot {
        columns: Vec::new(),
        indexes: Vec::new(),
        constraints: Vec::new(),
        runtime_options: zero_migrate::TableRuntimeOptions::default(),
        partition_by: None,
        comment: None,
        stored_create_sql: None,
    };
    table.probe("TableSnapshot::columns", columns, |t| {
        t.columns = vec![ColumnSnapshot::default()];
    });
    table.probe("TableSnapshot::indexes", indexes, |t| {
        t.indexes = vec![IndexSnapshot::btree(
            "t_a_idx",
            false,
            vec!["a".to_string()],
        )];
    });
    table.probe("TableSnapshot::constraints", constraints, |t| {
        t.constraints = vec![ConstraintSnapshot {
            name: "c".to_string(),
            kind: "CHECK".to_string(),
            definition: "CHECK ((a > 0))".to_string(),
            comment: None,
            cascade_columns: None,
        }];
    });
    table.probe("TableSnapshot::runtime_options", runtime_options, |t| {
        t.runtime_options.soft_delete = true;
    });
    table.probe("TableSnapshot::partition_by", partition_by, |t| {
        t.partition_by = Some(zero_migrate::PartitionSpec::Range {
            columns: vec!["a".to_string()],
            collapse: false,
        });
    });
    table.probe("TableSnapshot::comment", comment, |t| {
        t.comment = Some("a note".to_string());
    });
    table.probe("TableSnapshot::stored_create_sql", stored_create_sql, |t| {
        t.stored_create_sql = Some("CREATE TABLE t (a)".to_string());
    });

    table
}

/// Every field of [`IndexSnapshot`], with a mutation each.
#[must_use]
pub fn index_snapshot_probes() -> ProbeSet<IndexSnapshot> {
    let mut index: ProbeSet<IndexSnapshot> = ProbeSet::new();
    // EXHAUSTIVE, no `..`.
    let IndexSnapshot {
        name,
        unique,
        columns,
        elements,
        access_method,
        predicate,
        include,
        with,
        only,
        opclass,
        nulls_not_distinct,
        comment,
        expr_cascade_columns,
    } = IndexSnapshot::btree("t_a_idx", false, vec!["a".to_string()]);
    index.probe("IndexSnapshot::name", name, |i| {
        i.name = "renamed_idx".to_string();
    });
    index.probe("IndexSnapshot::unique", unique, |i| i.unique = true);
    index.probe("IndexSnapshot::columns", columns, |i| {
        i.columns = vec!["b".to_string()];
    });
    index.probe("IndexSnapshot::elements", elements, |i| {
        i.elements = vec![IndexElementSnapshot::column("b")];
    });
    index.probe("IndexSnapshot::access_method", access_method, |i| {
        i.access_method = "gin".to_string();
    });
    index.probe("IndexSnapshot::predicate", predicate, |i| {
        i.predicate = Some("(a > 0)".to_string());
    });
    index.probe("IndexSnapshot::include", include, |i| {
        i.include = vec!["c".to_string()];
    });
    index.probe("IndexSnapshot::with", with, |i| {
        i.with = Some(IndexStorageParams {
            pages_per_range: None,
            fillfactor: Some(70),
        });
    });
    index.probe("IndexSnapshot::only", only, |i| i.only = true);
    index.probe("IndexSnapshot::opclass", opclass, |i| {
        i.opclass = Some("vector_cosine_ops".to_string());
    });
    index.probe(
        "IndexSnapshot::nulls_not_distinct",
        nulls_not_distinct,
        |i| i.nulls_not_distinct = true,
    );
    index.probe("IndexSnapshot::comment", comment, |i| {
        i.comment = Some("a note".to_string());
    });
    index.probe(
        "IndexSnapshot::expr_cascade_columns",
        expr_cascade_columns,
        |i| i.expr_cascade_columns = Some(vec!["a".to_string()]),
    );
    index
}

/// Every field of [`ConstraintSnapshot`], with a mutation each.
#[must_use]
pub fn constraint_snapshot_probes() -> ProbeSet<ConstraintSnapshot> {
    let mut constraint: ProbeSet<ConstraintSnapshot> = ProbeSet::new();
    // EXHAUSTIVE, no `..`.
    let ConstraintSnapshot {
        name,
        kind,
        definition,
        comment,
        cascade_columns,
    } = ConstraintSnapshot {
        name: "t_a_check".to_string(),
        kind: "CHECK".to_string(),
        definition: "CHECK ((a > 0))".to_string(),
        comment: None,
        cascade_columns: None,
    };
    constraint.probe("ConstraintSnapshot::name", name, |c| {
        c.name = "renamed".to_string();
    });
    constraint.probe("ConstraintSnapshot::kind", kind, |c| {
        c.kind = "UNIQUE".to_string();
    });
    constraint.probe("ConstraintSnapshot::definition", definition, |c| {
        c.definition = "CHECK ((a > 1))".to_string();
    });
    constraint.probe("ConstraintSnapshot::comment", comment, |c| {
        c.comment = Some("a note".to_string());
    });
    constraint.probe(
        "ConstraintSnapshot::cascade_columns",
        cascade_columns,
        |c| {
            c.cascade_columns = Some(vec!["a".to_string()]);
        },
    );
    constraint
}
