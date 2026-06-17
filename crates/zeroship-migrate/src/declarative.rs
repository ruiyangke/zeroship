//! Declarative schema-as-code: desired-schema compiler (v3 Plan A, phase P0).
//!
//! The platform's authoring layer holds a creator's **declared schema** — the
//! per-collection descriptor JSON the `@zeroship/db` SDK emits via `registerModel`
//! (`{ _meta, _indexes, <field>: { type, required, unique, default, ref } }`). This
//! module turns that declared schema into a deterministic [`SchemaSnapshot`]
//! ([`desired_snapshot`], P0). A later phase **diffs** it against the live
//! snapshot to generate migrations (the `DeclarativeAuthor`, P1/P2), all of which
//! flow through the unchanged guard → gate → executor pipeline (no DDL bypass).
//!
//! # Trust boundary
//!
//! Descriptor field/table names and types are **untrusted** (a prompt-injectable
//! AI authored them). The diff phase validates them at the author boundary
//! (mirroring [`crate::expand_contract`]) AND relies on the guard as the second
//! line; the pure P0 compiler here is a projection with no SQL emission.
//!
//! # Type-mapping provenance (shared-truth-to-extract-later)
//!
//! The DSL-type → Postgres-type table here is **replicated** from
//! `crates/plugin-db/src/query.rs` (`def_to_pg_type` /
//! `def_to_column_type_for_dialect`) and the platform system-field set
//! (`build_system_field_columns`). It is duplicated *deliberately*:
//! `zeroship-migrate` and `plugin-db` are different trust domains and the migrate
//! crate must not depend on the runtime plugin. The shared vocabulary should be
//! lifted into a small shared crate later; until then the
//! [`desired_snapshot`]-round-trips-to-live test (`tests/declarative_pg.rs`) is
//! the guard against the two copies drifting apart.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::drift::{
    ColumnSnapshot, ConstraintSnapshot, IndexSnapshot, SchemaSnapshot, TableSnapshot,
};

// ---------------------------------------------------------------------------
// Input contract — the per-collection declared-schema descriptor.
// ---------------------------------------------------------------------------

/// One field of a collection, as the `registerModel` descriptor declares it
/// (`{ type, required, unique, default, ref }`).
///
/// Untrusted: `name` and `ty` are validated at the author boundary before any
/// SQL is emitted (the diff phase, P1/P2).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FieldDescriptor {
    /// The field (column) name.
    pub name: String,
    /// The DSL type token (`string`, `number`, `boolean`, `date`,
    /// `calendarDate`, `json`, `object`, `array`, `union`, `ref`, `bytes`,
    /// `actor`, `id`). See [`dsl_to_pg_data_type`].
    #[serde(rename = "type")]
    pub ty: String,
    /// `true` ⇒ the column is `NOT NULL`.
    #[serde(default)]
    pub required: bool,
    /// `true` ⇒ a unique index is declared over this column. (Materialised as a
    /// `CREATE UNIQUE INDEX`, mirroring the SDK's A1 rule — never an inline
    /// `UNIQUE`.)
    #[serde(default)]
    pub unique: bool,
    /// For a `ref` field, the referenced collection (FK target table). `None`
    /// for non-`ref` fields.
    #[serde(rename = "ref", default)]
    pub references: Option<String>,
}

/// One declared index of a collection (the `_indexes` array entry).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IndexDescriptor {
    /// The index name (already collision-stable from the SDK).
    pub name: String,
    /// The columns the index covers, in order.
    pub columns: Vec<String>,
    /// `true` ⇒ a unique index.
    #[serde(default)]
    pub unique: bool,
}

/// A per-collection declared-schema descriptor (one table).
///
/// Mirrors the `registerModel` JSON the SDK emits, parsed into a typed shape:
/// `{ _meta, _indexes:[…], <field>:{…} }`. The `_meta` slot is opaque metadata
/// the migrate crate does not consume (it carries soft-delete / versioning flags
/// the SDK already expanded into concrete fields before this point).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CollectionDescriptor {
    /// The collection (table) name.
    pub name: String,
    /// The declared fields (columns), excluding platform system fields (those
    /// are injected by [`desired_snapshot`], matching the SDK's behaviour).
    #[serde(default)]
    pub fields: Vec<FieldDescriptor>,
    /// The declared named indexes (`_indexes`).
    #[serde(default)]
    pub indexes: Vec<IndexDescriptor>,
}

// ---------------------------------------------------------------------------
// DSL-type → information_schema.data_type mapping (replicated from plugin-db).
// ---------------------------------------------------------------------------

/// Map a DSL type token to the EXACT `information_schema.columns.data_type`
/// string Postgres reports for the column `plugin-db` would emit for it.
///
/// This is the type-fidelity core. The mapping is replicated from
/// `crates/plugin-db/src/query.rs::def_to_pg_type` + `def_to_column_type_for_dialect`
/// (see the module-level provenance note) and translated from the DDL spelling
/// (`TEXT`, `DOUBLE PRECISION`, `TIMESTAMPTZ`, …) to the canonical
/// `information_schema` spelling (`text`, `double precision`,
/// `timestamp with time zone`, …) — NOT the DDL alias.
///
/// An unknown token maps to `text`, mirroring `def_to_pg_type`'s `_ => "TEXT"`
/// fallback, so an unrecognised type is stored conservatively rather than
/// failing the compile.
#[must_use]
#[allow(
    clippy::match_same_arms,
    reason = "the named arms and the `_` fallback both map to `text` by \
              coincidence, not intent — keeping them separate documents which \
              DSL types are KNOWN-text (string/ref/actor/id) versus the \
              conservative unknown-type fallback (mirrors plugin-db's `_ => TEXT`)"
)]
pub fn dsl_to_pg_data_type(dsl_type: &str) -> &'static str {
    match dsl_type {
        // string / ref / actor / id all land on `text` (def_to_pg_type +
        // the `_ => TEXT` default; `ref` is the FK column, `id` is the PK).
        "string" | "ref" | "actor" | "id" => "text",
        // t.number() → DOUBLE PRECISION (FLOAT8).
        "number" => "double precision",
        "boolean" => "boolean",
        // t.date() → TIMESTAMPTZ; information_schema spells it long-form.
        "date" => "timestamp with time zone",
        // t.calendarDate() → DATE.
        "calendarDate" => "date",
        // json / object / array / union → JSONB.
        "json" | "object" | "array" | "union" => "jsonb",
        // t.bytes() → BYTEA.
        "bytes" => "bytea",
        // Conservative fallback (matches def_to_pg_type's `_ => TEXT`).
        _ => "text",
    }
}

/// The seven platform-managed system fields, in canonical order, as
/// [`ColumnSnapshot`]s. Replicated from `plugin-db`'s
/// `build_system_field_columns` (`id TEXT PRIMARY KEY`, `created_at`/`updated_at`
/// `TIMESTAMPTZ NOT NULL`, `created_by`/`updated_by` `TEXT NULL`, `version`
/// `INTEGER NOT NULL`, `deleted_at` `TIMESTAMPTZ NULL`), expressed in
/// `information_schema` data-type spelling.
///
/// Every collection table gets these injected by [`desired_snapshot`], matching
/// what `installSchema` materialises, so the desired snapshot round-trips to the
/// live table the SDK creates.
fn system_field_columns() -> Vec<ColumnSnapshot> {
    let ts = "timestamp with time zone";
    vec![
        ColumnSnapshot { name: "id".into(), data_type: "text".into(), nullable: false },
        ColumnSnapshot { name: "created_at".into(), data_type: ts.into(), nullable: false },
        ColumnSnapshot { name: "updated_at".into(), data_type: ts.into(), nullable: false },
        ColumnSnapshot { name: "created_by".into(), data_type: "text".into(), nullable: true },
        ColumnSnapshot { name: "updated_by".into(), data_type: "text".into(), nullable: true },
        ColumnSnapshot { name: "version".into(), data_type: "integer".into(), nullable: false },
        ColumnSnapshot { name: "deleted_at".into(), data_type: ts.into(), nullable: true },
    ]
}

// ---------------------------------------------------------------------------
// P0 — desired_snapshot compiler.
// ---------------------------------------------------------------------------

/// Compile a set of [`CollectionDescriptor`]s into a deterministic
/// [`SchemaSnapshot`] — the **desired** schema (P0).
///
/// For each collection it emits a [`TableSnapshot`] whose:
/// - **columns** are the seven platform system fields (see
///   [`system_field_columns`]) plus one column per declared field, with the
///   `data_type` from [`dsl_to_pg_data_type`] and `nullable = !required`;
/// - **constraints** carry the `id` PRIMARY KEY (named `<table>_pkey`, the
///   Postgres default) and one FOREIGN KEY per `ref` field;
/// - **indexes** carry the declared named indexes, a unique index per
///   `unique: true` field, and the PRIMARY KEY's implicit `<table>_pkey` index.
///
/// The snapshot is the same shape [`snapshot_schema`](crate::drift::snapshot_schema)
/// produces from the live DB, so a freshly-created table introspects to a
/// byte-equal snapshot (zero drift) — that equality is the P0 type-fidelity
/// proof.
///
/// **Pure.** No I/O, no DDL, no validation side effects. Name/type validation
/// happens at the *author* boundary (the diff phase); this compiler is a pure
/// projection so it can be reused for drift comparison too.
#[must_use]
pub fn desired_snapshot(descriptors: &[CollectionDescriptor]) -> SchemaSnapshot {
    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();

    for d in descriptors {
        let mut columns = system_field_columns();
        let mut indexes: Vec<IndexSnapshot> = Vec::new();
        let mut constraints: Vec<ConstraintSnapshot> = Vec::new();

        // The id PRIMARY KEY (Postgres names a bare `PRIMARY KEY` constraint
        // `<table>_pkey`). Definition matches pg_get_constraintdef's spelling.
        constraints.push(ConstraintSnapshot {
            name: format!("{}_pkey", d.name),
            kind: "PRIMARY KEY".into(),
            definition: "PRIMARY KEY (id)".into(),
        });
        // A PRIMARY KEY also materialises an IMPLICIT unique index named
        // `<table>_pkey` (pg_index reports it). The live snapshot always carries
        // it, so the desired snapshot must too — otherwise the differ would read
        // it as an out-of-band index to DROP. It is created by the `PRIMARY KEY`
        // clause, never by a standalone CREATE INDEX.
        indexes.push(IndexSnapshot {
            name: format!("{}_pkey", d.name),
            unique: true,
        });

        for f in &d.fields {
            columns.push(ColumnSnapshot {
                name: f.name.clone(),
                data_type: dsl_to_pg_data_type(&f.ty).to_string(),
                nullable: !f.required,
            });
            // A `unique: true` field becomes a unique index (A1 rule). The
            // name mirrors plugin-db's deterministic per-field index name.
            if f.unique {
                indexes.push(IndexSnapshot {
                    name: unique_index_name(&d.name, &f.name),
                    unique: true,
                });
            }
            // A `ref` field declares a FOREIGN KEY constraint.
            if f.ty == "ref" {
                if let Some(target) = &f.references {
                    constraints.push(ConstraintSnapshot {
                        name: fk_constraint_name(&f.name),
                        kind: "FOREIGN KEY".into(),
                        // pg_get_constraintdef spelling; the desired-side
                        // definition is informational (drift compares it only
                        // when the live side populates it).
                        definition: format!(
                            "FOREIGN KEY ({}) REFERENCES {} (id)",
                            f.name, target
                        ),
                    });
                }
            }
        }

        for idx in &d.indexes {
            indexes.push(IndexSnapshot {
                name: idx.name.clone(),
                unique: idx.unique,
            });
        }

        // Deterministic ordering (snapshot_schema sorts everything by name).
        columns.sort_by(|a, b| a.name.cmp(&b.name));
        indexes.sort_by(|a, b| a.name.cmp(&b.name));
        constraints.sort_by(|a, b| a.name.cmp(&b.name));

        tables.insert(d.name.clone(), TableSnapshot { columns, indexes, constraints });
    }

    SchemaSnapshot { tables }
}

/// Deterministic name for a per-field unique index (`<table>_<field>_key`,
/// matching the Postgres convention so the desired snapshot round-trips to the
/// live one a `CREATE UNIQUE INDEX` of this name produces).
fn unique_index_name(table: &str, field: &str) -> String {
    format!("{table}_{field}_key")
}

/// Deterministic FK constraint name (`<field>_fkey`, mirroring plugin-db's
/// `fk_constraint_name`).
fn fk_constraint_name(field: &str) -> String {
    format!("{field}_fkey")
}
