//! Declarative schema-as-code: desired-schema → generated migrations
//! (v3 Plan A, phases P0–P2).
//!
//! The platform's authoring layer holds a creator's **declared schema** — the
//! per-collection descriptor JSON the `@zeroship/db` SDK emits via `registerModel`
//! (`{ _meta, _indexes, <field>: { type, required, unique, default, ref } }`). This
//! module turns that declared schema into a deterministic [`SchemaSnapshot`]
//! ([`desired_snapshot`], P0) and then **diffs** it against the live snapshot to
//! generate migrations ([`DeclarativeAuthor::diff`], P1 additive + P2
//! destructive-gated).
//!
//! The differ is a new **author**, not a new executor: every [`Migration`] it
//! produces still flows through the unchanged
//! [`plan`](crate::engine::MigrationEngine::plan) →
//! [`guard`](crate::guard::SqlGuard) →
//! [`gate`](crate::engine::MigrationEngine::apply) →
//! [`executor::apply`](crate::executor::apply) pipeline. There is no DDL bypass.
//!
//! # Trust boundary
//!
//! Descriptor field/table names and types are **untrusted** (a prompt-injectable
//! AI authored them). They are validated at the author boundary
//! ([`validate_ident`] / [`validate_type`], mirroring
//! [`crate::expand_contract`]) AND re-checked by the guard as the second line.
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

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::drift::{
    ColumnSnapshot, ConstraintSnapshot, IndexSnapshot, SchemaSnapshot, TableSnapshot,
};
use crate::expand_contract::{
    ExpandContractAuthor, ExpandContractError, ExpandContractPlan, OnlineIntent,
};
use crate::migration::{Checksum, Migration, MigrationFlags, MigrationId};

/// Quote a Postgres identifier (double embedded quotes, wrap in `"`). Mirrors
/// [`crate::author`]'s quoting so emitted SQL is injection-safe even past the
/// author-boundary `validate_ident` (defense in depth — the guard is line two).
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

// ---------------------------------------------------------------------------
// Input contract — the per-collection declared-schema descriptor.
// ---------------------------------------------------------------------------

/// One field of a collection, as the `registerModel` descriptor declares it
/// (`{ type, required, unique, default, ref }`).
///
/// Untrusted: `name` and `ty` are validated at the author boundary before any
/// SQL is emitted (see [`DeclarativeAuthor::diff`]).
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
// P3 — rename hints (the OPT-IN, never-heuristic rename surface).
// ---------------------------------------------------------------------------

/// An **explicit** column-rename hint.
///
/// "On `table`, the column called `from` (present in live) is the column called
/// `to` (present in desired) — they are the same column under a new name, NOT a
/// drop+add."
///
/// Renames are **opt-in by hint ONLY** — the differ NEVER infers a rename from a
/// drop+add pair heuristically (that risks silent data loss: a coincidental
/// "drop col X, add col Y" on the same table is two independent intents, and
/// treating it as a rename would carry X's data into Y against the creator's
/// will, or — worse — a misclassified rename could drop the wrong column). A
/// hint is the creator's signed statement of intent; without one, a drop+add
/// stays two independent ops (a gated DROP + an additive ADD).
///
/// When a hint matches an actual drop+add pair (and the types are compatible),
/// the differ routes that pair through the zero-downtime expand-contract path
/// ([`ExpandContractAuthor::RenameColumn`](crate::expand_contract)) instead of
/// emitting drop+add — the column's data is preserved by the dual-write +
/// backfill sequence, and the destructive `DROP COLUMN <from>` is gated.
///
/// The DSL `renamedFrom` surface that produces these hints is a separate SDK
/// follow-up; this struct is the engine-side input contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameHint {
    /// The table the rename happens on.
    pub table: String,
    /// The existing (live) column name being renamed away from.
    pub from: String,
    /// The new (desired) column name being renamed to.
    pub to: String,
}

/// A [`RenameHint`] that has been **verified** against the desired/live snapshots
/// (matched an actual drop+add pair with identical types). The diff routes each
/// one through the expand-contract rename sequence. `ty` is the shared
/// `information_schema` data-type spelling of the two matched columns.
#[derive(Debug, Clone)]
struct ResolvedRename {
    table: String,
    from: String,
    to: String,
    ty: String,
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
/// Exactly the twelve in-scope DSL tokens map; anything else (an out-of-scope
/// extension/parameterised type — `vector`/`geoPoint`/`encrypted` — OR a typo /
/// wrong spelling — `bigint`/`uuid`/`int4`/`serial`/`__proto__`) is rejected
/// with [`DeclarativeError::UnsupportedType`] BEFORE any SQL is emitted. There
/// is deliberately NO `_ => text` fallback: silently degrading an unrecognised
/// type to `text` (#2) gave the creator a column they never declared and a
/// permanent divergence from what plugin-db's runtime materialises.
///
/// # Errors
/// [`DeclarativeError::UnsupportedType`] if `dsl_type` is not one of the twelve
/// supported tokens.
pub fn dsl_to_pg_data_type(dsl_type: &str) -> Result<&'static str, DeclarativeError> {
    Ok(match dsl_type {
        // string / ref / actor / id all land on `text` (def_to_pg_type;
        // `ref` is the FK column, `id` is the PK).
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
        // No silent fallback: an unrecognised or out-of-scope type is an error,
        // not a `text` column (#2).
        other => {
            return Err(DeclarativeError::UnsupportedType { ty: other.to_string() });
        }
    })
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
/// - **indexes** carry the declared named indexes plus a unique index per
///   `unique: true` field.
///
/// The snapshot is the same shape [`snapshot_schema`](crate::drift::snapshot_schema)
/// produces from the live DB, so a freshly-created table introspects to a
/// byte-equal snapshot (zero drift) — that equality is the P0 type-fidelity
/// proof.
///
/// `project_schema` is the schema every table lives in; it is needed because a
/// FOREIGN KEY's `pg_get_constraintdef` body is **schema-qualified**
/// (`FOREIGN KEY (col) REFERENCES <schema>.target(id)`), so the desired-side FK
/// definition must carry the same qualification to match live exactly — otherwise
/// every FK shows permanent phantom drift (1b). It is NOT used for any non-FK
/// part of the snapshot.
///
/// **Pure.** No I/O, no DDL. It performs the minimal author-boundary check that
/// guards the *projection itself* — an unrecognised/out-of-scope field type (#2)
/// — so a degraded snapshot (the creator declared X, would have got `text`) is
/// never produced. Full identifier re-validation still happens in
/// [`DeclarativeAuthor::diff`] (defense in depth) and the guard is the second
/// line.
///
/// # Errors
/// - [`DeclarativeError::UnsupportedType`] — a field used a type token outside
///   the twelve supported (or an out-of-scope `vector`/`geoPoint`/`encrypted`).
/// - [`DeclarativeError::DuplicateTable`] — two descriptors share a table name.
/// - [`DeclarativeError::Invalid`] — a `ref` field's target table is not a safe
///   bare identifier.
pub fn desired_snapshot(
    project_schema: &str,
    descriptors: &[CollectionDescriptor],
) -> Result<SchemaSnapshot, DeclarativeError> {
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
        // clause, never by a standalone CREATE INDEX, so the differ skips it
        // (see `is_pk_index`).
        indexes.push(IndexSnapshot {
            name: format!("{}_pkey", d.name),
            unique: true,
            // The PK's implicit index covers `id` (live `pg_index` reports the
            // same key column). The differ never emits DDL for it (see
            // `is_pk_index`), but the snapshot must carry the column list so the
            // attribute-aware diff stays clean against live.
            columns: vec!["id".into()],
        });

        for f in &d.fields {
            columns.push(ColumnSnapshot {
                name: f.name.clone(),
                data_type: dsl_to_pg_data_type(&f.ty)?.to_string(),
                nullable: !f.required,
            });
            // A `unique: true` field becomes a unique index (A1 rule). The
            // name mirrors plugin-db's deterministic per-field index name.
            if f.unique {
                indexes.push(IndexSnapshot {
                    name: unique_index_name(&d.name, &f.name),
                    unique: true,
                    columns: vec![f.name.clone()],
                });
            }
            // A `ref` field declares a FOREIGN KEY constraint.
            if f.ty == "ref" {
                if let Some(target) = &f.references {
                    // #3-ref: the FK target table is interpolated into
                    // `REFERENCES <schema>.<target>(id)`; validate it as a bare
                    // identifier at the author boundary (mirroring how table /
                    // column names are checked) so a malformed / injecting ref
                    // target (`control.users`, `x"; DROP …`, `;`) is rejected
                    // up-front rather than relying on downstream quoting alone.
                    validate_ident("ref target", target)?;
                    constraints.push(ConstraintSnapshot {
                        name: fk_constraint_name(&f.name),
                        kind: "FOREIGN KEY".into(),
                        // EXACT `pg_get_constraintdef` spelling (1b): the target
                        // is schema-qualified and there is NO space before `(id)`.
                        // The generated FK carries no ON DELETE / ON UPDATE /
                        // DEFERRABLE clause, and `pg_get_constraintdef` renders
                        // none for a bare FK, so this body is byte-identical to
                        // live — no phantom drift, and the differ can compare FK
                        // bodies on existing tables (a changed target is caught,
                        // not silently skipped).
                        definition: format!(
                            "FOREIGN KEY ({}) REFERENCES {}.{}(id)",
                            f.name, project_schema, target
                        ),
                    });
                }
            }
        }

        for idx in &d.indexes {
            // Carry the declared columns through VERBATIM (1a) — recovering them
            // from the index name was unsound for composite / custom-named
            // indexes. `render_create_index` emits this list directly.
            indexes.push(IndexSnapshot {
                name: idx.name.clone(),
                unique: idx.unique,
                columns: idx.columns.clone(),
            });
        }

        // Deterministic ordering (snapshot_schema sorts everything by name).
        columns.sort_by(|a, b| a.name.cmp(&b.name));
        indexes.sort_by(|a, b| a.name.cmp(&b.name));
        constraints.sort_by(|a, b| a.name.cmp(&b.name));

        // #6: refuse a duplicate table name rather than silently merging
        // (last-writer-wins would clobber the first descriptor's columns with
        // no error). Until P4 (union/ownership) lands, two descriptors naming
        // the same table is an author error.
        if tables.contains_key(&d.name) {
            return Err(DeclarativeError::DuplicateTable { table: d.name.clone() });
        }
        tables.insert(d.name.clone(), TableSnapshot { columns, indexes, constraints });
    }

    Ok(SchemaSnapshot { tables })
}

/// True if `index_name` is the implicit index a PRIMARY KEY materialises
/// (`<table>_pkey`). It is created/dropped by the PK clause, never by a
/// standalone CREATE/DROP INDEX, so the differ never emits DDL for it.
fn is_pk_index(table: &str, index_name: &str) -> bool {
    index_name == format!("{table}_pkey")
}

/// Deterministic name for a per-field unique index (`<table>_<field>_key`,
/// matching the Postgres convention so the desired snapshot round-trips to the
/// live one a `CREATE UNIQUE INDEX` of this name produces). Capped to ≤63 bytes
/// via [`crate::author::cap_ident_name`] (1c) — an un-capped name would be
/// truncated server-side on CREATE, so the desired (full) name would never match
/// the live (truncated) name and a re-diff would churn DROP+CREATE forever.
fn unique_index_name(table: &str, field: &str) -> String {
    crate::author::cap_ident_name(&format!("{table}_{field}_key"))
}

/// Deterministic FK constraint name (`<field>_fkey`, mirroring plugin-db's
/// `fk_constraint_name`).
fn fk_constraint_name(field: &str) -> String {
    format!("{field}_fkey")
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

/// A failure to diff a declarative desired schema against the live one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeclarativeError {
    /// A descriptor name/type was not a safe bare identifier / type at the
    /// author boundary (mirrors [`crate::expand_contract`]'s `validate_ident` /
    /// `validate_type`). Nothing is generated.
    #[error("invalid descriptor: {0}")]
    Invalid(String),
    /// The diff requires an op v1 does not generate: an in-place INDEX or
    /// FOREIGN KEY redefinition (a flipped `unique` flag, a changed column set, a
    /// re-pointed FK target — each a DROP+CREATE deferred to a later phase).
    /// Surfaced explicitly — never silently skipped. (Type/nullability changes are
    /// handled in P3 as gated/ungated ALTERs; destructive DROPs are P2 gated
    /// migrations — neither uses this error.)
    #[error("unsupported in v1 (deferred to a later phase): {0}")]
    UnsupportedInV1(String),
    /// A declared field used a DSL type token the v1 differ does not map. This
    /// covers both out-of-scope parameterised/extension types
    /// (`vector`/`geoPoint`/`encrypted`) AND typos / wrong spellings
    /// (`bigint`, `uuid`, `int4`, `serial`, …). It is rejected at the author
    /// boundary BEFORE any SQL is emitted, rather than silently degrading to a
    /// `text` column (#2 — the creator declared X, would have got `text`, with
    /// permanent divergence from what plugin-db materialises).
    #[error(
        "unsupported field type '{ty}' (not mapped in v1; vector/geoPoint/encrypted \
         are out of v1 scope). Supported: string, number, boolean, date, calendarDate, \
         json, object, array, union, ref, bytes, actor, id"
    )]
    UnsupportedType {
        /// The unrecognised / out-of-scope DSL type token.
        ty: String,
    },
    /// Two descriptors declared the same table name. Until P4 (union/ownership)
    /// lands, [`desired_snapshot`] refuses to silently merge them (last-writer-wins
    /// would clobber the first descriptor's columns with no error). (#6)
    #[error("duplicate table name across descriptors: '{table}'")]
    DuplicateTable {
        /// The table name declared by more than one descriptor.
        table: String,
    },
    /// A [`RenameHint`] (P3) did not match an actual drop+add pair: the `from`
    /// column is not present in live as a dropped column, OR the `to` column is
    /// not present in desired as an added column, on the named table. The hint is
    /// the creator's signed statement of intent, so an un-matchable hint is a hard
    /// error — never silently ignored (a silently-dropped hint would fall back to
    /// an unintended gated-drop + additive-add, losing the column's data).
    #[error(
        "rename hint {table}.{from} → {to} does not match a drop+add pair \
         (from must be a live-only column and to a desired-only column on {table})"
    )]
    RenameHintUnmatched {
        /// The table the hint named.
        table: String,
        /// The `from` column the hint named (expected: live-only).
        from: String,
        /// The `to` column the hint named (expected: desired-only).
        to: String,
    },
    /// A [`RenameHint`] (P3) matched a drop+add pair whose **types differ**: the
    /// live `from` column and the desired `to` column do not share a `data_type`.
    /// A pure online rename (expand-contract dual-write) requires type identity —
    /// a simultaneous rename + type change is two distinct intents and is refused
    /// rather than silently mirrored across incompatible types (which the
    /// dual-write `NEW.<to> := NEW.<from>` assignment could corrupt or reject).
    #[error(
        "rename hint {table}.{from} → {to} matched, but the types differ \
         ({from_type} → {to_type}); a rename requires type identity (rename + \
         type change is two separate intents)"
    )]
    RenameHintTypeMismatch {
        /// The table the hint named.
        table: String,
        /// The `from` column.
        from: String,
        /// The `to` column.
        to: String,
        /// The live `from` column's data type.
        from_type: String,
        /// The desired `to` column's data type.
        to_type: String,
    },
    /// Two [`RenameHint`]s on the same table shared a `from` (e.g. `[a→c, a→d]`)
    /// or a `to` (e.g. `[a→c, b→c]`) column. Each hint resolves INDEPENDENTLY, so
    /// a shared endpoint produces two colliding expand-contract sequences: a
    /// duplicated `ADD COLUMN <to>` (the second fails `already exists`), divergent
    /// dual-write triggers, or a double `DROP COLUMN <from>`. The cross-hint
    /// validation pass rejects it before any SQL is authored (H1). `side` is
    /// `"from"` or `"to"` — which endpoint was duplicated.
    #[error(
        "duplicate rename hint endpoint: column {table}.{column} appears as the \
         {side} of more than one hint; a column may be renamed at most once per \
         deploy"
    )]
    DuplicateRenameHint {
        /// The table the colliding hints named.
        table: String,
        /// The column that appeared more than once on the same side.
        column: String,
        /// Which endpoint collided: `"from"` or `"to"`.
        side: &'static str,
    },
    /// A [`RenameHint`]'s `to` equals another hint's `from` on the same table
    /// (e.g. `[a→b, b→c]`) — a rename CHAIN. Chains are not supported: the engine
    /// resolves each hint against the single live/desired snapshot pair, where the
    /// intermediate name (`b`) cannot be simultaneously a live-only drop and a
    /// desired-only add. Reject it EXPLICITLY rather than leave it to surface
    /// incidentally as an [`DeclarativeError::RenameHintUnmatched`] (H2).
    #[error(
        "rename hint chain on {table}: column {column} is both the target of one \
         hint and the source of another; chained renames are unsupported (resolve \
         them as separate deploys)"
    )]
    RenameHintChained {
        /// The table the chained hints named.
        table: String,
        /// The intermediate column that is both a `to` and a `from`.
        column: String,
    },
    /// A [`RenameHint`] had `from == to` — a no-op rename of a column to its own
    /// name. This is rejected with a PRECISE error rather than the misleading
    /// [`DeclarativeError::RenameHintUnmatched`] it would otherwise produce (the
    /// identical name is neither live-only nor desired-only) (M1).
    #[error(
        "no-op rename hint on {table}: from and to are the same column ({column}); \
         a rename must change the column name"
    )]
    RenameHintNoop {
        /// The table the hint named.
        table: String,
        /// The identical `from`/`to` column name.
        column: String,
    },
    /// Authoring the expand-contract rename sequence for a matched [`RenameHint`]
    /// failed (e.g. an identifier that passed the declarative author boundary was
    /// rejected by the stricter expand-contract author boundary). Surfaced rather
    /// than swallowed.
    #[error("failed to author rename expand-contract sequence: {0}")]
    Rename(#[from] ExpandContractError),
}

// ---------------------------------------------------------------------------
// The structured diff result.
// ---------------------------------------------------------------------------

/// The **structured** result of [`DeclarativeAuthor::diff`].
///
/// It carries the plain (additive / destructive) migrations PLUS the online
/// renames, each kept as its full [`ExpandContractPlan`] and NOT flattened into
/// the plain set.
///
/// # Why a declarative rename must NOT be flattened (C1 — data loss)
///
/// A column rename is an **online, multi-deploy** operation, not a single
/// statement. Its [`ExpandContractPlan`] is more than a list of `Migration`s: it
/// also carries the [`BackfillSpec`](crate::backfill::BackfillSpec) that mirrors
/// **pre-existing** rows from `<from>` into `<to>`. E3's `up` is only a `SELECT 1`
/// marker — the actual data copy is [`run_backfill`](crate::backfill::run_backfill),
/// driven exclusively by [`run_expand`](crate::engine::MigrationEngine::run_expand).
///
/// If the rename were flattened into the plain migration set (`out.extend(plan.all())`)
/// and pushed through `plan` → `executor::apply`, the backfill would NEVER run:
/// E3's marker journals as "done" without the rows ever being copied, and the
/// contract `DROP COLUMN <from>` then destroys the originals → **data loss**.
/// (A flat batch is also dead-on-arrival: the executor's expand/contract gate
/// refuses the contract while its own expand is still pending.)
///
/// So the differ keeps renames structured. The caller drives them through
/// [`MigrationEngine::apply_declarative`](crate::engine::MigrationEngine::apply_declarative),
/// which runs the REAL backfill and surfaces the contract as a DEFERRED set for a
/// later deploy.
#[derive(Debug, Clone, Default)]
pub struct DeclarativePlan {
    /// The plain additive / destructive migrations (CREATE TABLE, ADD/DROP
    /// COLUMN, indexes, FKs, type / nullability changes). A rename's `<from>` is
    /// EXCLUDED from the destructive drop pass (its drop is the deferred contract)
    /// and its `<to>` is EXCLUDED from the additive add pass (the expand adds it).
    pub migrations: Vec<Migration>,
    /// The online renames, each as a full [`ExpandContractPlan`] (expand migs +
    /// `BackfillSpec` + contract migs). NEVER flattened into `migrations`.
    pub renames: Vec<ExpandContractPlan>,
}

impl DeclarativePlan {
    /// True if the plan reconciles nothing — no plain migrations AND no renames.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.migrations.is_empty() && self.renames.is_empty()
    }

    /// All migrations the plan would ultimately apply, flattened (plain set +
    /// every rename's expand-then-contract migrations) — for **inspection /
    /// preview only** (lint, counting, SQL-shape assertions). This is NOT an
    /// apply order: a rename's expand and contract belong to DIFFERENT deploys,
    /// and the backfill between them is not a `Migration`. Use
    /// [`apply_declarative`](crate::engine::MigrationEngine::apply_declarative)
    /// to apply.
    #[must_use]
    pub fn all_migrations(&self) -> Vec<Migration> {
        let mut all = self.migrations.clone();
        for r in &self.renames {
            all.extend(r.all());
        }
        all
    }
}

// ---------------------------------------------------------------------------
// P1/P2 — the declarative differ.
// ---------------------------------------------------------------------------

/// The declarative differ — turns a desired/live snapshot pair into the
/// migrations that reconcile them (P1 additive + P2 destructive-gated).
///
/// A [`MigrationAuthor`](crate::author::MigrationAuthor)-family author: it
/// reuses [`DeterministicAuthor`] rendering where possible and emits
/// [`Migration`]s with correct [`MigrationFlags`]. It validates every descriptor
/// name/type at its boundary and relies on the guard as the second line.
#[derive(Debug, Clone)]
pub struct DeclarativeAuthor {
    /// The project schema every emitted statement is qualified into.
    project_schema: String,
    /// The declaring app (`app_…`) recorded on each migration.
    owner_app: String,
}

impl DeclarativeAuthor {
    /// Construct a declarative author bound to a project schema + owner app.
    #[must_use]
    pub fn new(project_schema: impl Into<String>, owner_app: impl Into<String>) -> Self {
        Self {
            project_schema: project_schema.into(),
            owner_app: owner_app.into(),
        }
    }

    /// Render `<schema>.<object>`, both parts quoted.
    fn qualified(&self, object: &str) -> String {
        format!("{}.{}", quote_ident(&self.project_schema), quote_ident(object))
    }

    /// Build a [`Migration`] from rendered `up`/`down` SQL + flags + deps.
    fn make(
        &self,
        name: &str,
        up: String,
        down: Option<String>,
        flags: MigrationFlags,
        depends_on: Vec<MigrationId>,
    ) -> Migration {
        let checksum = Checksum::of(&up, down.as_deref());
        Migration {
            version: MigrationId::generate(),
            name: name.to_string(),
            up,
            down,
            checksum,
            flags,
            owner_app: self.owner_app.clone(),
            depends_on,
            supersedes: Vec::new(),
        }
    }

    /// Diff the **desired** snapshot against the **live** snapshot and generate
    /// the migrations that reconcile them.
    ///
    /// P1 (additive) handles:
    /// - **CREATE TABLE** — a table in desired, absent in live (with its
    ///   columns, PK, unique indexes, and own-table FKs inlined; FKs to a
    ///   not-yet-created table are deferred to a follow-on `ALTER TABLE ADD
    ///   CONSTRAINT`, mirroring plugin-db's deferred-FK pattern);
    /// - **ADD COLUMN** — a column in desired, absent in a live table;
    /// - **CREATE INDEX** — an index in desired, absent in a live table.
    ///
    /// P2 (destructive, gated) handles a live-only object (absent in desired):
    /// - **DROP TABLE / DROP COLUMN** — DATA LOSS: the classifier/guard marks
    ///   these destructive, so the existing engine gate refuses them without
    ///   [`Approval::Approved`](crate::Approval). NEVER auto-applied.
    /// - **DROP INDEX** — a PLAIN index DROP is NOT data loss (reversible by
    ///   recreating the index), so it flows through ungated, the same as an
    ///   additive op. A **UNIQUE** index DROP, however, silently removes a
    ///   data-integrity guarantee (#4), so it is classified `destructive +
    ///   requires_approval` (gated, like DROP COLUMN) — see [`render_drop_index`].
    ///
    /// P3 (rename, opt-in) routes a **hinted** drop+add pair through the
    /// zero-downtime expand-contract sequence
    /// ([`ExpandContractAuthor::RenameColumn`](crate::expand_contract)) instead of
    /// emitting an independent drop + add. A rename is emitted ONLY when a
    /// [`RenameHint`] explicitly names the `(table, from→to)` pair AND `from` is a
    /// live-only column AND `to` is a desired-only column AND their types match.
    /// Without a matching hint, a drop+add stays two independent ops — the differ
    /// NEVER infers a rename heuristically (that risks silent data loss).
    ///
    /// P3 (type / nullability) handles a same-name column whose attributes
    /// changed (these were `UnsupportedInV1` before P3):
    /// - **type change** → a GATED `ALTER COLUMN … TYPE …` (`destructive` +
    ///   `requires_approval`; no auto type-change in v1);
    /// - **`DROP NOT NULL`** (required true→false) → an ungated additive
    ///   `ALTER COLUMN DROP NOT NULL` (relaxing a constraint is safe);
    /// - **`SET NOT NULL`** (required false→true) → a GATED `ALTER COLUMN SET NOT
    ///   NULL` (lock-heavy + can fail on existing NULLs).
    ///
    /// Ordering: CREATE TABLE precede their own indexes; FK-target tables are
    /// created before referencing tables (deferred FK breaks cycles); the
    /// per-version `UUIDv7` gives a stable total order, and `depends_on` records
    /// cross-table deps for the executor's topo sort.
    ///
    /// # Errors
    /// - [`DeclarativeError::Invalid`] — a descriptor name/type failed the
    ///   author-boundary validation (nothing generated).
    /// - [`DeclarativeError::RenameHintUnmatched`] — a hint named a pair that is
    ///   not an actual drop+add.
    /// - [`DeclarativeError::RenameHintTypeMismatch`] — a hint matched a pair
    ///   whose types differ.
    /// - [`DeclarativeError::UnsupportedInV1`] — an index/FK in-place
    ///   redefinition (still deferred).
    #[allow(
        clippy::too_many_lines,
        reason = "the diff is one cohesive pass — new tables (FK-ordered), \
                  deferred FKs, then per-table column/index add + gated drops — \
                  that reads more clearly as a single function than split across \
                  helpers that would each need the shared created_version map"
    )]
    pub fn diff(
        &self,
        desired: &SchemaSnapshot,
        live: &SchemaSnapshot,
        hints: &[RenameHint],
    ) -> Result<DeclarativePlan, DeclarativeError> {
        // Author-boundary validation: every desired table/column/index name and
        // every column data_type must be safe BEFORE we render any SQL.
        Self::validate_desired(desired)?;

        // Resolve + validate the rename hints up-front: every hint MUST match an
        // actual drop+add pair (from live-only, to desired-only, types equal) on
        // its table. An un-matchable / type-mismatched hint is a hard error (the
        // hint is the creator's signed intent — never silently ignored). Returns
        // the per-table set of (from,to,type) renames the column diff will route
        // through expand-contract instead of emitting drop+add.
        let resolved = Self::resolve_rename_hints(desired, live, hints)?;

        let mut out: Vec<Migration> = Vec::new();
        // The online renames, carried as their full ExpandContractPlan (expand
        // migs + BackfillSpec + contract migs) — NOT flattened into `out` (C1).
        // Flattening would discard the BackfillSpec, so the pre-existing-row
        // mirror never runs and the contract DROP COLUMN <from> destroys data.
        let mut renames: Vec<ExpandContractPlan> = Vec::new();

        // --- New tables (in desired, not in live), in FK-dependency order. ---
        let new_tables: Vec<&String> = desired
            .tables
            .keys()
            .filter(|t| !live.tables.contains_key(*t))
            .collect();
        let order = topo_order_new_tables(desired, &new_tables);

        // Map each newly-created table to its CREATE migration's version, so a
        // deferred FK (or an FK inlined into a table created earlier in this
        // batch) can `depends_on` the target's creation.
        let mut created_version: BTreeMap<String, MigrationId> = BTreeMap::new();
        // FKs that must be deferred (target not yet created when the table is
        // emitted) → emitted as ALTER TABLE ADD CONSTRAINT after all CREATEs.
        let mut deferred_fks: Vec<(String, ConstraintSnapshot)> = Vec::new();

        for table in &order {
            let t = &desired.tables[*table];
            // Inline only the FKs whose target table already exists (live) or
            // was created earlier in this batch; defer the rest.
            let mut inline_fks: Vec<&ConstraintSnapshot> = Vec::new();
            let mut depends_on: Vec<MigrationId> = Vec::new();
            for c in &t.constraints {
                if c.kind != "FOREIGN KEY" {
                    continue;
                }
                let target = fk_target_table(&c.definition);
                match target {
                    Some(tt)
                        if live.tables.contains_key(&tt)
                            || created_version.contains_key(&tt) =>
                    {
                        if let Some(v) = created_version.get(&tt) {
                            depends_on.push(v.clone());
                        }
                        inline_fks.push(c);
                    }
                    _ => deferred_fks.push(((*table).clone(), c.clone())),
                }
            }

            let up = self.render_create_table(table, t, &inline_fks);
            let down = format!("DROP TABLE {}", self.qualified(table));
            let mig = self.make(
                &format!("create_table_{table}"),
                up,
                Some(down),
                MigrationFlags::default(),
                depends_on,
            );
            created_version.insert((*table).clone(), mig.version.clone());

            // Emit CREATE INDEX migrations for the new table's indexes, each
            // depending on the table's creation. The implicit PK index
            // (`<table>_pkey`) is created by the inline PRIMARY KEY clause, so
            // it is NOT emitted as a standalone CREATE INDEX.
            let table_version = mig.version.clone();
            out.push(mig);
            for idx in &t.indexes {
                if is_pk_index(table, &idx.name) {
                    continue;
                }
                out.push(self.render_create_index(
                    table,
                    idx,
                    vec![table_version.clone()],
                ));
            }
        }

        // --- Deferred FKs (ALTER TABLE ADD CONSTRAINT), after all CREATEs. ---
        for (table, fk) in &deferred_fks {
            let dep = created_version.get(table).cloned().into_iter();
            let target = fk_target_table(&fk.definition);
            let target_dep = target
                .as_ref()
                .and_then(|t| created_version.get(t))
                .cloned()
                .into_iter();
            let depends_on: Vec<MigrationId> = dep.chain(target_dep).collect();
            out.push(self.render_add_fk(table, fk, depends_on));
        }

        // --- Existing tables: column / index additions + destructive drops. ---
        for (table, dt) in &desired.tables {
            let Some(lt) = live.tables.get(table) else {
                continue; // newly created above
            };

            let live_cols: BTreeMap<&str, &ColumnSnapshot> =
                lt.columns.iter().map(|c| (c.name.as_str(), c)).collect();
            let desired_cols: BTreeMap<&str, &ColumnSnapshot> =
                dt.columns.iter().map(|c| (c.name.as_str(), c)).collect();

            // P3 rename (opt-in): the resolved renames for THIS table. A hinted
            // `from`→`to` is routed through the expand-contract sequence below and
            // its `from`/`to` columns are EXCLUDED from the plain drop/add diff so
            // they are not double-handled (drop the renamed-away column / add the
            // renamed-to column).
            let table_renames: Vec<&ResolvedRename> =
                resolved.iter().filter(|r| &r.table == table).collect();
            let renamed_from: std::collections::BTreeSet<&str> =
                table_renames.iter().map(|r| r.from.as_str()).collect();
            let renamed_to: std::collections::BTreeSet<&str> =
                table_renames.iter().map(|r| r.to.as_str()).collect();

            // Author the expand-contract rename sequences (E1..E3, C1, C2) and
            // carry them STRUCTURED — do NOT flatten into `out` (C1: that would
            // discard the BackfillSpec, so the real pre-existing-row mirror never
            // runs and the contract DROP destroys data). The caller drives each
            // expand through `run_expand` (which runs the real backfill) and
            // defers the contract to a subsequent deploy. The `from`/`to` columns
            // are excluded from the plain drop/add passes below so they are not
            // double-handled.
            let ec = ExpandContractAuthor::new(&self.project_schema, &self.owner_app);
            for r in &table_renames {
                let plan = ec.author(&OnlineIntent::RenameColumn {
                    table: table.clone(),
                    from: r.from.clone(),
                    to: r.to.clone(),
                    ty: ddl_type(&r.ty).to_string(),
                })?;
                renames.push(plan);
            }

            // ADD COLUMN: in desired, not in live (skip a rename's `to` column —
            // it is created by the rename's E1 ADD COLUMN, not a plain add).
            for c in &dt.columns {
                if renamed_to.contains(c.name.as_str()) {
                    continue;
                }
                match live_cols.get(c.name.as_str()) {
                    None => out.push(self.render_add_column(table, c)),
                    Some(lc) => {
                        // Same-name column whose attributes changed (P3):
                        // - type change → GATED ALTER COLUMN TYPE (no auto change);
                        // - SET NOT NULL (false→true) → GATED (lock-heavy, can
                        //   fail on existing NULLs);
                        // - DROP NOT NULL (true→false) → ungated additive.
                        if lc.data_type != c.data_type {
                            out.push(self.render_alter_column_type(table, c));
                        }
                        if lc.nullable != c.nullable {
                            out.push(self.render_alter_column_nullability(
                                table,
                                &c.name,
                                c.nullable,
                            ));
                        }
                    }
                }
            }

            // DROP COLUMN (P2): in live, not in desired → destructive, gated
            // (skip a rename's `from` column — it is dropped by the rename's gated
            // contract C2, not a plain drop).
            for c in &lt.columns {
                if renamed_from.contains(c.name.as_str()) {
                    continue;
                }
                if !desired_cols.contains_key(c.name.as_str()) {
                    out.push(self.render_drop_column(table, &c.name));
                }
            }

            // CREATE INDEX / DROP INDEX on an existing table.
            let live_idx: BTreeMap<&str, &IndexSnapshot> =
                lt.indexes.iter().map(|i| (i.name.as_str(), i)).collect();
            let desired_idx: BTreeMap<&str, &IndexSnapshot> =
                dt.indexes.iter().map(|i| (i.name.as_str(), i)).collect();
            for idx in &dt.indexes {
                if is_pk_index(table, &idx.name) {
                    continue; // implicit; created by the PRIMARY KEY clause
                }
                match live_idx.get(idx.name.as_str()) {
                    None => out.push(self.render_create_index(table, idx, Vec::new())),
                    Some(li) => {
                        // Same-name index on both sides: a flipped `unique` flag or
                        // a changed column set is an in-place redefinition
                        // (DROP+CREATE), deferred to a later phase. Surface it
                        // EXPLICITLY (5-idx) — never silently skip (the old loop
                        // only checked name presence, so a uniqueness flip emitted
                        // 0 migrations and left the wrong index in place).
                        if li.unique != idx.unique {
                            return Err(DeclarativeError::UnsupportedInV1(format!(
                                "index {}.{} uniqueness change {} → {}",
                                table, idx.name, li.unique, idx.unique
                            )));
                        }
                        if li.columns != idx.columns {
                            return Err(DeclarativeError::UnsupportedInV1(format!(
                                "index {}.{} column change {:?} → {:?}",
                                table, idx.name, li.columns, idx.columns
                            )));
                        }
                    }
                }
            }
            for idx in &lt.indexes {
                if is_pk_index(table, &idx.name) {
                    continue; // never drop the PK's implicit index
                }
                if !desired_idx.contains_key(idx.name.as_str()) {
                    out.push(self.render_drop_index(idx));
                }
            }

            // FK constraints on an existing table (5-fk): a same-name FK whose
            // BODY changed (e.g. the referenced target was re-pointed) is an
            // in-place constraint redefinition (DROP+ADD), deferred to a later
            // phase. Compare bodies and surface the divergence EXPLICITLY — the
            // old differ never looked at constraints here, so a changed FK target
            // was silently skipped (the FK definition spelling now matches live,
            // so this compare is meaningful, not phantom-drift noise).
            let live_fk: BTreeMap<&str, &ConstraintSnapshot> = lt
                .constraints
                .iter()
                .filter(|c| c.kind == "FOREIGN KEY")
                .map(|c| (c.name.as_str(), c))
                .collect();
            for c in &dt.constraints {
                if c.kind != "FOREIGN KEY" {
                    continue;
                }
                if let Some(lc) = live_fk.get(c.name.as_str()) {
                    if lc.definition != c.definition {
                        return Err(DeclarativeError::UnsupportedInV1(format!(
                            "foreign key {}.{} definition change {:?} → {:?}",
                            table, c.name, lc.definition, c.definition
                        )));
                    }
                }
            }
        }

        // --- DROP TABLE (P2): in live, not in desired → destructive, gated. ---
        for table in live.tables.keys() {
            if !desired.tables.contains_key(table) {
                out.push(self.render_drop_table(table));
            }
        }

        // Total order by UUIDv7 version (stable; the executor topo-sorts on
        // depends_on within it). Only the PLAIN migrations are ordered here; each
        // rename keeps its own internal expand→contract ordering and is applied
        // through the dedicated multi-deploy path, not interleaved with the plain
        // set.
        out.sort_by(|a, b| a.version.cmp(&b.version));
        Ok(DeclarativePlan {
            migrations: out,
            renames,
        })
    }

    /// Validate every desired table/column/index name + column `data_type` at the
    /// author boundary (mirrors `expand_contract`'s `validate_ident`/`validate_type`).
    fn validate_desired(desired: &SchemaSnapshot) -> Result<(), DeclarativeError> {
        for (table, t) in &desired.tables {
            validate_ident("table", table)?;
            for c in &t.columns {
                validate_ident("column", &c.name)?;
                validate_type(&c.data_type)?;
            }
            for i in &t.indexes {
                validate_ident("index", &i.name)?;
            }
            for c in &t.constraints {
                validate_ident("constraint", &c.name)?;
            }
        }
        Ok(())
    }

    /// Resolve + validate the [`RenameHint`]s against the desired/live snapshots.
    ///
    /// Each hint MUST match an actual drop+add pair: `from` present in the live
    /// table and ABSENT in desired (a column being dropped), `to` present in
    /// desired and ABSENT in live (a column being added), on the named table —
    /// and the two columns' `data_type`s MUST be equal. Any hint that fails is a
    /// hard error ([`DeclarativeError::RenameHintUnmatched`] /
    /// [`DeclarativeError::RenameHintTypeMismatch`]). The hint is the creator's
    /// signed statement of intent; silently dropping a hint would fall back to an
    /// unintended drop+add and lose the column's data.
    ///
    /// This is the ONLY place a rename is recognised — there is NO heuristic
    /// drop+add⇒rename inference anywhere in the differ.
    fn resolve_rename_hints(
        desired: &SchemaSnapshot,
        live: &SchemaSnapshot,
        hints: &[RenameHint],
    ) -> Result<Vec<ResolvedRename>, DeclarativeError> {
        // --- Cross-hint validation (H1/H2). ---------------------------------
        //
        // The per-hint resolution below validates each hint INDEPENDENTLY
        // (`from` live-only, `to` desired-only, type identity). That misses
        // collisions ACROSS hints on the same table, which produce colliding /
        // duplicated expand-contract sequences (a doubled `ADD COLUMN <to>`,
        // divergent dual-write triggers, a double `DROP COLUMN <from>`) or a
        // rename chain the single-snapshot resolution cannot express. Reject
        // those EXPLICITLY here, before any sequence is authored.
        //
        // Scoped PER TABLE: `from`/`to` are column names, unique only within a
        // table, so a `from` on table A and a `to` on table B sharing a spelling
        // is not a collision.
        {
            let mut froms: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
            let mut tos: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
            for h in hints {
                // H1: the multiset of `from`s per table must be duplicate-free.
                if !froms.entry(h.table.as_str()).or_default().insert(h.from.as_str()) {
                    return Err(DeclarativeError::DuplicateRenameHint {
                        table: h.table.clone(),
                        column: h.from.clone(),
                        side: "from",
                    });
                }
                // H1: …and so must the multiset of `to`s.
                if !tos.entry(h.table.as_str()).or_default().insert(h.to.as_str()) {
                    return Err(DeclarativeError::DuplicateRenameHint {
                        table: h.table.clone(),
                        column: h.to.clone(),
                        side: "to",
                    });
                }
            }
            // H2: no chain — a `to` on a table must not equal any OTHER hint's
            // `from` on the same table (e.g. `[a→b, b→c]`: `b` is both a target
            // and a source). A `from == to` hint trivially "matches" its own
            // `from`; that is a no-op handled by M1 below, not a chain, so skip it
            // here.
            for h in hints {
                if h.from == h.to {
                    continue;
                }
                if let Some(table_froms) = froms.get(h.table.as_str()) {
                    if table_froms.contains(h.to.as_str()) {
                        return Err(DeclarativeError::RenameHintChained {
                            table: h.table.clone(),
                            column: h.to.clone(),
                        });
                    }
                }
            }
        }

        let mut resolved = Vec::with_capacity(hints.len());
        for h in hints {
            // M1: a `from == to` hint is a no-op rename. Reject it with a PRECISE
            // error rather than the misleading `RenameHintUnmatched` it would
            // otherwise produce (an identical name is neither live-only nor
            // desired-only).
            if h.from == h.to {
                return Err(DeclarativeError::RenameHintNoop {
                    table: h.table.clone(),
                    column: h.from.clone(),
                });
            }
            // The named table must exist on BOTH sides (a rename is in-place on an
            // existing table). If it is missing on either side the hint cannot be
            // a drop+add pair → unmatched.
            let (Some(lt), Some(dt)) =
                (live.tables.get(&h.table), desired.tables.get(&h.table))
            else {
                return Err(DeclarativeError::RenameHintUnmatched {
                    table: h.table.clone(),
                    from: h.from.clone(),
                    to: h.to.clone(),
                });
            };
            let live_from = lt.columns.iter().find(|c| c.name == h.from);
            let desired_from = dt.columns.iter().any(|c| c.name == h.from);
            let desired_to = dt.columns.iter().find(|c| c.name == h.to);
            let live_to = lt.columns.iter().any(|c| c.name == h.to);

            // `from` must be live-only (present in live, absent in desired); `to`
            // must be desired-only (present in desired, absent in live). Anything
            // else is not a drop+add pair.
            let (Some(lf), Some(dtc)) = (live_from, desired_to) else {
                return Err(DeclarativeError::RenameHintUnmatched {
                    table: h.table.clone(),
                    from: h.from.clone(),
                    to: h.to.clone(),
                });
            };
            if desired_from || live_to {
                return Err(DeclarativeError::RenameHintUnmatched {
                    table: h.table.clone(),
                    from: h.from.clone(),
                    to: h.to.clone(),
                });
            }
            // Types must be identical — a pure online rename mirrors values across
            // the two columns and cannot also change the type.
            if lf.data_type != dtc.data_type {
                return Err(DeclarativeError::RenameHintTypeMismatch {
                    table: h.table.clone(),
                    from: h.from.clone(),
                    to: h.to.clone(),
                    from_type: lf.data_type.clone(),
                    to_type: dtc.data_type.clone(),
                });
            }
            resolved.push(ResolvedRename {
                table: h.table.clone(),
                from: h.from.clone(),
                to: h.to.clone(),
                ty: lf.data_type.clone(),
            });
        }
        Ok(resolved)
    }

    /// Render `CREATE TABLE <schema>.<table> (<cols…>, <pk>, <inline fks…>)`.
    fn render_create_table(
        &self,
        table: &str,
        t: &TableSnapshot,
        inline_fks: &[&ConstraintSnapshot],
    ) -> String {
        let mut parts: Vec<String> = Vec::new();
        for c in &t.columns {
            let null = if c.nullable { "" } else { " NOT NULL" };
            // `id` carries the inline PRIMARY KEY.
            let pk = if c.name == "id" { " PRIMARY KEY" } else { "" };
            parts.push(format!(
                "{} {}{}{}",
                quote_ident(&c.name),
                ddl_type(&c.data_type),
                pk,
                null
            ));
        }
        for fk in inline_fks {
            parts.push(self.fk_clause(fk));
        }
        format!(
            "CREATE TABLE {} ({})",
            self.qualified(table),
            parts.join(", ")
        )
    }

    /// Render a `CONSTRAINT … FOREIGN KEY (…) REFERENCES <schema>.<tgt> (id)`
    /// clause for inline CREATE TABLE use.
    fn fk_clause(&self, fk: &ConstraintSnapshot) -> String {
        let col = fk_local_column(&fk.definition).unwrap_or_default();
        let target = fk_target_table(&fk.definition).unwrap_or_default();
        format!(
            "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} (id)",
            quote_ident(&fk.name),
            quote_ident(&col),
            self.qualified(&target),
        )
    }

    /// Render a deferred `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY …`.
    fn render_add_fk(
        &self,
        table: &str,
        fk: &ConstraintSnapshot,
        depends_on: Vec<MigrationId>,
    ) -> Migration {
        let up = format!(
            "ALTER TABLE {} ADD {}",
            self.qualified(table),
            self.fk_clause(fk)
        );
        let down = format!(
            "ALTER TABLE {} DROP CONSTRAINT {}",
            self.qualified(table),
            quote_ident(&fk.name)
        );
        self.make(
            &format!("add_fk_{}_{}", table, fk.name),
            up,
            Some(down),
            MigrationFlags::default(),
            depends_on,
        )
    }

    /// Render an `ALTER TABLE … ADD COLUMN …` (additive).
    fn render_add_column(&self, table: &str, c: &ColumnSnapshot) -> Migration {
        let null = if c.nullable { "" } else { " NOT NULL" };
        let up = format!(
            "ALTER TABLE {} ADD COLUMN {} {}{}",
            self.qualified(table),
            quote_ident(&c.name),
            ddl_type(&c.data_type),
            null
        );
        let down = format!(
            "ALTER TABLE {} DROP COLUMN {}",
            self.qualified(table),
            quote_ident(&c.name)
        );
        self.make(
            &format!("add_column_{table}_{}", c.name),
            up,
            Some(down),
            MigrationFlags::default(),
            Vec::new(),
        )
    }

    /// Render a GATED `ALTER TABLE … ALTER COLUMN … TYPE …` (P3 type change).
    ///
    /// A type change is `destructive` + `requires_approval` in v1 — there is NO
    /// auto type-change. It can rewrite the whole table under `ACCESS EXCLUSIVE`
    /// and can be lossy (e.g. `text` → `integer` fails / truncates), so it flows
    /// through the gate exactly like a drop. The `USING <col>::<type>` cast is
    /// emitted so a compatible widening (e.g. `integer` → `double precision`)
    /// applies without a manual cast; an incompatible change still fails loudly at
    /// apply (never silently). Type spelling goes through [`validate_type`] (via
    /// `validate_desired`) + the guard.
    ///
    /// `down` is `None`: a type change is treated as irreversible (the reverse
    /// cast may not round-trip — `double precision` → `integer` loses the
    /// fraction), so there is no structural down. A re-diff after applying it is
    /// clean because live then matches desired.
    fn render_alter_column_type(&self, table: &str, c: &ColumnSnapshot) -> Migration {
        let ty = ddl_type(&c.data_type);
        let up = format!(
            "ALTER TABLE {} ALTER COLUMN {} TYPE {} USING {}::{}",
            self.qualified(table),
            quote_ident(&c.name),
            ty,
            quote_ident(&c.name),
            ty,
        );
        self.make(
            &format!("alter_column_type_{table}_{}", c.name),
            up,
            None,
            destructive_flags(),
            Vec::new(),
        )
    }

    /// Render an `ALTER TABLE … ALTER COLUMN … {SET|DROP} NOT NULL` (P3
    /// nullability change).
    ///
    /// - **`DROP NOT NULL`** (`nullable` true — relaxing required true→false) is
    ///   SAFE: it only removes a constraint, never rewrites data, so it is ungated
    ///   (default flags) and applies like an additive op. `down` re-tightens.
    /// - **`SET NOT NULL`** (`nullable` false — tightening required false→true) is
    ///   lock-heavy (full scan under `ACCESS EXCLUSIVE`) and FAILS if any existing
    ///   row is NULL, so it is GATED (`destructive` is false — no data is lost —
    ///   but `requires_approval` is true; a later analyzer-lint plan will suggest
    ///   the `CHECK … NOT VALID` → `VALIDATE` online path). `down` relaxes it.
    fn render_alter_column_nullability(
        &self,
        table: &str,
        col: &str,
        nullable: bool,
    ) -> Migration {
        let (verb, reverse, flags) = if nullable {
            // DROP NOT NULL — safe, ungated; down re-adds NOT NULL.
            ("DROP NOT NULL", "SET NOT NULL", MigrationFlags::default())
        } else {
            // SET NOT NULL — gated (lock-heavy, can fail on existing NULLs). Not
            // "destructive" (no data is lost) but requires_approval. down relaxes it.
            (
                "SET NOT NULL",
                "DROP NOT NULL",
                MigrationFlags {
                    requires_approval: true,
                    ..MigrationFlags::default()
                },
            )
        };
        let up = format!(
            "ALTER TABLE {} ALTER COLUMN {} {}",
            self.qualified(table),
            quote_ident(col),
            verb
        );
        let down = format!(
            "ALTER TABLE {} ALTER COLUMN {} {}",
            self.qualified(table),
            quote_ident(col),
            reverse
        );
        self.make(
            &format!("alter_column_null_{table}_{col}"),
            up,
            Some(down),
            flags,
            Vec::new(),
        )
    }

    /// Render a `CREATE [UNIQUE] INDEX IF NOT EXISTS …`.
    fn render_create_index(
        &self,
        table: &str,
        idx: &IndexSnapshot,
        depends_on: Vec<MigrationId>,
    ) -> Migration {
        // The snapshot carries the index's covered columns VERBATIM (1a), so we
        // emit them directly — no name-based reconstruction (which broke for
        // composite / custom-named indexes, recovering `a_b` from `events_a_b_idx`
        // and producing `column "a_b" does not exist`).
        let unique = if idx.unique { "UNIQUE " } else { "" };
        let col_list = idx
            .columns
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        let up = format!(
            "CREATE {unique}INDEX IF NOT EXISTS {} ON {} ({col_list})",
            quote_ident(&idx.name),
            self.qualified(table),
        );
        let down = format!("DROP INDEX IF EXISTS {}", self.qualified(&idx.name));
        self.make(
            &format!("create_index_{}", idx.name),
            up,
            Some(down),
            MigrationFlags::default(),
            depends_on,
        )
    }

    /// Render a destructive (gated) `DROP TABLE` — `destructive = true,
    /// requires_approval = true` so the gate refuses it without approval.
    fn render_drop_table(&self, table: &str) -> Migration {
        let up = format!("DROP TABLE {}", self.qualified(table));
        self.make(
            &format!("drop_table_{table}"),
            up,
            None,
            destructive_flags(),
            Vec::new(),
        )
    }

    /// Render a destructive (gated) `DROP COLUMN`.
    fn render_drop_column(&self, table: &str, col: &str) -> Migration {
        let up = format!(
            "ALTER TABLE {} DROP COLUMN {}",
            self.qualified(table),
            quote_ident(col)
        );
        self.make(
            &format!("drop_column_{table}_{col}"),
            up,
            None,
            destructive_flags(),
            Vec::new(),
        )
    }

    /// Render a `DROP INDEX`.
    ///
    /// Dropping a PLAIN (non-unique) index is **not data loss** — it is fully
    /// reversible by recreating the index — so it carries default (non-destructive)
    /// flags and flows through the engine gate ungated, like an additive op.
    ///
    /// Dropping a **UNIQUE** index, however, silently removes a data-integrity
    /// guarantee (#4): duplicate rows become possible afterwards and a later
    /// re-add fails on the now-dirty data. That is an integrity change the
    /// creator never approved, so it is classified `destructive +
    /// requires_approval` (gated, like DROP COLUMN). (The implicit PK index is
    /// never reached here — `diff` filters it via `is_pk_index`.)
    ///
    /// `down` recreates nothing because the declarative re-diff would re-add the
    /// index from the desired snapshot.
    fn render_drop_index(&self, idx: &IndexSnapshot) -> Migration {
        let up = format!("DROP INDEX {}", self.qualified(&idx.name));
        let flags = if idx.unique {
            destructive_flags()
        } else {
            MigrationFlags::default()
        };
        self.make(
            &format!("drop_index_{}", idx.name),
            up,
            None,
            flags,
            Vec::new(),
        )
    }
}

/// Flags for a destructive, gated drop: `destructive` + `requires_approval` so
/// the existing engine gate refuses it without [`crate::Approval::Approved`].
/// The drop is NEVER auto-applied.
fn destructive_flags() -> MigrationFlags {
    MigrationFlags {
        destructive: true,
        requires_approval: true,
        ..MigrationFlags::default()
    }
}

/// Map an `information_schema` data-type spelling back to the DDL spelling for
/// emission. `snapshot_schema` reports `timestamp with time zone`, but the DDL
/// is written `TIMESTAMPTZ` (both round-trip to the same `information_schema`
/// type). All others are spelled identically (lowercased is valid DDL).
fn ddl_type(data_type: &str) -> &str {
    match data_type {
        "timestamp with time zone" => "timestamptz",
        "double precision" => "double precision",
        other => other,
    }
}

/// Validate a bare SQL identifier at the author boundary: non-empty, starts with
/// a letter/underscore, only `[A-Za-z0-9_]`. Mirrors
/// [`crate::expand_contract`]'s `validate_ident`. Rejects schema-qualifiers
/// (`control.users`), quote-injection (`t"; DROP …`), whitespace, punctuation.
fn validate_ident(what: &str, value: &str) -> Result<(), DeclarativeError> {
    let mut chars = value.chars();
    let ok_first = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    let ok_rest = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if value.is_empty() || !ok_first || !ok_rest {
        return Err(DeclarativeError::Invalid(format!(
            "{what} is not a valid bare identifier: '{value}'"
        )));
    }
    Ok(())
}

/// Validate a Postgres type spelling spliced into DDL: no statement separator
/// `;`, balanced parentheses. Mirrors [`crate::expand_contract`]'s
/// `validate_type` (accepts `numeric(10,2)`, rejects `text; DROP …` and
/// `numeric(10`).
fn validate_type(ty: &str) -> Result<(), DeclarativeError> {
    if ty.contains(';') {
        return Err(DeclarativeError::Invalid(format!(
            "column type contains a statement separator ';': '{ty}'"
        )));
    }
    let mut depth: i32 = 0;
    for c in ty.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return Err(DeclarativeError::Invalid(format!(
                        "column type has unbalanced parentheses: '{ty}'"
                    )));
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(DeclarativeError::Invalid(format!(
            "column type has unbalanced parentheses: '{ty}'"
        )));
    }
    Ok(())
}

/// Topologically order new tables so an FK-target table is created before the
/// table that references it. A cycle (mutual refs) falls back to name order; the
/// deferred-FK path in [`DeclarativeAuthor::diff`] breaks the cycle at runtime.
fn topo_order_new_tables<'a>(
    desired: &'a SchemaSnapshot,
    new_tables: &[&'a String],
) -> Vec<&'a String> {
    use std::collections::BTreeSet;
    let new_set: BTreeSet<&str> = new_tables.iter().map(|s| s.as_str()).collect();
    let mut ordered: Vec<&String> = Vec::new();
    let mut placed: BTreeSet<&str> = BTreeSet::new();

    // Stable name order for determinism, then Kahn-style relaxation: repeatedly
    // place any unplaced table whose new-table FK targets are all already placed.
    let mut remaining: Vec<&String> = new_tables.to_vec();
    remaining.sort();
    loop {
        let mut progressed = false;
        let mut still: Vec<&String> = Vec::new();
        for t in &remaining {
            let table = &desired.tables[*t];
            let deps_satisfied = table.constraints.iter().all(|c| {
                if c.kind != "FOREIGN KEY" {
                    return true;
                }
                match fk_target_table(&c.definition) {
                    // Only NEW-table targets gate ordering; targets that already
                    // exist (live) or are self-refs don't block.
                    Some(tt) if new_set.contains(tt.as_str()) && tt != **t => {
                        placed.contains(tt.as_str())
                    }
                    _ => true,
                }
            });
            if deps_satisfied {
                ordered.push(t);
                placed.insert(t.as_str());
                progressed = true;
            } else {
                still.push(t);
            }
        }
        remaining = still;
        if remaining.is_empty() {
            break;
        }
        if !progressed {
            // Cycle: place the rest in name order; deferred FKs break it.
            for t in &remaining {
                ordered.push(t);
            }
            break;
        }
    }
    ordered
}

/// Extract the referenced (target) table from an FK definition of the form
/// `FOREIGN KEY (<col>) REFERENCES <schema>.<table>(id)` (the schema-qualified
/// `pg_get_constraintdef` spelling [`desired_snapshot`] now emits, matching live).
/// Returns the BARE table name (schema stripped) so it matches `SchemaSnapshot`
/// table keys.
fn fk_target_table(definition: &str) -> Option<String> {
    let after = definition.split("REFERENCES").nth(1)?.trim_start();
    // The target token is up to the first '(' or whitespace (e.g. `prj.authors`).
    let end = after
        .find(|c: char| c == '(' || c.is_whitespace())
        .unwrap_or(after.len());
    let qualified = after[..end].trim();
    // Strip a `<schema>.` prefix to get the bare table. The table part may be
    // quoted (`"My Table"`) even when the schema is not; handle a quoted tail.
    let bare = match qualified.rsplit_once('.') {
        Some((_schema, table)) => table,
        None => qualified,
    };
    let target = bare.trim().trim_matches('"');
    if target.is_empty() {
        None
    } else {
        Some(target.to_string())
    }
}

/// Extract the local column from an FK definition `FOREIGN KEY (<col>) …`.
fn fk_local_column(definition: &str) -> Option<String> {
    let open = definition.find('(')?;
    let close = definition[open + 1..].find(')')? + open + 1;
    let col = definition[open + 1..close].trim().trim_matches('"');
    if col.is_empty() {
        None
    } else {
        Some(col.to_string())
    }
}

