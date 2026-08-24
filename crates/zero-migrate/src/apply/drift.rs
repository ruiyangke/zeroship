//! Drift detection — **read-only**.
//!
//! Drift is any divergence between what the journal says happened and either
//! (a) the migration set the operator now ships, or (b) the live database
//! schema. This module **surfaces** drift; it NEVER emits DDL and NEVER mutates
//! anything.
//!
//! # What is here and what is next door
//!
//! This module is the DIALECT-BLIND half: the report types, the pure
//! [`diff_snapshots`], and [`compare_applied_to_set`] — the one tamper/orphan
//! comparison every backend runs over its own journal read. It issues no catalog
//! query and names no vendor's catalog.
//!
//! The catalog readers are the backends'. Each supplies its own `snapshot_schema_for`
//! and hands back the SAME [`SchemaSnapshot`]: `backend::postgres::drift_sql` reads
//! `pg_catalog` / `information_schema`, `backend::mysql::drift_sql` reads MySQL's
//! `information_schema`, `backend::sqlite::drift_sql` reads `sqlite_master` +
//! PRAGMAs. Every one of them runs as the admin/read connection, never as the
//! privileged `migrator` role, and binds identifiers so an injected schema/table
//! name cannot break the introspection queries.
//!
//! Two independent axes:
//!
//! - **B1 — checksum / tamper / orphan drift** ([`check_checksum_drift`](crate::apply::backend::MigrationBackend::check_checksum_drift)):
//! compares the journal's recorded checksum for each NET-applied version
//! against the checksum of the same version in the supplied set. A mismatch
//! means the migration SQL was edited after it applied, or the journal row was
//! tampered. A net-applied version with NO matching
//! migration in the supplied set is an **orphan** ([`OrphanJournal`]) — the
//! bundle is missing a migration the database already has. This is the exact
//! comparison the executor's apply flow does as its abort-on-drift pre-check;
//! [`apply`](crate::engine::MigrationEngine::apply) calls this function and aborts
//! if it returns any [`ChecksumDrift`], so the report and the gate share one
//! implementation.
//!
//! - **B2 — structural introspection** ([`snapshot_schema`](crate::apply::backend::MigrationBackend::snapshot_schema) +
//! [`diff_snapshots`]):
//! introspect the LIVE project schema into a deterministic [`SchemaSnapshot`]
//! and `diff` it against an **expected** snapshot the CALLER supplies. The
//! expected snapshot is owned by the control-plane / authoring layer (it holds
//! the declared/union schema, design); this module does NOT rebuild a schema
//! model by replaying DDL — that is the authoring layer's job. `diff_snapshots`
//! is a pure function returning a [`StructuralDrift`] report; it never returns
//! DDL.

use std::collections::BTreeMap;

use crate::model::ir::{
    IndexSortOrder, IndexStorageParams, SafeI64, SequenceOwnedBy, TriggerEvent,
};
use crate::model::snapshot::{
    canonical_index_sort_order, index_elements_canonically_eq, index_predicates_canonically_eq,
    ColumnCollationSnapshot, ColumnSnapshot, ConstraintSnapshot, ExtensionSnapshot,
    FunctionIdentity, FunctionKey, GeneratedKindSnapshot, IdDefaultSnapshot, IndexElementSnapshot,
    IndexSnapshot, PolicyIdentity, PolicyKey, RoleSnapshot, SchemaObjectSnapshot, SchemaSnapshot,
    SequenceSnapshot, TableSnapshot, TriggerIdentity, TriggerKey, VendorObjectIdentities,
};
use crate::render::value_format::{
    catalog_id_default, catalog_id_default_for_expected, catalog_text_id_default,
};

// ── The drift REPORT shapes moved down to the backend contract, whose drift
// queries return them, and `compare_applied_to_set` has now followed them: it is the
// checksum/tamper/orphan comparison EVERY backend runs over its own journal read, so
// it belongs below the vendors with the shapes it produces. The STRUCTURAL
// comparisons — `diff_snapshots` and every per-vendor catalog normalization below —
// stay here, because they read the engine's dialect-resolving value-format helpers.
// Re-exported so each `crate::apply::drift::…` path resolves unchanged.
pub use zero_migrate_backend::drift::{
    compare_applied_to_set, AlteredObject, ChecksumDrift, ChecksumDriftReport, DriftError,
    DriftReport, OrphanJournal, StructuralDrift,
};
// The one-partition declared-vs-live comparison followed the existence-guard decider
// down. Both this module's structural differ and that decider read it, and they must
// read the SAME one — a second, drifting copy in the probe is exactly how a guard and
// a drift report come to disagree about the same catalog — so it now sits beside the
// decider rather than one crate above it.
pub(crate) use zero_migrate_backend::drift::partition_divergences;

// ---------------------------------------------------------------------------
// B2 — structural introspection + pure diff
// ---------------------------------------------------------------------------

/// Diff an **expected** snapshot against the **actual** (live) snapshot — a PURE
/// function, no I/O, no DDL.
///
/// The expected snapshot is **supplied by the caller** — the control-plane /
/// authoring layer owns the declared/union schema and is the only
/// component that knows the intended shape. This function does NOT rebuild that
/// model by replaying the migration DDL; that is deliberately the authoring
/// layer's responsibility, and this seam keeps the two concerns separate.
///
/// Returns:
/// - `missing_objects` — present in `expected`, absent in `actual` (a declared
/// table/column/index/constraint the DB never got).
/// - `unexpected_objects` — present in `actual`, absent in `expected` (an
/// out-of-band object created outside the journal — scenario 35).
///
/// Object names are qualified for legibility: a table as `"users"`, a column as
/// `"users.email"`, an index as `"users index orders_email_idx"`, a constraint
/// as `"users constraint users_pkey"`. Output vectors are sorted + deterministic.
///
/// Same-name objects present on BOTH sides are compared ATTRIBUTE-BY-ATTRIBUTE
/// (#1): columns include physical type/nullability, identity/auto-increment,
/// semantic ID defaults, and enforced TypeID/ULID format; indexes include unique,
/// ordered keys, method, predicate, INCLUDE columns, and storage parameters;
/// constraints include kind plus a comparable definition. Foreign-key definitions
/// are canonical structured identities (target schema/table, ordered local and
/// referenced tuples, actions, match behavior, and deferrability), while ordinary
/// CHECK/PK/UNIQUE bodies use the catalog-author comparison spelling. Any
/// divergence becomes an [`AlteredObject`] — closing the out-of-band-`ALTER`
/// blind spot that pure name diffing left open.
///
/// # This comparison is dialect-blind, and partitions are where that shows
///
/// No dialect is supplied and none is inferred, so both sides are compared as
/// written. That is correct for every object class except one. SQLite and MySQL
/// collapse a partition child into its parent instead of creating a relation, so
/// their introspection reports no partition, while a folded `expected` records the
/// child unconditionally because the lower reads those bounds back to derive the
/// collapsed deletes. Comparing the two therefore reports `missing` for a
/// `partition <name>` that is not absent so much as never separately created, and
/// no schema change clears it.
///
/// A caller diffing a folded history against a live SQLite or MySQL database should
/// treat a missing `partition ...` as an artifact of that collapse rather than
/// drift. PostgreSQL is unaffected: it creates real partitions and both sides agree.
/// [`crate::render::fold`] carries the mechanism and why the fold cannot simply stop
/// recording the child.
#[must_use]
pub fn diff_snapshots(expected: &SchemaSnapshot, actual: &SchemaSnapshot) -> StructuralDrift {
    diff_snapshots_with_index_aliases(expected, actual, &BTreeMap::new())
}

/// [`diff_snapshots`], plus the derived-index-name provenance that lets a live index
/// pair with the OTHER derivation of its own name.
///
/// The data plane and the declarative author cap an overlong index name through
/// different schemes, so one index can be live under a name the expected snapshot
/// would never spell. Name-only diffing reports that index as BOTH missing and
/// unexpected even though the database is exactly right, which is the same false
/// drift the migration differ avoids by pairing on the same provenance. Pass
/// [`DesiredSchema::derived_index_aliases`](crate::render::declarative::DesiredSchema::derived_index_aliases)
/// - `table -> derived name -> the data plane's spelling` - to get the matching
/// report; [`diff_snapshots`] passes an empty map and stays name-only.
///
/// An alias is honoured only for a name the author DERIVED and only when the two
/// indexes' comparable shapes agree, so an author-supplied index rename still shows
/// up as one missing and one unexpected object.
#[must_use]
pub fn diff_snapshots_with_index_aliases(
    expected: &SchemaSnapshot,
    actual: &SchemaSnapshot,
    index_aliases: &BTreeMap<String, BTreeMap<String, String>>,
) -> StructuralDrift {
    let mut missing = Vec::new();
    let mut unexpected = Vec::new();
    let mut altered = Vec::new();
    // A table with no derived index names borrows this instead of allocating.
    let empty_index_aliases: BTreeMap<String, String> = BTreeMap::new();

    // Tables present in expected but not actual → missing (whole table + its
    // children fold into the single table name; the table is the unit of
    // missing-ness). Tables in actual but not expected → unexpected.
    for name in expected.tables.keys() {
        if !actual.tables.contains_key(name) {
            missing.push(name.clone());
        }
    }
    for name in actual.tables.keys() {
        if !expected.tables.contains_key(name) {
            unexpected.push(name.clone());
        }
    }
    for name in expected.partitions.keys() {
        if !actual.partitions.contains_key(name) {
            missing.push(format!("partition {name}"));
        }
    }
    for name in actual.partitions.keys() {
        if !expected.partitions.contains_key(name) {
            unexpected.push(format!("partition {name}"));
        }
    }
    for (name, exp_partition) in &expected.partitions {
        let Some(act_partition) = actual.partitions.get(name) else {
            continue;
        };
        for (field, expected, actual) in partition_divergences(exp_partition, act_partition) {
            altered.push(AlteredObject {
                table: name.clone(),
                object: format!("partition {name}"),
                field: field.to_string(),
                expected,
                actual,
            });
        }
    }
    // ROW-LEVEL SECURITY, per table. Skipping a table the ACTUAL side does not
    // mention is what keeps engines with no row-level security - and any table the
    // live snapshot did not reach - from reporting drift they cannot have.
    for (name, expected_rls) in &expected.table_rls {
        let Some(actual_rls) = actual.table_rls.get(name) else {
            continue;
        };
        if expected_rls != actual_rls {
            altered.push(AlteredObject {
                table: name.clone(),
                object: format!("table {name}"),
                field: "row_level_security".to_string(),
                expected: expected_rls.to_string(),
                actual: actual_rls.to_string(),
            });
        }
    }
    // FUNCTIONS, POLICIES AND TRIGGERS. What is compared, what is deliberately not,
    // and when the whole comparison is skipped: `comparable_vendor_objects`.
    if let Some((expected_vendor, actual_vendor)) = comparable_vendor_objects(expected, actual) {
        for (key, expected_function) in &expected_vendor.functions {
            match actual_vendor.functions.get(key) {
                Some(actual_function) => {
                    diff_function_attrs(key, expected_function, actual_function, &mut altered);
                }
                None => missing.push(function_label(key)),
            }
        }
        for key in actual_vendor.functions.keys() {
            if !expected_vendor.functions.contains_key(key) {
                unexpected.push(function_label(key));
            }
        }
        for (key, expected_policy) in &expected_vendor.policies {
            match actual_vendor.policies.get(key) {
                Some(actual_policy) => {
                    diff_policy_attrs(key, expected_policy, actual_policy, &mut altered);
                }
                None => missing.push(policy_label(key)),
            }
        }
        for key in actual_vendor.policies.keys() {
            if !expected_vendor.policies.contains_key(key) {
                unexpected.push(policy_label(key));
            }
        }
        for (key, expected_trigger) in &expected_vendor.triggers {
            match actual_vendor.triggers.get(key) {
                Some(actual_trigger) => {
                    diff_trigger_attrs(key, expected_trigger, actual_trigger, &mut altered);
                }
                None => missing.push(trigger_label(key)),
            }
        }
        for key in actual_vendor.triggers.keys() {
            if !expected_vendor.triggers.contains_key(key) {
                unexpected.push(trigger_label(key));
            }
        }
    }
    for name in expected.views.keys() {
        if !actual.views.contains_key(name) {
            missing.push(format!("view {name}"));
        }
    }
    for name in actual.views.keys() {
        if !expected.views.contains_key(name) {
            unexpected.push(format!("view {name}"));
        }
    }
    for (name, exp_ty) in &expected.named_types {
        if !actual.named_types.contains_key(name) {
            missing.push(format!("{} {name}", exp_ty.kind));
        }
    }
    for (name, act_ty) in &actual.named_types {
        if !expected.named_types.contains_key(name) {
            unexpected.push(format!("{} {name}", act_ty.kind));
        }
    }
    for (name, exp_ty) in &expected.named_types {
        let Some(act_ty) = actual.named_types.get(name) else {
            continue;
        };
        if exp_ty.kind != act_ty.kind {
            altered.push(AlteredObject {
                table: name.clone(),
                object: format!("type {name}"),
                field: "kind".to_string(),
                expected: exp_ty.kind.clone(),
                actual: act_ty.kind.clone(),
            });
        }
        if exp_ty.comment != act_ty.comment {
            altered.push(AlteredObject {
                table: name.clone(),
                object: format!("type {name}"),
                field: "comment".to_string(),
                expected: exp_ty.comment.clone().unwrap_or_default(),
                actual: act_ty.comment.clone().unwrap_or_default(),
            });
        }
    }
    for name in expected.sequences.keys() {
        if !actual.sequences.contains_key(name) {
            missing.push(format!("sequence {name}"));
        }
    }
    for name in actual.sequences.keys() {
        if !expected.sequences.contains_key(name) {
            unexpected.push(format!("sequence {name}"));
        }
    }
    for (name, exp_seq) in &expected.sequences {
        let Some(act_seq) = actual.sequences.get(name) else {
            continue;
        };
        diff_sequence_attrs(name, exp_seq, act_seq, &mut altered);
    }
    for name in expected.roles.keys() {
        if !actual.roles.contains_key(name) {
            missing.push(format!("role {name}"));
        }
    }
    for (name, exp_role) in &expected.roles {
        let Some(act_role) = actual.roles.get(name) else {
            continue;
        };
        diff_role_attrs(name, exp_role, act_role, &mut altered);
    }
    for name in expected.schemas.keys() {
        if !actual.schemas.contains_key(name) {
            missing.push(format!("schema {name}"));
        }
    }
    for (name, exp_schema) in &expected.schemas {
        let Some(act_schema) = actual.schemas.get(name) else {
            continue;
        };
        diff_schema_attrs(name, exp_schema, act_schema, &mut altered);
    }
    for name in expected.extensions.keys() {
        if !actual.extensions.contains_key(name) {
            missing.push(format!("extension {name}"));
        }
    }
    for (name, exp_extension) in &expected.extensions {
        let Some(act_extension) = actual.extensions.get(name) else {
            continue;
        };
        diff_extension_attrs(name, exp_extension, act_extension, &mut altered);
    }
    for (name, exp_v) in &expected.views {
        let Some(act_v) = actual.views.get(name) else {
            continue;
        };
        if exp_v.materialized != act_v.materialized {
            altered.push(AlteredObject {
                table: name.clone(),
                object: format!("view {name}"),
                field: "materialized".to_string(),
                expected: exp_v.materialized.to_string(),
                actual: act_v.materialized.to_string(),
            });
        }
        if exp_v.comment != act_v.comment {
            altered.push(AlteredObject {
                table: name.clone(),
                object: format!("view {name}"),
                field: "comment".to_string(),
                expected: exp_v.comment.clone().unwrap_or_default(),
                actual: act_v.comment.clone().unwrap_or_default(),
            });
        }
        // The body, and ONLY when both sides carry one. See
        // [`resolve_view_bodies`] for why that condition is the whole design: the
        // folded side carries a typed `authored_query` and no text, the
        // introspected side carries text and no typed query, so nothing here can
        // compare them. `resolve_view_bodies` is what puts a body on both sides,
        // and it puts the SERVER's re-print of the same moment on both - so this
        // stays a plain equality with no normaliser of its own to over-collapse.
        //
        // `comparable_body` rather than `definition`, and the difference is
        // load-bearing: a catalog-seeded fold CLONES an introspected `definition`
        // onto the expected side, and PostgreSQL follows a table rename into a
        // dependent view's stored body with no statement naming the view - so that
        // clone reports drift on a view nobody touched. Only the field
        // `resolve_view_bodies` writes carries two bodies re-printed together.
        //
        // A caller that has not run it leaves both sides `None` and declines here,
        // which is exactly the behaviour this comparison replaced.
        if let (Some(exp_body), Some(act_body)) = (&exp_v.comparable_body, &act_v.comparable_body) {
            if exp_body != act_body {
                altered.push(AlteredObject {
                    table: name.clone(),
                    object: format!("view {name}"),
                    field: "body".to_string(),
                    expected: exp_body.clone(),
                    actual: act_body.clone(),
                });
            }
        }
    }

    // For tables present on BOTH sides, diff their columns / indexes / constraints
    // by name (added/removed → missing/unexpected) AND, for same-name children,
    // by attribute (→ altered).
    for (name, exp_t) in &expected.tables {
        let Some(act_t) = actual.tables.get(name) else {
            continue;
        };
        diff_named(
            name,
            "",
            &exp_t
                .columns
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>(),
            &act_t
                .columns
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>(),
            &mut missing,
            &mut unexpected,
        );
        diff_indexes(
            name,
            &exp_t.indexes,
            &act_t.indexes,
            index_aliases.get(name).unwrap_or(&empty_index_aliases),
            &mut missing,
            &mut unexpected,
        );
        diff_named(
            name,
            "constraint ",
            &exp_t
                .constraints
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>(),
            &act_t
                .constraints
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>(),
            &mut missing,
            &mut unexpected,
        );

        // Attribute diff for same-name objects on both sides.
        diff_attrs(name, exp_t, act_t, &mut altered);
    }

    missing.sort_unstable();
    unexpected.sort_unstable();
    altered.sort_unstable_by(|a, b| {
        (&a.table, &a.object, &a.field).cmp(&(&b.table, &b.object, &b.field))
    });
    StructuralDrift {
        missing_objects: missing,
        unexpected_objects: unexpected,
        altered_objects: altered,
    }
}

fn format_sequence_bound(value: Option<SafeI64>) -> String {
    value.map_or_else(|| "default".to_string(), |n| n.to_string())
}

fn format_sequence_owned_by(value: Option<&SequenceOwnedBy>) -> String {
    value.map_or_else(String::new, |owned| {
        format!("{}.{}", owned.table, owned.column)
    })
}

// The `nextval` spelling's parse moved down to the backend contract, beside the
// sequence-bound normalizers that already carry PostgreSQL's sequence semantics as
// shared vocabulary. The PostgreSQL introspector reaches it to recover an ID default
// out of `pg_get_expr`, and it cannot reach into the engine. Re-exported so
// `crate::apply::drift::parse_nextval_sequence_ref` resolves unchanged.
pub(crate) use zero_migrate_backend::snapshot::parse_nextval_sequence_ref;

/// Canonical rendered form of a `nextval` default, or `None` when the expression
/// is not one. Backend-identity-free on purpose: the sequence identity is the whole key.
fn comparable_nextval_default(expr: Option<&str>) -> Option<String> {
    let sequence = parse_nextval_sequence_ref(expr?)?;
    Some(crate::render::declarative::nextval_default_expr(&sequence))
}

/// One column's ORDINARY `DEFAULT` reduced to a semantic key when comparing the
/// two sides' spellings is meaningful, and `None` when it is not.
///
/// The FOURTH member of the family [`constraint_definition_is_comparable`],
/// [`index_expression_bodies_are_comparable`], [`comparable_vendor_objects`],
/// [`comparable_generated_column`] and [`comparable_function_body`] belong to,
/// answering the same question - is this
/// text worth comparing across an offline render and a live catalog read? - for the
/// default on a column that
/// carries no ID facet at all. Those columns never populate
/// [`ColumnSnapshot::id_default`], so the raw SQL text in
/// [`ColumnSnapshot::default`] is the only evidence either side holds, and until
/// this existed nothing compared it: an out-of-band `ALTER COLUMN ... SET DEFAULT`
/// changed what every silent write stores and no drift line said so.
///
/// WHAT PostgreSQL NORMALISES. `pg_get_expr` deparses from the parse tree rather
/// than replaying the authored text. Measured on PostgreSQL 18.4:
///
/// | authored             | read back from the catalog |
/// |----------------------|----------------------------|
/// | `DEFAULT 'active'`   | `'active'::text`           |
/// | `DEFAULT '{}'`       | `'{}'::jsonb`              |
/// | `DEFAULT current_user` | `CURRENT_USER`           |
///
/// So a byte compare of authored text against catalog text reports drift on every
/// text and JSON default that exists, on every comparison, on a schema nobody has
/// touched. What this returns instead is the SAME semantic key the ID-default
/// surface already uses: [`catalog_id_default`] strips the cast parse analysis
/// inferred, canonicalises quoting, decimal spelling and boolean form, and lands
/// `'active'` and `'active'::text` on one fingerprint. Both sides go through it, so
/// the normalisation is applied to the authored render and the catalog read alike -
/// which is sound precisely because `pg_get_expr` is idempotent.
///
/// WHEN THE COMPARISON IS SKIPPED, and why each skip is not an oversight:
///
/// * `None` vendor. [`introspected_table_vendor`] recognises a live catalog read
///   by the evidence only introspection leaves; a snapshot without it is not one,
///   and the literal fingerprint is dialect-sensitive (a MySQL `COLUMN_DEFAULT`
///   arrives with its SQL quotes already stripped). A `nextval` still compares,
///   because its key is a sequence identity rather than a spelling.
/// * Either side reduces to [`IdDefaultSnapshot::Expression`]. That arm is a
///   fingerprint of DEPARSED TEXT, and the two sides do not produce text the same
///   way: the offline renderer quotes every identifier and knows no column types,
///   so it cannot reproduce PostgreSQL's inferred casts or keyword rewriting. This
///   is the same refusal [`constraint_definition_is_comparable`] makes for a CHECK
///   body and [`index_expression_bodies_are_comparable`] makes for an index
///   expression key, for the same reason.
///
/// WHAT THIS GIVES UP: an expression default rewritten out of band is not reported.
/// `DEFAULT now()` swapped for `DEFAULT clock_timestamp()`, or a `DEFAULT '{}'`
/// swapped for `DEFAULT '[]'`, leaves this differ silent - and so does an
/// expression default ADDED to a column that had none, because the added side
/// reduces to `Expression` even though the absent side is unambiguous. That is a
/// real loss, the same one the CHECK, index-expression and vendor-object
/// exemptions already take, and recovering it needs the same treatment foreign
/// keys get: parse the catalog text back to the closed AST and compare
/// structurally rather than comparing spellings.
/// The two sides' GENERATED-column facet reduced to a comparable key, and `None`
/// when comparing them is not meaningful.
///
/// The FIFTH member of the family [`constraint_definition_is_comparable`],
/// [`index_expression_bodies_are_comparable`], [`comparable_vendor_objects`],
/// [`comparable_column_default`] and [`comparable_function_body`] belong to,
/// answering the same question - is this
/// worth comparing across an offline render and a live catalog read? - for a column
/// the engine computes rather than the application writing it. Until this existed
/// nothing compared any part of it: an out-of-band
/// `ALTER COLUMN ... DROP EXPRESSION` turned a computed column into an ordinary
/// writable one, left its name, type and nullability untouched, and no drift line
/// said so.
///
/// WHAT IS COMPARED, and why it is immune to the deparse problem. Only
/// [`ColumnSnapshot::generated_kind`] - `pg_attribute.attgenerated`, ONE CHAR per
/// column. PostgreSQL stores it structurally, exactly as it stores a `polcmd` code
/// or a `tgtype` bit set, so no renderer is involved on either side and there is
/// nothing for `pg_get_expr` to rewrite. That is the same property that makes an
/// [`IndexElementSnapshot::Column`] comparable where an `Expr` key is not.
///
/// WHEN THE COMPARISON IS SKIPPED: `None` on either side. MySQL and SQLite
/// introspection do not populate the field, so those engines are never accused of
/// having dropped a generated column they never modeled. This is the both-sides rule
/// [`comparable_vendor_objects`] applies, at the granularity this facet needs.
///
/// WHAT THIS GIVES UP: the EXPRESSION. `ALTER COLUMN c SET EXPRESSION AS (src + 2)`
/// keeps `attgenerated = 's'`, so both sides agree here and the rewrite is not
/// reported. That refusal is measured rather than assumed, and the measurement is
/// NOT the injected cast the sibling predicates cite - it is the column RENAME.
/// [`GeneratedColumnSnapshot::expr`](zero_migrate_backend::snapshot::GeneratedColumnSnapshot::expr)
/// is RENDERED TEXT, and `fold_ops`'s
/// `Op::RenameColumn` arm cannot replay a rename over it: substituting the name
/// inside rendered SQL would rewrite the string literal in a real generated column
/// such as `(note || 'qty_on_hand'::text)`, which is exactly the false positive
/// [`IndexSnapshot::expr_cascade_columns`] exists to avoid. Measured on PostgreSQL
/// 18.4, after `RENAME COLUMN qty_on_hand TO amount_on_hand`:
///
/// | side | generated expression       |
/// |------|----------------------------|
/// | fold | `("qty_on_hand" + 1)`      |
/// | live | `(amount_on_hand + 1)`     |
///
/// Those are two different COLUMN NAMES, not two spellings of one thing, so the
/// reduce-both-sides-through-one-key technique that closed the ordinary-default gap
/// cannot close this: normalising quoting and casts lands them on
/// `qty_on_hand|literal:1` against `amount_on_hand|literal:1`, still different, and
/// no apply could ever clear it. Recovering the expression needs the treatment
/// foreign keys get - keep the closed AST rather than its rendering, and compare
/// structurally.
fn comparable_generated_column(
    expected: &ColumnSnapshot,
    actual: &ColumnSnapshot,
) -> Option<(GeneratedKindSnapshot, GeneratedKindSnapshot)> {
    Some((expected.generated_kind?, actual.generated_kind?))
}

fn format_generated_kind(kind: GeneratedKindSnapshot) -> &'static str {
    match kind {
        GeneratedKindSnapshot::NotGenerated => "",
        GeneratedKindSnapshot::Stored => "stored",
        GeneratedKindSnapshot::Virtual => "virtual",
    }
}

fn comparable_column_default(
    raw: Option<&str>,
    vendor: Option<&zero_migrate_backend::registry::BackendVendor>,
    expression_default: Option<bool>,
) -> Option<IdDefaultSnapshot> {
    let Some(raw) = raw else {
        return Some(IdDefaultSnapshot::Absent);
    };
    if let Some(sequence) = comparable_nextval_default(Some(raw)) {
        return Some(IdDefaultSnapshot::Nextval(sequence));
    }
    let vendor = vendor?;
    let dialect = &vendor.descriptor.id;
    // MySQL reports `COLUMN_DEFAULT` in its COERCED character form, without SQL
    // quotes, so the two sides only meet once the authored key is projected into
    // that same storage spelling. `expression_default` is the authoritative
    // literal-vs-expression bit, carried by whichever backend declares its catalog
    // marker authoritative rather than by a named dialect; the authored side has
    // none and does not need one, because its text still carries its quotes.
    let key = if vendor
        .value_format
        .catalog_default_marker_is_authoritative()
    {
        catalog_text_id_default(Some(raw), dialect, expression_default)
    } else {
        catalog_id_default(Some(raw), dialect, None)
    };
    (!matches!(key, IdDefaultSnapshot::Expression(_))).then_some(key)
}

fn diff_sequence_attrs(
    name: &str,
    expected: &SequenceSnapshot,
    actual: &SequenceSnapshot,
    altered: &mut Vec<AlteredObject>,
) {
    let mut push = |field: &str, expected: String, actual: String| {
        if expected != actual {
            altered.push(AlteredObject {
                table: name.to_string(),
                object: format!("sequence {name}"),
                field: field.to_string(),
                expected,
                actual,
            });
        }
    };

    push(
        "as",
        expected.as_type.to_string(),
        actual.as_type.to_string(),
    );
    push(
        "increment",
        expected.increment.to_string(),
        actual.increment.to_string(),
    );
    push(
        "min_value",
        format_sequence_bound(expected.min_value),
        format_sequence_bound(actual.min_value),
    );
    push(
        "max_value",
        format_sequence_bound(expected.max_value),
        format_sequence_bound(actual.max_value),
    );
    push(
        "start",
        expected.start.to_string(),
        actual.start.to_string(),
    );
    push(
        "cache",
        expected.cache.to_string(),
        actual.cache.to_string(),
    );
    push(
        "cycle",
        expected.cycle.to_string(),
        actual.cycle.to_string(),
    );
    push(
        "owned_by",
        format_sequence_owned_by(expected.owned_by.as_ref()),
        format_sequence_owned_by(actual.owned_by.as_ref()),
    );
    push(
        "comment",
        expected.comment.clone().unwrap_or_default(),
        actual.comment.clone().unwrap_or_default(),
    );
}

fn push_vendor_attr(
    altered: &mut Vec<AlteredObject>,
    name: &str,
    object: String,
    field: &str,
    expected: String,
    actual: String,
) {
    if expected != actual {
        altered.push(AlteredObject {
            table: name.to_string(),
            object,
            field: field.to_string(),
            expected,
            actual,
        });
    }
}

fn diff_role_attrs(
    name: &str,
    expected: &RoleSnapshot,
    actual: &RoleSnapshot,
    altered: &mut Vec<AlteredObject>,
) {
    let object = format!("role {name}");
    push_vendor_attr(
        altered,
        name,
        object.clone(),
        "login",
        expected.login.to_string(),
        actual.login.to_string(),
    );
    push_vendor_attr(
        altered,
        name,
        object.clone(),
        "superuser",
        expected.superuser.to_string(),
        actual.superuser.to_string(),
    );
    push_vendor_attr(
        altered,
        name,
        object.clone(),
        "create_db",
        expected.create_db.to_string(),
        actual.create_db.to_string(),
    );
    push_vendor_attr(
        altered,
        name,
        object.clone(),
        "create_role",
        expected.create_role.to_string(),
        actual.create_role.to_string(),
    );
    push_vendor_attr(
        altered,
        name,
        object.clone(),
        "bypass_rls",
        expected.bypass_rls.to_string(),
        actual.bypass_rls.to_string(),
    );
    push_vendor_attr(
        altered,
        name,
        object.clone(),
        "inherit",
        expected.inherit.to_string(),
        actual.inherit.to_string(),
    );
    push_vendor_attr(
        altered,
        name,
        object.clone(),
        "replication",
        expected.replication.to_string(),
        actual.replication.to_string(),
    );
    push_vendor_attr(
        altered,
        name,
        object,
        "member_of",
        expected.member_of.join(","),
        actual.member_of.join(","),
    );
}

fn diff_schema_attrs(
    name: &str,
    expected: &SchemaObjectSnapshot,
    actual: &SchemaObjectSnapshot,
    altered: &mut Vec<AlteredObject>,
) {
    if let Some(owner) = expected.owner.as_ref() {
        push_vendor_attr(
            altered,
            name,
            format!("schema {name}"),
            "owner",
            owner.clone(),
            actual.owner.clone().unwrap_or_default(),
        );
    }
}

fn diff_extension_attrs(
    name: &str,
    expected: &ExtensionSnapshot,
    actual: &ExtensionSnapshot,
    altered: &mut Vec<AlteredObject>,
) {
    if let Some(schema) = expected.schema.as_ref() {
        push_vendor_attr(
            altered,
            name,
            format!("extension {name}"),
            "schema",
            schema.clone(),
            actual.schema.clone().unwrap_or_default(),
        );
    }
}

fn format_case_sensitive(case_sensitive: Option<bool>) -> &'static str {
    match case_sensitive {
        Some(false) => "false",
        _ => "",
    }
}

fn format_collation(collation: Option<&ColumnCollationSnapshot>) -> String {
    collation.map_or_else(String::new, ColumnCollationSnapshot::display_name)
}

/// The `identity` drift attribute's operator-facing spelling.
///
/// The two row-identifier-alias arms used to read "sqlite rowid" and "sqlite
/// autoincrement" -- core naming a vendor in a report line it prints for whichever
/// backend produced the snapshot. They name the CONTRACT now: an alias with no
/// identity is a plain row-identifier alias, and one carrying a by-default identity
/// is that alias plus an always-increasing allocator.
fn format_identity(column: &ColumnSnapshot) -> &'static str {
    match (column.rowid_alias, column.identity) {
        (true, Some(identity)) if !identity.always => "rowid alias, auto increment",
        (true, None) => "rowid alias",
        (_, Some(identity)) if identity.always => "always",
        (_, Some(_)) => "by default / auto increment",
        _ => "",
    }
}

fn format_value_format(value_format: Option<&crate::model::ir::ValueFormat>) -> String {
    match value_format {
        None => String::new(),
        Some(crate::model::ir::ValueFormat::TypeId { prefix }) => {
            format!("typeId({prefix})")
        }
        Some(crate::model::ir::ValueFormat::Ulid) => "ulid".to_string(),
    }
}

fn format_id_default(default: Option<&crate::model::snapshot::IdDefaultSnapshot>) -> String {
    use crate::model::snapshot::IdDefaultSnapshot;
    match default {
        None => String::new(),
        Some(IdDefaultSnapshot::Absent) => "absent".to_string(),
        Some(IdDefaultSnapshot::UuidV4) => "uuidV4".to_string(),
        Some(IdDefaultSnapshot::UuidV7) => "uuidV7".to_string(),
        Some(IdDefaultSnapshot::Nextval(sequence)) => sequence.clone(),
        Some(IdDefaultSnapshot::Literal(value)) => value.clone(),
        Some(IdDefaultSnapshot::UuidLiteral(value)) => value.clone(),
        Some(IdDefaultSnapshot::Expression(expression)) => expression.clone(),
    }
}

fn introspected_table_vendor(
    table: &TableSnapshot,
) -> Option<&'static zero_migrate_backend::registry::BackendVendor> {
    crate::render::backends::VENDORS
        .as_slice()
        .iter()
        .copied()
        .filter_map(|vendor| {
            vendor
                .catalog_fold
                .snapshot_provenance_strength(table)
                .map(|strength| (strength, vendor))
        })
        .max_by_key(|(strength, _)| *strength)
        .map(|(_, vendor)| vendor)
}

fn column_data_types_eq(expected: &ColumnSnapshot, actual: &ColumnSnapshot) -> bool {
    // A backend's own physical contract, when BOTH sides carry that backend's, is
    // the authority. The portable `data_type` cannot be: `canonical_type`
    // NORMALIZES, so a backend that folds every `varchar(n)` to one spelling makes a
    // declared 255 and a live 64 the same string here.
    //
    // Which contracts exist and what makes two of them equal is the VENDOR's
    // question, asked through the carrier and answered in its crate; core neither
    // names a contract type nor matches on one. Both sides, not either - a snapshot
    // from another dialect carries no leg, and so does one written before that
    // backend's contract existed - and `physical_identity` returns `None` unless
    // some dialect recorded on both, which is that rule made structural.
    if let Some(verdict) = expected.vendor.physical_identity(&actual.vendor) {
        return verdict;
    }
    if expected.data_type == actual.data_type {
        return true;
    }
    if !(expected.rowid_alias && actual.rowid_alias) {
        return false;
    }

    // A row-identifier alias fixes its column's PHYSICAL type on any backend that
    // has one -- that is what makes it an alias rather than an ordinary key -- so the
    // authored integer width the portable `data_type` records (bigint, smallint) is
    // not what the column physically is. Two columns BOTH proven to be that alias are
    // therefore the same physical column across the whole integer family.
    //
    // Confined to that pair on purpose: exact type drift is preserved everywhere
    // else, and only a producer that sets `rowid_alias` on both sides reaches here.
    let integer_family = |data_type: &str| {
        matches!(
            data_type.trim().to_ascii_lowercase().as_str(),
            "smallint" | "integer" | "bigint" | "int" | "int2" | "int4" | "int8" | "boolean"
        )
    };
    integer_family(&expected.data_type) && integer_family(&actual.data_type)
}

/// The two sides of a `data_type` drift line, spelled so that they NAME the difference
/// [`column_data_types_eq`] found.
///
/// The portable `data_type` cannot do that job on a backend whose `canonical_type`
/// normalizes, and it fails in two different directions. `fold_ops` emits one
/// dialect's `information_schema` spelling regardless of target while the catalog side
/// is folded through the target's own normalizer, so a `decimal(12, 2)` widened to
/// `decimal(30, 10)` reported `expected: "numeric", actual: "decimal"` - one type
/// spelled two ways, naming nothing a reader can act on. And when the two spellings
/// COINCIDE - a live `TEXT` narrowed to `VARCHAR(64)` is `"text"` on both sides -
/// `diff_attrs`'s `push` dropped the entry entirely, because its job is to skip fields
/// whose two sides are equal. That is the case the physical contract exists for, so
/// the report was blind exactly where the comparator was not.
///
/// The contract is what the comparator compared, so the contract is what the report
/// prints - in the OWNING VENDOR's spelling, produced by the vendor. Core does not and
/// must not spell a vendor's type text: the two sides come back already written. A
/// dialect that recorded no leg, or recorded on one side only, leaves the portable
/// spelling it always had, for the reason [`column_data_types_eq`] gives.
fn column_data_type_report(expected: &ColumnSnapshot, actual: &ColumnSnapshot) -> (String, String) {
    expected
        .vendor
        .type_drift_report(&actual.vendor)
        .unwrap_or_else(|| (expected.data_type.clone(), actual.data_type.clone()))
}

/// Compare the attributes of same-name children (columns/indexes/constraints
/// present on BOTH sides of one table), pushing an [`AlteredObject`] per diverging
/// field. Added/removed children are NOT this function's concern (they go to the
/// missing/unexpected buckets via [`diff_named`]); only matched names are compared.
fn diff_attrs(
    table: &str,
    exp_t: &TableSnapshot,
    act_t: &TableSnapshot,
    altered: &mut Vec<AlteredObject>,
) {
    let mut push = |object: &str, field: &str, expected: &str, actual: &str| {
        if expected != actual {
            altered.push(AlteredObject {
                table: table.to_string(),
                object: object.to_string(),
                field: field.to_string(),
                expected: expected.to_string(),
                actual: actual.to_string(),
            });
        }
    };
    let format_index_elements = |elements: &[IndexElementSnapshot]| {
        elements
            .iter()
            .map(|element| match element {
                IndexElementSnapshot::Column { name, order, .. } => {
                    match canonical_index_sort_order(*order) {
                        Some(IndexSortOrder::Desc) => format!("col:{name} desc"),
                        Some(IndexSortOrder::Asc) | None => format!("col:{name}"),
                    }
                }
                IndexElementSnapshot::Expr(expr) => format!("expr:{expr}"),
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let format_index_storage_params = |params: Option<&IndexStorageParams>| {
        let Some(params) = params else {
            return String::new();
        };
        let mut entries = Vec::new();
        if let Some(pages_per_range) = params.pages_per_range {
            entries.push(format!("pages_per_range={pages_per_range}"));
        }
        if let Some(fillfactor) = params.fillfactor {
            entries.push(format!("fillfactor={fillfactor}"));
        }
        entries.join(",")
    };

    push(
        "table",
        "comment",
        exp_t.comment.as_deref().unwrap_or(""),
        act_t.comment.as_deref().unwrap_or(""),
    );

    // Columns: physical type/nullability, identity/default generation, enforced
    // value format, recoverable text collation, and catalog comment.
    let act_cols: BTreeMap<&str, &ColumnSnapshot> =
        act_t.columns.iter().map(|c| (c.name.as_str(), c)).collect();
    let actual_vendor = introspected_table_vendor(act_t);
    let actual_dialect = actual_vendor.map(|vendor| &vendor.descriptor.id);
    for ec in &exp_t.columns {
        if let Some(ac) = act_cols.get(ec.name.as_str()) {
            let obj = format!("column {}", ec.name);
            if !column_data_types_eq(ec, ac) {
                // NOT `&ec.data_type` / `&ac.data_type`: on MySQL those two strings can
                // be equal for columns the comparator just called different, and `push`
                // drops an entry whose sides are equal. `column_data_type_report` says
                // why, and prints the contract that established the difference.
                let (expected_type, actual_type) = column_data_type_report(ec, ac);
                push(&obj, "data_type", &expected_type, &actual_type);
            }
            push(
                &obj,
                "nullable",
                &ec.nullable.to_string(),
                &ac.nullable.to_string(),
            );
            push(&obj, "identity", format_identity(ec), format_identity(ac));
            // Whether the engine computes this column at all. `comparable_generated_column`
            // documents why the storage KIND compares and the expression does not.
            if let Some((expected_generated, actual_generated)) =
                comparable_generated_column(ec, ac)
            {
                push(
                    &obj,
                    "generated",
                    format_generated_kind(expected_generated),
                    format_generated_kind(actual_generated),
                );
            }
            push(
                &obj,
                "format",
                &format_value_format(ec.value_format.as_ref()),
                &format_value_format(ac.value_format.as_ref()),
            );
            push(
                &obj,
                "case_sensitive",
                format_case_sensitive(ec.case_sensitive),
                format_case_sensitive(ac.case_sensitive),
            );
            push(
                &obj,
                "collation",
                &format_collation(ec.collation.as_ref()),
                &format_collation(ac.collation.as_ref()),
            );
            push(
                &obj,
                "comment",
                ec.comment.as_deref().unwrap_or(""),
                ac.comment.as_deref().unwrap_or(""),
            );
            if let Some(expected_default) = ec.id_default.as_ref() {
                let recover_against_expected = || {
                    if actual_vendor.is_some_and(|vendor| {
                        vendor
                            .value_format
                            .catalog_default_marker_is_authoritative()
                    }) && ac.text_storage.is_some()
                    {
                        return catalog_text_id_default(
                            ac.default.as_deref(),
                            actual_dialect.expect("an actual vendor supplies its own dialect id"),
                            ac.expression_default,
                        );
                    }
                    catalog_id_default_for_expected(
                        expected_default,
                        ac.default.as_deref(),
                        actual_dialect,
                        ac.expression_default,
                    )
                };
                let actual_default =
                    if matches!(expected_default, IdDefaultSnapshot::UuidLiteral(_)) {
                        // A typed UUID reference may intentionally omit its child
                        // format CHECK, so the live side cannot always infer UUID
                        // literal semantics independently. The expected UUID arm is
                        // authoritative for canonicalizing its retained raw default.
                        recover_against_expected()
                    } else {
                        ac.id_default
                            .clone()
                            .unwrap_or_else(recover_against_expected)
                    };
                push(
                    &obj,
                    "default",
                    &format_id_default(Some(expected_default)),
                    &format_id_default(Some(&actual_default)),
                );
            } else if let (Some(expected_default), Some(actual_default)) = (
                // The ordinary-default surface: no ID facet, so the raw SQL text
                // is all either side holds. `comparable_column_default` documents
                // which spellings that text can be compared through and which it
                // cannot; a `None` on either side is that refusal, not an absence.
                comparable_column_default(ec.default.as_deref(), actual_vendor, None),
                comparable_column_default(
                    ac.default.as_deref(),
                    actual_vendor,
                    ac.expression_default,
                ),
            ) {
                push(
                    &obj,
                    "default",
                    &format_id_default(Some(&expected_default)),
                    &format_id_default(Some(&actual_default)),
                );
            }
        }
    }

    // Indexes: unique + elements + predicate + comments. A same-name index whose covered columns changed
    // out-of-band (REINDEX over a different column set, or a name reused for a
    // different shape) is surfaced by the `columns` compare — the name-only diff
    // cannot see it (1a).
    let act_idx: BTreeMap<&str, &IndexSnapshot> =
        act_t.indexes.iter().map(|i| (i.name.as_str(), i)).collect();
    for ei in &exp_t.indexes {
        if let Some(ai) = act_idx.get(ei.name.as_str()) {
            let obj = format!("index {}", ei.name);
            push(
                &obj,
                "unique",
                &ei.unique.to_string(),
                &ai.unique.to_string(),
            );
            push(
                &obj,
                "columns",
                &ei.columns.join(","),
                &ai.columns.join(","),
            );
            let bodies_comparable = index_expression_bodies_are_comparable(ai);
            let elements_eq = if bodies_comparable {
                index_elements_canonically_eq(&ei.elements, &ai.elements)
            } else {
                index_element_shapes_eq(&ei.elements, &ai.elements)
            };
            if !elements_eq {
                push(
                    &obj,
                    "elements",
                    &format_index_elements(&ei.elements),
                    &format_index_elements(&ai.elements),
                );
            }
            // Access-method drift (#index-method-drift): a same-name index whose
            // `pg_am` kind changed out-of-band — e.g. someone dropped the ANN
            // ivfflat and re-created a plain btree under the same name, or vice
            // versa. Name + columns can match while the method silently differs,
            // so this compare is the only thing that catches a btree→ivfflat
            // flip. An expected snapshot built without a method (`""`) opts out of
            // the compare (it never asserts a method it didn't intend to model).
            if !ei.access_method.is_empty() {
                push(&obj, "access_method", &ei.access_method, &ai.access_method);
            }
            let expected_predicate = effective_index_predicate(ei.predicate.as_deref());
            let actual_predicate = effective_index_predicate(ai.predicate.as_deref());
            let predicates_eq = if bodies_comparable {
                index_predicates_canonically_eq(expected_predicate, actual_predicate)
            } else {
                expected_predicate.is_some() == actual_predicate.is_some()
            };
            if !predicates_eq {
                push(
                    &obj,
                    "predicate",
                    ei.predicate.as_deref().unwrap_or(""),
                    ai.predicate.as_deref().unwrap_or(""),
                );
            }
            // The structural replacement for the exempted bodies: WHICH table columns
            // the index reads. Compared whenever both producers recorded it, which is
            // exactly where a body was exempted, so the exemption never leaves a site
            // uncovered by anything.
            if let (Some(expected_refs), Some(actual_refs)) =
                (index_referenced_columns(ei), index_referenced_columns(ai))
            {
                push(
                    &obj,
                    "referenced_columns",
                    &expected_refs.join(","),
                    &actual_refs.join(","),
                );
            }
            push(
                &obj,
                "include",
                &ei.include.join(","),
                &ai.include.join(","),
            );
            push(
                &obj,
                "with",
                &format_index_storage_params(ei.with.as_ref()),
                &format_index_storage_params(ai.with.as_ref()),
            );
            // `only` is deliberately absent. PostgreSQL renders `ON ONLY` in
            // `pg_get_indexdef` for every index on a partitioned parent, whether or
            // not `ONLY` was authored, so introspection reports a constant `false`
            // and comparing the field reported drift on every index that set it.
            // `IndexSnapshot` excludes it from equality for the same reason, and
            // documents the measurement.
            push(
                &obj,
                "comment",
                ei.comment.as_deref().unwrap_or(""),
                ai.comment.as_deref().unwrap_or(""),
            );
        }
    }

    // Constraints: kind + comparable canonical definitions. PostgreSQL FKs are
    // structured catalog reconstructions; the other comparable kinds retain the
    // authored/catalog body spelling. EXCLUDE definitions are intentionally
    // presence/kind-only: PG canonicalizes them differently from the authored IR
    // render, and this engine cannot normalize them to a proven comparable form.
    // The existence guard still fails closed for same-name unprovable constraints;
    // structural drift must not false-positive after a clean apply +
    // re-introspection.
    let act_con: BTreeMap<&str, &ConstraintSnapshot> = act_t
        .constraints
        .iter()
        .map(|c| (c.name.as_str(), c))
        .collect();
    for ec in &exp_t.constraints {
        if let Some(ac) = act_con.get(ec.name.as_str()) {
            let obj = format!("constraint {}", ec.name);
            push(&obj, "kind", &ec.kind, &ac.kind);
            if constraint_definition_is_comparable(&ec.kind)
                && constraint_definition_is_comparable(&ac.kind)
            {
                push(&obj, "definition", &ec.definition, &ac.definition);
            }
            push(
                &obj,
                "comment",
                ec.comment.as_deref().unwrap_or(""),
                ac.comment.as_deref().unwrap_or(""),
            );
        }
    }
}

/// Whether an index's rendered EXPRESSION BODIES - the partial-index `predicate` and
/// the text inside an [`IndexElementSnapshot::Expr`] key - are meaningful to compare
/// across an offline-rendered snapshot and a live catalog read.
///
/// The same problem `constraint_definition_is_comparable` answers for a CHECK, at the
/// two other sites that hold rendered SQL. PostgreSQL stores neither body as written:
/// `pg_get_expr` / `pg_get_indexdef` deparse from the parsed tree, so they re-quote
/// only the identifiers that need it and inject the casts parse analysis inferred.
/// Verified on PostgreSQL 18.4: `WHERE (note <> 'a')` reads back as
/// `(note <> 'a'::text)`, and `WHERE (true)` is dropped entirely. An offline renderer
/// quotes every column unconditionally and knows no column types, so it cannot
/// reproduce that - the comparison reported drift on partial indexes that had never
/// been touched, which is why `fold_drop_column_index_cascade_pg` had to weaken two of
/// its assertions to index SURVIVAL.
///
/// A column rename makes it permanent rather than merely noisy. `pg_index.indpred` and
/// `indexprs` are parse trees over attribute NUMBERS, so PostgreSQL deparses the NEW
/// name the instant a rename commits while the fold keeps the old rendering; no apply
/// can ever reconcile the two.
///
/// The exemption is keyed on the ACTUAL side having recorded
/// [`IndexSnapshot::expr_cascade_columns`], which is the PostgreSQL introspector and
/// nothing else. That is deliberate: it drops the text comparison exactly where the
/// structural replacement - the referenced-column set from `pg_depend`, compared via
/// [`index_referenced_columns`] - is available to take its place. MySQL has no partial
/// indexes and the SQLite introspector recovers no such set, so both keep comparing
/// text exactly as before.
///
/// WHY THE REDUCE-BOTH-SIDES TECHNIQUE DOES NOT RESCUE THIS, measured rather than
/// assumed. [`comparable_column_default`] closed its own gap by putting both sides
/// through one semantic key instead of declining, which works because `pg_get_expr` is
/// idempotent. Run on these bodies, that technique gets most of the way and then stops
/// dead. The shared fingerprint already collapses everything the catalog INJECTS -
/// measured on the four bodies this file's fixtures produce, `("note" <> 'a')` and
/// `(note <> 'a'::text)` differ only in the QUOTING of the identifier, the `::text` and
/// the parentheses having normalised away - so a rule that unquotes an identifier
/// PostgreSQL would not have quoted would land them on one key.
///
/// The RENAME is what cannot be reduced. Measured on PostgreSQL 18.4, after
/// `RENAME COLUMN qty_on_hand TO amount_on_hand` the fold projects
/// `("qty_on_hand" > 0)` where the catalog deparses `(amount_on_hand > 0)`
/// (`fold_rename_column_stale_index_body_pg` pins both sides separately). Those are two
/// different COLUMN NAMES, not two spellings of one thing; normalisation reduces
/// spellings. The fold cannot repair its side either, because
/// [`IndexSnapshot::predicate`] is rendered TEXT and substituting a name inside it
/// would rewrite the string literal in `WHERE (note <> 'qty_on_hand')` - the exact
/// false positive [`IndexSnapshot::expr_cascade_columns`] exists to avoid. So comparing
/// these bodies would make every column rename permanent drift that no apply can clear,
/// which is strictly worse than the silence below. It stays declined.
///
/// [`comparable_generated_column`] reaches the same verdict about a generated column's
/// expression, for the same measurement, and both take the same way out: compare the
/// structural facet the catalog stores natively - the referenced-column set here,
/// `attgenerated` there - and leave the rendered body alone.
///
/// What this gives up: two expressions over the SAME columns with DIFFERENT logic
/// compare equal. `WHERE (qty > 0)` and `WHERE (qty > -2147483648)` are
/// indistinguishable, and so are `(a + 1)` and `(a * 1000)` as expression keys. That
/// is a real loss, the same one the CHECK exemption already takes. Recovering it needs
/// the treatment foreign keys get: parse the catalog text back to the closed AST and
/// compare structurally, rather than comparing spellings.
///
/// PRESENCE is a separate question and is NOT declined here - see
/// [`effective_index_predicate`], which reduces both sides through one key rather than
/// declining, because the server itself defines that reduction.
///
/// Presence is NOT exempted, and neither is any other facet: `None` against `Some` on
/// the predicate, element count and order, `Column`-vs-`Expr` element kind, plain
/// column names and sort orders all still compare.
fn index_expression_bodies_are_comparable(actual: &IndexSnapshot) -> bool {
    actual.expr_cascade_columns.is_none()
}

/// One index's partial predicate as the SERVER understands it: `None` for an index
/// that restricts nothing.
///
/// PostgreSQL discards a `WHERE` clause that is the bare constant `TRUE` - `CREATE
/// INDEX ... WHERE TRUE` leaves `pg_index.indpred` NULL and the index is total. The
/// fold has no server to ask, so it projects the predicate it was authored with, and
/// the presence comparison above then read `Some` against `None` and reported
/// `predicate: expected "TRUE", actual ""` on the FIRST introspection after a clean
/// apply, forever, with nothing in the history but the `createIndex` that built it.
///
/// So this reduces BOTH sides through one key instead of declining - the technique
/// [`comparable_column_default`] uses for a literal default, applied to presence. It
/// is sound because the server DEFINES the reduction: a constant-true predicate is
/// not a predicate, which is why there is nothing in the catalog to read back.
///
/// The rule is exactly as narrow as the measurement. On PostgreSQL 18.4 only the bare
/// constant is dropped; every other predicate survives verbatim, including the ones
/// that are semantically constant:
///
/// | authored                 | read back from the catalog |
/// |--------------------------|----------------------------|
/// | `WHERE TRUE`             | *(no predicate at all)*    |
/// | `WHERE FALSE`            | `false`                    |
/// | `WHERE (1 = 1)`          | `(1 = 1)`                  |
/// | `WHERE (TRUE AND TRUE)`  | `(true AND true)`          |
///
/// Widening this to "anything tautological" would therefore INVENT the divergence it
/// is here to remove, so it stays a text match on the constant, modulo the case and
/// grouping either renderer may add. Presence is not otherwise weakened: an index that
/// loses a real predicate out of band still reports.
fn effective_index_predicate(predicate: Option<&str>) -> Option<&str> {
    let predicate = predicate?;
    let mut body = predicate.trim();
    while let Some(inner) = body
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
    {
        body = inner.trim();
    }
    (!body.eq_ignore_ascii_case("true")).then_some(predicate)
}

/// [`index_elements_canonically_eq`] with the `Expr` BODIES exempted - element count,
/// order, `Column`-vs-`Expr` kind, plain column names and canonical sort orders all
/// still have to agree, and only the rendered text inside an expression key is skipped.
///
/// Deliberately NOT a change to `index_elements_canonically_eq` itself: that predicate
/// backs `IndexSnapshot`'s `PartialEq` / `Eq` and the declarative index pairing,
/// where a refusal is a human-readable stop rather than a silently wrong answer. This
/// exemption is drift-local.
fn index_element_shapes_eq(left: &[IndexElementSnapshot], right: &[IndexElementSnapshot]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(a, b)| match (a, b) {
            (IndexElementSnapshot::Expr(_), IndexElementSnapshot::Expr(_)) => true,
            _ => index_elements_canonically_eq(std::slice::from_ref(a), std::slice::from_ref(b)),
        })
}

/// Every table column an index READS, or `None` when this producer recorded no
/// expression-site provenance.
///
/// The union of the exact-name lists an index carries - the key `columns` and the
/// `INCLUDE` payload - with [`IndexSnapshot::expr_cascade_columns`]. The union is what
/// makes the two producers comparable: the offline one records the expression sites
/// ALONE (the key and INCLUDE columns are already exact names it would only repeat),
/// while `pg_depend` reports every attribute the index depends on, key and INCLUDE
/// included. Unioning both sides lands them on the same set without either having to
/// change what it stores.
///
/// `None` on an index with no expression site and on any producer that cannot record
/// one, which is what keeps a constraint-backed index out of the comparison:
/// `<table>_pkey` depends on its `pg_constraint`, not on the attributes, so `pg_depend`
/// reports no columns for it at all. Such an index has no expression site either, so
/// both sides answer `None` and the compare is skipped rather than reporting an empty
/// set against a populated one.
fn index_referenced_columns(index: &IndexSnapshot) -> Option<Vec<&str>> {
    let expression_columns = index.expr_cascade_columns.as_ref()?;
    let mut out: std::collections::BTreeSet<&str> =
        index.columns.iter().map(String::as_str).collect();
    out.extend(index.include.iter().map(String::as_str));
    out.extend(expression_columns.iter().map(String::as_str));
    Some(out.into_iter().collect())
}

/// Whether a constraint's `definition` text is meaningful to compare across an
/// offline-rendered snapshot and a live catalog read.
///
/// `EXCLUDE` and `CHECK` are excluded because PostgreSQL does not store either
/// body as written: `pg_get_constraintdef` deparses from the parsed tree, so it
/// re-quotes only the identifiers that need it, injects the casts that parse
/// analysis inferred, expands `IN` to `= ANY (ARRAY[...])`, and lowercases
/// keywords. Verified on PostgreSQL 18.4: `CHECK (quantity > 0)` reads back as
/// `CHECK ((quantity > 0))`, `CHECK (code = 'x')` as `CHECK ((code = 'x'::text))`,
/// and `CHECK (TRUE)` as `CHECK (true)`. An offline renderer quotes every column
/// unconditionally and knows no column types, so it cannot reproduce any of that
/// without PostgreSQL's own parse analysis. Comparing the text therefore reports
/// drift on every CHECK constraint that exists, on every comparison.
///
/// This makes the differ agree with the intent already recorded at
/// `render::declarative::field_check_constraints`, which states that CHECK bodies
/// are not re-diffed and that presence plus enforcement are what round-trip.
///
/// What this gives up: a CHECK whose body is altered out of band while keeping
/// its name and kind is not reported. That is a real loss, not a technicality -
/// swapping `CHECK (quantity > 0)` for `CHECK (quantity > -2147483648)` leaves the
/// invariant vacuous and this differ silent. Recovering it needs the treatment
/// foreign keys already get: parse the catalog text back to the closed AST and
/// compare structurally, rather than comparing spellings.
///
/// Kept separate from `constraint_definition_is_retained` (private to
/// `zero_migrate_postgres::backend::drift_sql`, so it is named here rather than linked)
/// on purpose — it is the PostgreSQL introspector's own rule about what to STORE and
/// now lives with the reader that applies it. Not
/// comparing a body is not a reason to stop recording it: the guard's fail-closed
/// refusal reports the live definition so an operator can see what is actually
/// installed, and collapsing that to `<present>` would remove the only text in the
/// message that says anything specific.
fn constraint_definition_is_comparable(kind: &str) -> bool {
    !matches!(kind, "EXCLUDE" | "CHECK")
}

/// The two sides' vendor-object identity when comparing them is meaningful, and
/// `None` when it is not.
///
/// The THIRD member of the family [`constraint_definition_is_comparable`] and
/// [`index_expression_bodies_are_comparable`] belong to, answering the same
/// question - is this text worth comparing across an offline render and a live
/// catalog read? - for PostgreSQL functions, policies and triggers. Its shape
/// differs because its answer is structural rather than per-field: the
/// non-comparable text is never COLLECTED, so there is no field left to exempt at
/// comparison time and nothing a later change could start comparing by accident.
///
/// WHAT PostgreSQL NORMALISES. Measured on PostgreSQL 18.4:
///
/// | authored                       | read back from the catalog   |
/// |--------------------------------|------------------------------|
/// | `CREATE FUNCTION f(x int)`     | `f(integer)`                 |
/// | `USING (owner = current_user)` | `((owner = CURRENT_USER))`   |
/// | `WHEN (NEW.v > 0)`             | `WHEN ((new.v > 0))`         |
///
/// `pg_get_expr` deparses a policy predicate and a trigger `WHEN` clause from the
/// parse tree exactly as it does the index predicate the sibling above exempts, and
/// `format_type` resolves an argument-type alias. So [`VendorObjectIdentities`]
/// carries only what survives the round trip: a function's schema, name and
/// canonicalised argument vector; a policy's table, command, roles and permissive
/// flag; a trigger's table, timing and event set. PostgreSQL stores every one of
/// those STRUCTURALLY - a `polcmd` code, a `tgtype` bit set, an OID vector - which
/// is the same property that makes an [`IndexElementSnapshot::Column`] immune where
/// an `Expr` key is not.
///
/// WHEN THE COMPARISON IS SKIPPED: `None` on either side.
/// [`SchemaSnapshot::vendor_objects`] is `None` for every snapshot that did not
/// look - a SQLite or MySQL catalog read, and a fold for either dialect - so those
/// engines cannot be accused of having lost objects they never modeled. This is the
/// absent-side rule the row-level-security diff applies per table, at the
/// granularity THIS facet needs: presence is the signal here, so a per-object skip
/// would make a dropped policy unreportable, which is exactly the case worth
/// reporting.
///
/// WHAT THIS GIVES UP: a policy predicate or a trigger `WHEN` clause rewritten out
/// of band while the identity is untouched is not reported. `USING (owner =
/// current_user)` swapped for `USING (true)` leaves the table readable by everyone
/// and this differ silent. That is a real loss, the same one the CHECK and
/// partial-index exemptions already take, and recovering it needs the same treatment
/// foreign keys get: parse the catalog text back to the closed AST and compare
/// structurally rather than comparing spellings.
///
/// A FUNCTION BODY IS NO LONGER ON THAT LIST. It was, on the assumption that it
/// deparsed like the predicates do; it does not. `pg_proc.prosrc` is the authored
/// text byte for byte, so [`comparable_function_body`] compares it directly and
/// [`VendorObjectIdentities::functions`] carries it. The predicates are genuinely
/// irreducible here and the body never was.
fn comparable_vendor_objects<'a>(
    expected: &'a SchemaSnapshot,
    actual: &'a SchemaSnapshot,
) -> Option<(&'a VendorObjectIdentities, &'a VendorObjectIdentities)> {
    Some((
        expected.vendor_objects.as_ref()?,
        actual.vendor_objects.as_ref()?,
    ))
}

/// `schema.name(argtype, ...)` - the overload, not just the name, because
/// PostgreSQL lets two functions share a name.
fn function_label(key: &FunctionKey) -> String {
    format!(
        "function {}.{}({})",
        key.schema,
        key.name,
        key.arg_types.join(", ")
    )
}

/// `policy <name> on <schema>.<table>` - a policy name is scoped to its table, so
/// the table is part of the identity and not decoration.
fn policy_label(key: &PolicyKey) -> String {
    format!("policy {} on {}.{}", key.name, key.schema, key.table)
}

/// `trigger <name> on <schema>.<table>`, for the same reason.
fn trigger_label(key: &TriggerKey) -> String {
    format!("trigger {} on {}.{}", key.name, key.schema, key.table)
}

/// The two sides' function BODY reduced to a comparable pair, and `None` when
/// comparing them is not meaningful.
///
/// The SIXTH member of the family [`constraint_definition_is_comparable`],
/// [`index_expression_bodies_are_comparable`], [`comparable_vendor_objects`],
/// [`comparable_column_default`] and [`comparable_generated_column`] belong to,
/// answering the same question - is this text worth comparing across an offline
/// render and a live catalog read? - for the one thing a function actually DOES.
/// Until this existed nothing compared it: [`comparable_vendor_objects`] compares a
/// function's schema, name and canonicalised argument vector and nothing else, so a
/// `CREATE OR REPLACE FUNCTION` run out of band with the SAME signature and a
/// DIFFERENT body reported the schema CLEAN. That is not an edge case - replacing
/// the body without touching the signature is the ordinary way a function is
/// changed.
///
/// WHY THIS IS COMPARABLE WHERE THE SIBLING PREDICATES ARE NOT. The vendor-object
/// work excluded function bodies, policy `USING`/`WITH CHECK` and trigger `WHEN`
/// together, on the grounds that PostgreSQL does not store SQL as written. That is
/// TRUE OF THE PREDICATES AND FALSE OF THE BODY. Measured on PostgreSQL 18.4:
///
/// | authored                            | read back from the catalog         |
/// |-------------------------------------|------------------------------------|
/// | `AS $$ SELECT x+1 $$`               | `prosrc` = `[ SELECT x+1 ]`        |
/// | `AS $$BEGIN\n   RETURN   42;\nEND$$`| `[BEGIN\n   RETURN   42;\nEND]`    |
/// | `USING (owner = current_user)`      | `((owner = CURRENT_USER))`         |
///
/// A `LANGUAGE sql`/`plpgsql` body is an OPAQUE STRING to PostgreSQL - it is stored
/// verbatim and handed to the language handler at call time - so leading and
/// trailing spaces, odd internal whitespace and a nested `$tag$` literal all survive
/// byte for byte. A policy predicate is a parse tree `pg_get_expr` re-prints, which
/// injects casts, uppercases `CURRENT_USER` and adds parentheses. So the body needs
/// NO normaliser and the predicates cannot have one. Policies and triggers are
/// deliberately left where they are.
///
/// The one reduction both sides do go through is [`comparable_function_body`], and
/// it is forced by this project's own renderer rather than by PostgreSQL: the body
/// is emitted inside `$zsfn$\n … \n$zsfn$`, so the stored `prosrc` carries a newline
/// at each end that the authored string does not. That predicate documents the trim
/// and what it costs.
///
/// WHEN THE COMPARISON IS SKIPPED, and why the skip is not an oversight: `None` on
/// either side, which has exactly one cause - a SQL-standard-body
/// (`BEGIN ATOMIC … END`) function. Measured on PostgreSQL 18.4, `CREATE FUNCTION
/// f2(x int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT x+1; END` stores its body
/// as a PARSE TREE in `prosqlbody` and leaves `prosrc` EMPTY. Comparing an authored
/// body against `""` would report every such function as drifted on every run, so
/// [`FunctionIdentity::from_catalog`] declines DETECTABLY - empty `prosrc` WITH a
/// non-null `prosqlbody`, never empty `prosrc` alone. The identity comparison is
/// unaffected: such a function is still reported if it disappears.
///
/// WHAT THIS GIVES UP: the body of a `BEGIN ATOMIC` function, rewritten out of band,
/// is not reported. Recovering it needs what the predicates need - `pg_get_functiondef`
/// deparses `prosqlbody` back to text, but the authored side has no deparser to meet
/// it with, so it is the same parse-the-catalog-text-back-to-the-closed-AST work
/// foreign keys already get, not a spelling comparison.
fn comparable_function_body<'a>(
    expected: &'a FunctionIdentity,
    actual: &'a FunctionIdentity,
) -> Option<(&'a str, &'a str)> {
    Some((expected.body.as_deref()?, actual.body.as_deref()?))
}

/// The comparable facets of ONE same-signature function present on both sides.
///
/// `table` on the reported [`AlteredObject`] is the function NAME: a function does
/// not belong to a table, and the roles diff already reports a table-less object
/// this way.
fn diff_function_attrs(
    key: &FunctionKey,
    expected: &FunctionIdentity,
    actual: &FunctionIdentity,
    altered: &mut Vec<AlteredObject>,
) {
    let Some((expected_body, actual_body)) = comparable_function_body(expected, actual) else {
        return;
    };
    push_vendor_attr(
        altered,
        &key.name,
        function_label(key),
        "body",
        expected_body.to_string(),
        actual_body.to_string(),
    );
}

/// The comparable facets of ONE same-named policy present on both sides.
///
/// An empty role list renders as `PUBLIC` rather than as nothing: that is what an
/// authored `to: None` means and what `pg_policy` stores for it, and an empty
/// string in a drift report would read as a missing value instead of a real one.
fn diff_policy_attrs(
    key: &PolicyKey,
    expected: &PolicyIdentity,
    actual: &PolicyIdentity,
    altered: &mut Vec<AlteredObject>,
) {
    let object = policy_label(key);
    push_vendor_attr(
        altered,
        &key.table,
        object.clone(),
        "command",
        expected.for_cmd.as_sql().to_string(),
        actual.for_cmd.as_sql().to_string(),
    );
    push_vendor_attr(
        altered,
        &key.table,
        object.clone(),
        "roles",
        policy_role_list(&expected.to),
        policy_role_list(&actual.to),
    );
    push_vendor_attr(
        altered,
        &key.table,
        object,
        "permissive",
        expected.permissive.to_string(),
        actual.permissive.to_string(),
    );
}

/// The roles a policy applies to, with PostgreSQL's default spelled out.
fn policy_role_list(roles: &[String]) -> String {
    if roles.is_empty() {
        "PUBLIC".to_string()
    } else {
        roles.join(", ")
    }
}

/// The comparable facets of ONE same-named trigger present on both sides.
///
/// The event set renders in the fixed order both sides normalise to, so a
/// re-ordered authored list cannot show up here as a change - `tgtype` is a bit
/// set and does not retain the order at all.
fn diff_trigger_attrs(
    key: &TriggerKey,
    expected: &TriggerIdentity,
    actual: &TriggerIdentity,
    altered: &mut Vec<AlteredObject>,
) {
    let object = trigger_label(key);
    push_vendor_attr(
        altered,
        &key.table,
        object.clone(),
        "timing",
        expected.timing.as_sql().to_string(),
        actual.timing.as_sql().to_string(),
    );
    push_vendor_attr(
        altered,
        &key.table,
        object,
        "events",
        trigger_event_list(&expected.events),
        trigger_event_list(&actual.events),
    );
}

/// The firing events as `CREATE TRIGGER` spells them.
fn trigger_event_list(events: &[TriggerEvent]) -> String {
    events
        .iter()
        .map(|event| event.as_sql())
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Missing / unexpected indexes for one table, pairing exact names first and then
/// derived-name aliases, one-to-one.
///
/// Shares [`pair_indexes`](crate::render::declarative::pair_indexes) with the
/// migration differ, so drift and the plan cannot disagree about which live index a
/// desired one meant. When the pairing reports an ambiguity - one live index claimed
/// as an alias by two desired indexes - this falls back to the name-only diff, which
/// reports the objects instead of guessing which one was intended.
///
/// There is no matching change to the attribute pass. An alias is only accepted when
/// `same_definition_except_name` holds, and that predicate covers exactly the
/// attributes the index attribute diff compares (unique, columns, elements, access
/// method, predicate, INCLUDE, storage params, comment), so an accepted pair has no
/// attribute left to report as altered.
fn diff_indexes(
    table: &str,
    expected: &[IndexSnapshot],
    actual: &[IndexSnapshot],
    aliases: &BTreeMap<String, String>,
    missing: &mut Vec<String>,
    unexpected: &mut Vec<String>,
) {
    let names = |indexes: &[IndexSnapshot]| {
        indexes
            .iter()
            .map(|i| i.name.clone())
            .collect::<Vec<String>>()
    };
    let Ok(pairing) = crate::render::declarative::pair_indexes(table, expected, actual, aliases)
    else {
        diff_named(
            table,
            "index ",
            &names(expected),
            &names(actual),
            missing,
            unexpected,
        );
        return;
    };
    for ei in expected {
        if !pairing.matched.contains_key(ei.name.as_str()) {
            missing.push(format!("{table} index {}", ei.name));
        }
    }
    for ai in actual {
        if !pairing.consumed_live.contains(ai.name.as_str()) {
            unexpected.push(format!("{table} index {}", ai.name));
        }
    }
}

/// Diff two name lists belonging to one table, pushing qualified names into the
/// missing / unexpected accumulators. `kind_prefix` is `""` for columns (so the
/// name reads `table.col`), `"index "` / `"constraint "` otherwise.
fn diff_named(
    table: &str,
    kind_prefix: &str,
    expected: &[String],
    actual: &[String],
    missing: &mut Vec<String>,
    unexpected: &mut Vec<String>,
) {
    use std::collections::BTreeSet;
    let exp: BTreeSet<&str> = expected.iter().map(String::as_str).collect();
    let act: BTreeSet<&str> = actual.iter().map(String::as_str).collect();
    let label = |child: &str| {
        if kind_prefix.is_empty() {
            format!("{table}.{child}")
        } else {
            format!("{table} {kind_prefix}{child}")
        }
    };
    for child in expected {
        if !act.contains(child.as_str()) {
            missing.push(label(child));
        }
    }
    for child in actual {
        if !exp.contains(child.as_str()) {
            unexpected.push(label(child));
        }
    }
}

#[cfg(test)]
mod physical_contract_tests {
    //! CORE's half of the physical-contract comparison: that a vendor leg on both
    //! sides is consulted, that anything less falls through to the portable
    //! comparison, and that the report follows the comparator.
    //!
    //! The contract used here is a STAND-IN declared in this module, not a shipping
    //! backend's. That is deliberate twice over. Core may not name a vendor crate -
    //! `dialect_matrix/core_names_no_vendor_crate.rs` is the ratchet - and the
    //! property under test is not any vendor's rule but the seam's: whatever the
    //! vendor answers, core asks it exactly when a leg is present on both sides.
    //! What makes two MySQL contracts equal is asserted where that rule lives, in
    //! `zero_migrate_mysql::physical_type`.

    use std::any::Any;
    use std::sync::Arc;

    use super::{column_data_types_eq, ColumnSnapshot};
    use zero_migrate_backend::dialectal::{Dialectal, DialectalValue, VendorColumnFacts};
    use zero_migrate_ir::dialect::DialectId;

    const A_BACKEND: DialectId = DialectId::new("a_backend");
    const ANOTHER_BACKEND: DialectId = DialectId::new("another_backend");

    /// A stand-in for a backend's parsed physical identity: two of them are the same
    /// column when they carry the same text, and they report themselves verbatim.
    #[derive(Debug, PartialEq, Eq)]
    struct Contract(&'static str);

    impl DialectalValue for Contract {
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn dialectal_eq(&self, other: &dyn Any) -> bool {
            other.downcast_ref::<Self>() == Some(self)
        }
    }

    impl VendorColumnFacts for Contract {
        fn physical_identity(&self, other: &dyn VendorColumnFacts) -> bool {
            other.as_any().downcast_ref::<Self>() == Some(self)
        }
        fn type_drift_report(&self, other: &dyn VendorColumnFacts) -> Option<(String, String)> {
            let other = other.as_any().downcast_ref::<Self>()?;
            (self != other).then(|| (self.0.to_string(), other.0.to_string()))
        }
    }

    /// A column whose portable `data_type` is `data_type`, optionally carrying
    /// `contract` as `dialect`'s leg.
    fn column(data_type: &str, leg: Option<(DialectId, &'static str)>) -> ColumnSnapshot {
        let mut vendor: Dialectal<dyn VendorColumnFacts> = Dialectal::new();
        if let Some((dialect, contract)) = leg {
            vendor.insert(
                dialect,
                Arc::new(Contract(contract)) as Arc<dyn VendorColumnFacts>,
            );
        }
        ColumnSnapshot {
            name: "c".to_string(),
            data_type: data_type.to_string(),
            vendor,
            ..Default::default()
        }
    }

    /// A leg on `A_BACKEND` carrying `contract`.
    fn leg(contract: &'static str) -> Option<(DialectId, &'static str)> {
        Some((A_BACKEND, contract))
    }

    #[test]
    fn a_contract_on_both_sides_is_seen_where_the_portable_type_is_blind() {
        // Both sides fold to the portable `text`, so `data_type` alone reports
        // agreement. The contract is what tells them apart, and that blindness is
        // the whole reason the leg exists.
        assert!(
            column_data_types_eq(&column("text", None), &column("text", None)),
            "without contracts the two are indistinguishable, which is the defect"
        );
        assert!(
            !column_data_types_eq(&column("text", leg("wide")), &column("text", leg("narrow"))),
            "with contracts on both sides the vendor's verdict decides"
        );
        assert!(column_data_types_eq(
            &column("text", leg("wide")),
            &column("text", leg("wide"))
        ));
    }

    #[test]
    fn one_side_without_a_contract_keeps_the_portable_comparison() {
        // A snapshot from another dialect's catalog carries no leg, and neither does
        // one written before that backend's contract existed. Comparing present
        // against absent must not manufacture a difference.
        assert!(column_data_types_eq(
            &column("text", leg("wide")),
            &column("text", None)
        ));
        assert!(column_data_types_eq(
            &column("text", None),
            &column("text", leg("wide"))
        ));
    }

    #[test]
    fn two_different_dialects_legs_are_not_paired_with_each_other() {
        // The carrier keys by dialect, so one backend's contract is never compared
        // against another's - a leg each is still nobody describing the same thing
        // twice, and the portable comparison stands.
        assert!(column_data_types_eq(
            &column("text", leg("wide")),
            &column("text", Some((ANOTHER_BACKEND, "narrow"))),
        ));
    }

    #[test]
    fn a_dialect_without_a_contract_keeps_the_portable_spelling() {
        // A backend that leaves no leg must report exactly as it did before any
        // contract could be printed.
        let (expected, actual) = super::column_data_type_report(
            &column("character varying(255)", None),
            &column("integer", None),
        );
        assert_eq!(expected, "character varying(255)");
        assert_eq!(actual, "integer");

        // One side only is still the portable spelling: a contract compared against
        // an absent one describes nothing about the database.
        let (expected, actual) =
            super::column_data_type_report(&column("text", leg("wide")), &column("integer", None));
        assert_eq!(expected, "text");
        assert_eq!(actual, "integer");
    }

    #[test]
    fn a_difference_the_portable_type_cannot_see_reaches_the_report() {
        // The defect, at the unit boundary: both sides read `text`, so the report
        // printed two equal strings and `diff_attrs`'s `push` dropped the entry.
        let (expected, actual) = super::column_data_type_report(
            &column("text", leg("wide")),
            &column("text", leg("narrow")),
        );
        assert_ne!(
            expected, actual,
            "a narrowing must print two sides a reader can tell apart"
        );
        assert_eq!(expected, "wide");
        assert_eq!(actual, "narrow");
    }
}

#[cfg(test)]
mod constraint_definition_tests {
    use super::{diff_snapshots, ConstraintSnapshot};
    use crate::model::snapshot::{SchemaSnapshot, TableSnapshot};
    use crate::TableRuntimeOptions;

    /// These cover the differ directly rather than through a live database. The
    /// PostgreSQL round-trip oracle that found the CHECK mismatch is behind
    /// `require_live_pg!`, so on a checkout with no database configured it cannot run
    /// at all, and these are what still measure this contract.
    fn snapshot_with(constraints: Vec<ConstraintSnapshot>) -> SchemaSnapshot {
        let mut snapshot = SchemaSnapshot::default();
        snapshot.tables.insert(
            "orders".to_string(),
            TableSnapshot {
                columns: Vec::new(),
                indexes: Vec::new(),
                constraints,
                runtime_options: TableRuntimeOptions::default(),
                partition_by: None,
                comment: None,
                stored_create_sql: None,
            },
        );
        snapshot
    }

    fn constraint(name: &str, kind: &str, definition: &str) -> ConstraintSnapshot {
        ConstraintSnapshot {
            name: name.to_string(),
            kind: kind.to_string(),
            definition: definition.to_string(),
            comment: None,
            cascade_columns: None,
        }
    }

    #[test]
    fn check_bodies_that_differ_only_in_spelling_are_not_drift() {
        // The exact pair that PostgreSQL 18.4 produces: the offline renderer quotes
        // every column, `pg_get_constraintdef` deparses without the quotes.
        let expected = snapshot_with(vec![constraint(
            "orders_quantity_check",
            "CHECK",
            "CHECK ((\"quantity\" > 0))",
        )]);
        let actual = snapshot_with(vec![constraint(
            "orders_quantity_check",
            "CHECK",
            "CHECK ((quantity > 0))",
        )]);

        assert!(
            diff_snapshots(&expected, &actual).is_clean(),
            "a CHECK body that differs only in deparse spelling must not report drift"
        );
    }

    #[test]
    fn a_renamed_check_constraint_is_still_drift() {
        // Skipping the body comparison must not make CHECK constraints invisible:
        // without this, the test above is satisfied by a differ that ignores them.
        let expected = snapshot_with(vec![constraint(
            "orders_quantity_check",
            "CHECK",
            "CHECK ((quantity > 0))",
        )]);
        let actual = snapshot_with(vec![constraint(
            "orders_quantity_chk",
            "CHECK",
            "CHECK ((quantity > 0))",
        )]);

        let drift = diff_snapshots(&expected, &actual);
        assert!(
            !drift.is_clean(),
            "a CHECK constraint present under a different name must report drift: {drift:#?}"
        );
    }

    #[test]
    fn a_check_constraint_that_changed_kind_is_still_drift() {
        let expected = snapshot_with(vec![constraint(
            "orders_quantity_check",
            "CHECK",
            "CHECK ((quantity > 0))",
        )]);
        let actual = snapshot_with(vec![constraint(
            "orders_quantity_check",
            "UNIQUE",
            "UNIQUE (quantity)",
        )]);

        let drift = diff_snapshots(&expected, &actual);
        assert!(
            !drift.is_clean(),
            "a constraint whose kind changed must report drift: {drift:#?}"
        );
    }

    #[test]
    fn a_unique_body_change_is_still_drift() {
        // The exclusion is scoped to CHECK and EXCLUDE. Every other kind still
        // compares its body, so widening the exclusion by accident shows up here.
        let expected = snapshot_with(vec![constraint(
            "orders_code_key",
            "UNIQUE",
            "UNIQUE (code)",
        )]);
        let actual = snapshot_with(vec![constraint(
            "orders_code_key",
            "UNIQUE",
            "UNIQUE (code, tenant)",
        )]);

        let drift = diff_snapshots(&expected, &actual);
        assert!(
            !drift.is_clean(),
            "a UNIQUE body change must still report drift: {drift:#?}"
        );
    }
}

/// **A snapshot no backend claims must not be read in any backend's dialect.**
///
/// [`introspected_table_vendor`] recognises a live catalog read by the evidence only
/// introspection leaves — a `ddl_type_override`, a `text_storage`, a
/// `stored_create_sql` — and a table carrying none of the three matches no vendor. The
/// ID-default comparison below it then reaches
/// [`catalog_id_default_for_expected`](crate::render::value_format::catalog_id_default_for_expected)
/// with `None` for the dialect, which is the only entry point that takes an optional
/// one.
///
/// That is a LIVE shape, not a defensive one, and the two shipping halves of the proof
/// live in different places on purpose:
///
/// * `pg_drift::drift_unattributed_snapshot` measures it against a real server. MySQL
///   introspection writes `ddl_type_override: None` and `stored_create_sql: None`
///   unconditionally, so a MySQL table whose columns are all numeric leaves nothing any
///   vendor claims.
/// * These two cover the case that server cannot reach. A UUID generator default needs
///   a UUID column, and on MySQL that is character-typed — which hands MySQL its marker
///   back — while PostgreSQL stamps `ddl_type_override` on EVERY column it reads. The
///   route in is [`diff_snapshots`] itself, which is `pub` and takes whatever actual
///   snapshot the caller holds, including one restored from storage that predates the
///   markers.
///
/// The contract: an unattributed snapshot is never granted a vendor's SEMANTIC
/// identity. `gen_random_uuid()` is PostgreSQL's UUIDv4 generator and nothing else's,
/// so reading it as [`IdDefaultSnapshot::UuidV4`] would be core resolving a vendor out
/// of a snapshot that names none — and it would silently accept a column whose default
/// is the literal STRING `gen_random_uuid()` as satisfying an authored UUIDv4. The
/// answer degrades to the vendor-neutral textual key instead, which still NAMES what
/// the catalog holds, so the operator gets a line they can act on rather than silence.
/// The same refusal the dialect-KNOWN half already makes in
/// `render::value_format`'s `qualified_postgres_generators_and_dialect_specific_fallbacks_are_exact`,
/// where a foreign dialect's generator may not satisfy a typed-reference default.
#[cfg(test)]
mod unattributed_snapshot_tests {
    use super::{diff_snapshots, ColumnSnapshot, IdDefaultSnapshot, SchemaSnapshot, TableSnapshot};
    use crate::TableRuntimeOptions;

    /// PostgreSQL's UUIDv4 generator, as `pg_get_expr` deparses it.
    const PG_UUID_V4: &str = "gen_random_uuid()";

    fn snapshot_with(column: ColumnSnapshot) -> SchemaSnapshot {
        let mut snapshot = SchemaSnapshot::default();
        snapshot.tables.insert(
            "accounts".to_string(),
            TableSnapshot {
                columns: vec![column],
                indexes: Vec::new(),
                constraints: Vec::new(),
                runtime_options: TableRuntimeOptions::default(),
                partition_by: None,
                comment: None,
                stored_create_sql: None,
            },
        );
        snapshot
    }

    /// The authored side: an ID column declared to default to UUIDv4.
    fn expected() -> SchemaSnapshot {
        snapshot_with(ColumnSnapshot {
            name: "id".to_string(),
            data_type: "uuid".to_string(),
            id_default: Some(IdDefaultSnapshot::UuidV4),
            ..Default::default()
        })
    }

    /// The live side holding the generator as catalog text, with `ddl_type_override`
    /// as the ONLY difference between the two runs — it is PostgreSQL's provenance
    /// marker, and it is excluded from `ColumnSnapshot`'s equality, so it moves nothing
    /// else the differ looks at.
    fn actual(ddl_type_override: Option<&str>) -> SchemaSnapshot {
        snapshot_with(ColumnSnapshot {
            name: "id".to_string(),
            data_type: "uuid".to_string(),
            default: Some(PG_UUID_V4.to_string()),
            id_default: None,
            ddl_type_override: ddl_type_override.map(str::to_string),
            ..Default::default()
        })
    }

    /// The one `default` line for `accounts.id`, or a report of why there is none.
    fn default_line(actual: &SchemaSnapshot) -> Result<super::AlteredObject, String> {
        let drift = diff_snapshots(&expected(), actual);
        drift
            .altered_objects
            .iter()
            .find(|a| a.object == "column id" && a.field == "default")
            .cloned()
            .ok_or_else(|| format!("no default line for accounts.id in {drift:#?}"))
    }

    /// **THE INSTRUMENT.** With the marker present the snapshot IS a PostgreSQL read,
    /// the generator is recovered as UUIDv4, and the two sides agree. Without this, the
    /// refusal below is satisfied by a fixture whose default nothing could ever
    /// recover.
    #[test]
    fn a_postgres_marked_snapshot_recovers_the_generator_it_carries() {
        let drift = diff_snapshots(&expected(), &actual(Some("uuid")));
        assert!(
            drift.is_clean(),
            "a snapshot carrying PostgreSQL's own provenance marker must recover \
             {PG_UUID_V4} as the authored UUIDv4 default: {drift:#?}"
        );
    }

    #[test]
    fn an_unattributed_snapshot_is_not_granted_a_vendor_semantic_identity() {
        let line = default_line(&actual(None)).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            line.expected, "uuidV4",
            "the authored side is unchanged by the actual side's provenance"
        );
        for granted in ["uuidV4", "uuidV7"] {
            assert_ne!(
                line.actual, granted,
                "a snapshot no backend claims must not be read in PostgreSQL's dialect: \
                 {PG_UUID_V4} is PostgreSQL's generator and nothing else's, so granting \
                 it {granted} would also accept a column defaulting to the literal \
                 string"
            );
        }
        assert!(
            line.actual.contains("gen_random_uuid"),
            "the refusal must still NAME what the catalog holds so the operator can act \
             on the line, and it reads {:?}",
            line.actual
        );
    }
}
