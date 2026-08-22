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
//! - **B1 — checksum / tamper / orphan drift** ([`check_checksum_drift`](crate::check_checksum_drift)):
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
//! - **B2 — structural introspection** ([`snapshot_schema`](crate::snapshot_schema) +
//! [`diff_snapshots`]):
//! introspect the LIVE project schema into a deterministic [`SchemaSnapshot`]
//! and `diff` it against an **expected** snapshot the CALLER supplies. The
//! expected snapshot is owned by the control-plane / authoring layer (it holds
//! the declared/union schema, design); this module does NOT rebuild a schema
//! model by replaying DDL — that is the authoring layer's job. `diff_snapshots`
//! is a pure function returning a [`StructuralDrift`] report; it never returns
//! DDL.

use std::collections::BTreeMap;

use crate::apply::journal::{AppliedEntry, Phase};
use crate::model::ir::{
    IndexSortOrder, IndexStorageParams, SafeI64, SequenceOwnedBy, SequenceRef, TriggerEvent,
};
use crate::model::migration::Migration;
use crate::model::snapshot::{
    canonical_index_sort_order, index_elements_canonically_eq, index_predicates_canonically_eq,
    ColumnCollationSnapshot, ColumnSnapshot, ConstraintSnapshot, ExtensionSnapshot,
    FunctionIdentity, FunctionKey, GeneratedKindSnapshot, IdDefaultSnapshot, IndexElementSnapshot,
    IndexSnapshot, MysqlPhysicalType, PartitionSnapshot, PolicyIdentity, PolicyKey, RoleSnapshot,
    SchemaObjectSnapshot, SchemaSnapshot, SequenceSnapshot, TableSnapshot, TriggerIdentity,
    TriggerKey, VendorObjectIdentities,
};
use crate::render::value_format::{
    catalog_id_default, catalog_id_default_for_expected, catalog_text_id_default,
};

// ── The drift REPORT shapes moved down to the backend contract, whose drift
// queries return them. The COMPARISONS that build them — `compare_applied_to_set`,
// `diff_snapshots`, every per-vendor catalog normalization below — stay here.
// Re-exported so each `crate::apply::drift::…` path resolves unchanged.
pub use zero_migrate_backend::drift::{
    AlteredObject, ChecksumDrift, ChecksumDriftReport, DriftError, DriftReport, OrphanJournal,
    StructuralDrift,
};

// ---------------------------------------------------------------------------
// B1 — checksum / tamper / orphan drift
// ---------------------------------------------------------------------------

/// The **dialect-agnostic** core of [`check_checksum_drift`](crate::check_checksum_drift): compare a set of
/// net-applied journal entries (already read by the dialect-coupled `applied`)
/// against the supplied migration set, producing the [`ChecksumDriftReport`].
///
/// Extracted so EVERY [`MigrationBackend`](crate::apply::backend::MigrationBackend) impl
/// shares ONE comparison — the Postgres path and the SQLite path both call this
/// with their own `applied` read, so the repeatable-exemption / kind-mismatch /
/// tamper / orphan rules can never diverge across dialects (design: the
/// comparison is dialect-agnostic; only the journal read underneath differs).
///
/// Pure: no I/O. See [`check_checksum_drift`](crate::check_checksum_drift) for the per-rule rationale.
#[must_use]
pub fn compare_applied_to_set(
    applied: &[AppliedEntry],
    migrations: &[Migration],
) -> ChecksumDriftReport {
    let by_version: BTreeMap<&str, &Migration> =
        migrations.iter().map(|m| (m.version.as_str(), m)).collect();

    let mut report = ChecksumDriftReport::default();
    for entry in applied {
        // Only NET-applied (completed) versions can drift / be orphaned; a lone
        // `started` inflight marker is a crash-recovery key, not a settled state.
        if entry.phase != Phase::Completed {
            continue;
        }
        match by_version.get(entry.version.as_str()) {
            Some(m) => {
                // DRIFT EXEMPTION anchored on the JOURNALED
                // kind, NEVER on the attacker-suppliable `m.flags.repeatable`.
                //
                // A repeatable migration's checksum changes by DESIGN (a changed
                // `CREATE OR REPLACE …` re-runs each deploy), so a checksum mismatch
                // on a GENUINE repeatable is the re-run signal, not tamper. But the
                // ONLY trustworthy evidence that a version IS a repeatable is what the
                // journal recorded when it last applied (`kind='repeatable'`) — the
                // supplied flag is forgeable. So the exemption requires BOTH the
                // journaled kind AND the supplied flag to agree on "repeatable":
                //
                // - journaled `repeatable` AND supplied `repeatable=true` ⇒ EXEMPT
                // (the repeatable phase handles its re-apply);
                // - journaled once-only (apply/baseline/squash) but supplied
                // `repeatable=true` ⇒ KIND MISMATCH = TAMPER (the flip-flag attack:
                // turning an applied once-only into a repeatable to slip a mutated
                // `up` past the once-only abort) ⇒ ChecksumDrift / abort;
                // - journaled `repeatable` but supplied `repeatable=false` ⇒ reverse
                // re-classification (also a kind mismatch) ⇒ ChecksumDrift / abort;
                // - journaled once-only AND supplied once-only ⇒ the ordinary
                // once-only tamper guard (changed checksum still aborts).
                let journaled_repeatable = entry
                    .kind
                    .is_some_and(crate::apply::journal::JournaledKind::is_repeatable);
                let supplied_repeatable = m.flags.repeatable;
                if journaled_repeatable && supplied_repeatable {
                    // Legit repeatable re-run signal — exempt from the tamper abort.
                    continue;
                }
                if journaled_repeatable != supplied_repeatable {
                    // Kind mismatch: the supplied repeatability disagrees with the
                    // journaled identity-class. This is tamper (the flip-flag bypass
                    // or its reverse) — abort with ChecksumDrift regardless of whether
                    // the checksums happen to match, because the RE-CLASSIFICATION
                    // itself is the attack. Reuse ChecksumDrift so `apply` aborts on
                    // the shared gate; recorded vs expected carry the two checksums.
                    report.checksum_drift.push(ChecksumDrift {
                        version: entry.version.clone(),
                        recorded: entry.checksum.clone(),
                        expected: m.checksum.as_str().to_string(),
                    });
                    continue;
                }
                // Both once-only: the ordinary tamper guard.
                if entry.checksum != m.checksum.as_str() {
                    report.checksum_drift.push(ChecksumDrift {
                        version: entry.version.clone(),
                        recorded: entry.checksum.clone(),
                        expected: m.checksum.as_str().to_string(),
                    });
                }
            }
            None => report.orphan_journal.push(OrphanJournal {
                version: entry.version.clone(),
                recorded: entry.checksum.clone(),
            }),
        }
    }
    report
}

// ---------------------------------------------------------------------------
// B2 — structural introspection + pure diff
// ---------------------------------------------------------------------------

/// Compare ONE same-name child partition declared-vs-live, as `(field, expected,
/// actual)` triples in declaration order (`of` before `bounds`).
///
/// Hoisted out of [`diff_snapshots`] so the structural differ and the
/// existence-guard partition probe ([`crate::render::existence_probe::decide`])
/// share ONE definition of "the same partition": a second, drifting copy in the
/// probe is exactly how a guard and a drift report come to disagree about the same
/// catalog.
///
/// `bounds` equality is the derived `PartitionBounds` `PartialEq`, which is already
/// the canonical comparison: `snapshot_schema` parses `pg_get_expr` back into the
/// same enum, so an integer bound round-trips (PostgreSQL prints it unquoted). It
/// does NOT canonicalize literal SPELLING across types: a timestamptz bound
/// authored as `2026-05-01T00:00:00Z` and printed by the catalog as
/// `2026-05-01 00:00:00+00` compares unequal, which the probe reports as drift
/// rather than resolving.
pub(crate) fn partition_divergences(
    expected: &PartitionSnapshot,
    actual: &PartitionSnapshot,
) -> Vec<(&'static str, String, String)> {
    let mut out = Vec::new();
    if expected.of != actual.of {
        out.push(("of", expected.of.clone(), actual.of.clone()));
    }
    if expected.bounds != actual.bounds {
        out.push((
            "bounds",
            format!("{:?}", expected.bounds),
            format!("{:?}", actual.bounds),
        ));
    }
    out
}

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

fn parse_single_quoted_sql_string(input: &str) -> Option<String> {
    let mut chars = input.chars();
    if chars.next()? != '\'' {
        return None;
    }
    let mut out = String::new();
    while let Some(c) = chars.next() {
        if c == '\'' {
            match chars.next() {
                Some('\'') => out.push('\''),
                None => return Some(out),
                Some(_) => return None,
            }
        } else {
            out.push(c);
        }
    }
    None
}

/// The sequence a `nextval(...)` default names, or `None` when the expression is not
/// one.
///
/// SHARED rather than PostgreSQL-private, and the two callers are why. The PG
/// introspector reaches it to recover an ID default from `pg_get_expr`
/// (`backend::postgres::drift_sql::recover_nextval_default`), and the DIALECT-BLIND
/// differ reaches it through [`comparable_column_default`] — which runs for every
/// dialect, because the snapshot it is handed may have been produced by any of them.
/// A differ that could not read the spelling one producer emits would silently stop
/// comparing that producer's defaults, so the parse belongs to the shared vocabulary
/// even though only PostgreSQL writes the spelling.
pub(crate) fn parse_nextval_sequence_ref(expr: &str) -> Option<SequenceRef> {
    let expression = expr.trim();
    // pg_get_expr qualifies the built-in when a same-signature function earlier
    // on search_path would otherwise capture the deparsed spelling. The OID is
    // still proven through pg_depend below, so pg_catalog qualification is
    // catalog decoration rather than generator identity.
    let call = expression
        .strip_prefix("nextval(")
        .or_else(|| expression.strip_prefix("pg_catalog.nextval("))?;
    let inner = call.strip_suffix(')')?.trim();
    let literal = inner.strip_suffix("::regclass")?.trim();
    let regclass = parse_single_quoted_sql_string(literal)?;
    let (schema, name) = match regclass.split_once('.') {
        Some((schema, name)) if !schema.is_empty() && !name.is_empty() => {
            (Some(schema.to_string()), name.to_string())
        }
        None if !regclass.is_empty() => (None, regclass),
        _ => return None,
    };
    Some(SequenceRef { name, schema })
}

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
/// [`GeneratedColumnSnapshot::expr`] is RENDERED TEXT, and `fold_ops`'s
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
    mysql_expression_default: Option<bool>,
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
    // that same storage spelling. `mysql_expression_default` is the authoritative
    // literal-vs-expression bit; the authored side has none and does not need one,
    // because its text still carries its quotes.
    let key = if vendor
        .value_format
        .catalog_default_marker_is_authoritative()
    {
        catalog_text_id_default(Some(raw), dialect, mysql_expression_default)
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

fn format_identity(column: &ColumnSnapshot) -> &'static str {
    match (column.sqlite_rowid, column.identity) {
        (true, Some(identity)) if !identity.always => "sqlite autoincrement",
        (true, None) => "sqlite rowid",
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
    // A MySQL physical contract, when BOTH sides carry one, is the authority. The
    // portable `data_type` cannot be: `mysql_canonical_type` folds every `varchar(n)`
    // to the literal `text`, so a declared 255 and a live 64 are the same string here.
    //
    // Both sides, not either: a snapshot from another dialect carries none, and so
    // does one written before the contract existed. Comparing a contract against an
    // absent one would report a difference that says nothing about the database.
    if let (Some(expected_type), Some(actual_type)) =
        (&expected.mysql_physical_type, &actual.mysql_physical_type)
    {
        // An unmodelled family cannot ESTABLISH a difference, so this declines to
        // report one. That is the differ's safe direction and not a general rule -
        // an existence guard asking the same question must refuse to adopt instead,
        // because being wrong costs it a silently adopted column rather than a
        // missed drift line.
        if matches!(expected_type, MysqlPhysicalType::Unknown { .. })
            || matches!(actual_type, MysqlPhysicalType::Unknown { .. })
        {
            return true;
        }
        return expected_type == actual_type;
    }
    if expected.data_type == actual.data_type {
        return true;
    }
    if !(expected.sqlite_rowid && actual.sqlite_rowid) {
        return false;
    }

    // SQLite's rowid alias requires the physical declaration `INTEGER PRIMARY
    // KEY`, even when the portable authored integer width was bigint/smallint.
    // Preserve exact type drift everywhere else; this equivalence is confined to
    // two columns already proven to be the same rowid-alias contract.
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
/// The portable `data_type` cannot do that job on MySQL, and it fails in two different
/// directions. `fold_ops` emits the PostgreSQL `information_schema` spelling regardless
/// of dialect while the catalog side is folded through `mysql_canonical_type`, so a
/// `decimal(12, 2)` widened to `decimal(30, 10)` reported `expected: "numeric",
/// actual: "decimal"` - one type spelled two ways, naming nothing a reader can act on.
/// And when the two spellings COINCIDE - a live `TEXT` narrowed to `VARCHAR(64)` is
/// `"text"` on both sides - `diff_attrs`'s `push` dropped the entry entirely, because
/// its job is to skip fields whose two sides are equal. That is the case the physical
/// contract exists for, so the report was blind exactly where the comparator was not.
///
/// When BOTH sides carry a contract, the contract is what the comparator compared, so
/// the contract is what the report prints. Both sides, not either, for the same reason
/// [`column_data_types_eq`] gives: a contract against an absent one describes nothing
/// about the database, and a PostgreSQL or SQLite snapshot carries none - so those
/// dialects keep the portable spelling they always had.
fn column_data_type_report(expected: &ColumnSnapshot, actual: &ColumnSnapshot) -> (String, String) {
    let (Some(expected_type), Some(actual_type)) =
        (&expected.mysql_physical_type, &actual.mysql_physical_type)
    else {
        return (expected.data_type.clone(), actual.data_type.clone());
    };
    let expected_text = format_mysql_physical_type(expected_type);
    let actual_text = format_mysql_physical_type(actual_type);
    if expected_text != actual_text {
        return (expected_text, actual_text);
    }
    // Unreachable for every family `MysqlPhysicalType::parse` produces - each one
    // renders its distinguishing values below - but a collision here would re-lose the
    // difference through the very `push` guard this function exists to get past, which
    // is too quiet a failure to leave to inspection. The derived `Debug` prints every
    // field, so two values that are not equal cannot render the same.
    (format!("{expected_type:?}"), format!("{actual_type:?}"))
}

/// Spell one [`MysqlPhysicalType`] the way MySQL spells it, so a reader can take the
/// reported string straight to the server.
///
/// Round-trips through [`MysqlPhysicalType::parse`] for every family that function can
/// produce, which is what keeps the two sides of a report distinguishable: two
/// contracts that are not equal cannot render to the same text without the parse of
/// that text being wrong for one of them. `Spatial` is the one variant `parse` never
/// yields - the SRID comes from its own catalog column - so it is spelled for a human
/// rather than for the parser.
fn format_mysql_physical_type(physical: &MysqlPhysicalType) -> String {
    match physical {
        MysqlPhysicalType::Character { fixed, length } => {
            format!("{}({length})", if *fixed { "char" } else { "varchar" })
        }
        MysqlPhysicalType::Lob { tier } => tier.clone(),
        MysqlPhysicalType::Integer {
            kind,
            unsigned,
            boolean,
        } => {
            let width = if *boolean { "(1)" } else { "" };
            let sign = if *unsigned { " unsigned" } else { "" };
            format!("{kind}{width}{sign}")
        }
        MysqlPhysicalType::Decimal {
            precision,
            scale,
            unsigned,
        } => {
            let sign = if *unsigned { " unsigned" } else { "" };
            format!("decimal({precision},{scale}){sign}")
        }
        MysqlPhysicalType::Temporal { kind, fsp } => {
            // MySQL omits `(0)` entirely, and `parse` reads an absent precision as zero.
            if *fsp == 0 {
                kind.clone()
            } else {
                format!("{kind}({fsp})")
            }
        }
        MysqlPhysicalType::Members { kind, members } => {
            let members = members
                .iter()
                .map(|member| format!("'{}'", member.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(",");
            format!("{kind}({members})")
        }
        MysqlPhysicalType::Spatial { kind, srid } => match srid {
            Some(srid) => format!("{kind} srid {srid}"),
            None => kind.clone(),
        },
        MysqlPhysicalType::Plain { kind } => kind.clone(),
        MysqlPhysicalType::Unknown { raw } => raw.clone(),
    }
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
                    }) && ac.mysql_text_storage.is_some()
                    {
                        return catalog_text_id_default(
                            ac.default.as_deref(),
                            actual_dialect.expect("an actual vendor supplies its own dialect id"),
                            ac.mysql_default_generated,
                        );
                    }
                    catalog_id_default_for_expected(
                        expected_default,
                        ac.default.as_deref(),
                        actual_dialect,
                        ac.mysql_default_generated,
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
                    ac.mysql_default_generated,
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
/// Kept separate from
/// [`constraint_definition_is_retained`](crate::apply::backend::postgres::drift_sql::constraint_definition_is_retained)
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
mod mysql_physical_type_tests {
    use super::{column_data_types_eq, ColumnSnapshot, MysqlPhysicalType};

    /// Both sides fold to the portable `text` on MySQL, so `data_type` alone reports
    /// agreement. The contract is what tells them apart.
    fn column(data_type: &str, physical: Option<MysqlPhysicalType>) -> ColumnSnapshot {
        ColumnSnapshot {
            name: "c".to_string(),
            data_type: data_type.to_string(),
            mysql_physical_type: physical,
            ..Default::default()
        }
    }

    #[test]
    fn a_varchar_length_change_is_seen_where_the_portable_type_is_blind() {
        let expected = column("text", Some(MysqlPhysicalType::parse("varchar(255)")));
        let actual = column("text", Some(MysqlPhysicalType::parse("varchar(64)")));
        assert!(
            column_data_types_eq(&column("text", None), &column("text", None)),
            "without contracts the two are indistinguishable, which is the defect"
        );
        assert!(
            !column_data_types_eq(&expected, &actual),
            "with contracts the length change is a difference"
        );
    }

    #[test]
    fn the_renderer_spelling_and_the_catalog_spelling_agree() {
        // The renderer emits DECIMAL(65, 30); MySQL stores decimal(65,30). Reporting
        // drift on that pair would be a false red on a database nobody touched.
        let expected = column("numeric", Some(MysqlPhysicalType::parse("DECIMAL(65, 30)")));
        let actual = column("numeric", Some(MysqlPhysicalType::parse("decimal(65,30)")));
        assert!(column_data_types_eq(&expected, &actual));
    }

    #[test]
    fn one_side_without_a_contract_keeps_the_portable_comparison() {
        // A PostgreSQL or SQLite snapshot carries no contract, and neither does one
        // written before it existed. Comparing present against absent must not
        // manufacture a difference.
        let expected = column("text", Some(MysqlPhysicalType::parse("varchar(255)")));
        assert!(column_data_types_eq(&expected, &column("text", None)));
        assert!(column_data_types_eq(&column("text", None), &expected));
    }

    #[test]
    fn an_unmodelled_family_does_not_assert_a_difference_it_cannot_establish() {
        let expected = column("point", Some(MysqlPhysicalType::parse("point")));
        let actual = column("point", Some(MysqlPhysicalType::parse("geometry")));
        assert!(
            column_data_types_eq(&expected, &actual),
            "the differ declines rather than reporting a difference from two Unknowns"
        );
    }

    /// Every family `MysqlPhysicalType::parse` can produce, spelled so it parses back
    /// to itself.
    ///
    /// This is what makes the report FAITHFUL rather than merely non-empty: a reader
    /// can take the printed string to the server, and two contracts that are not equal
    /// cannot render to the same text without one of these round-trips failing.
    const PARSEABLE_SPELLINGS: &[&str] = &[
        "varchar(255)",
        "varchar(64)",
        "char(8)",
        "char(36)",
        "text",
        "tinytext",
        "mediumtext",
        "longtext",
        "blob",
        "longblob",
        "int",
        "int unsigned",
        "bigint",
        "bigint unsigned",
        "tinyint",
        "tinyint(1)",
        "smallint",
        "mediumint",
        "decimal(12,2)",
        "decimal(30,10)",
        "decimal(65,30)",
        "decimal(10,0) unsigned",
        "datetime",
        "datetime(3)",
        "datetime(6)",
        "timestamp",
        "timestamp(6)",
        "time(3)",
        "date",
        "year",
        "enum('a','b')",
        "enum('a, b','c''d')",
        "set('x','y')",
        "json",
        "double",
        "float",
        "bit",
    ];

    #[test]
    fn a_reported_contract_parses_back_to_the_contract_it_came_from() {
        for spelling in PARSEABLE_SPELLINGS {
            let physical = MysqlPhysicalType::parse(spelling);
            assert!(
                !matches!(physical, MysqlPhysicalType::Unknown { .. }),
                "{spelling} is meant to exercise a MODELLED family, but parsed as Unknown"
            );
            let printed = super::format_mysql_physical_type(&physical);
            assert_eq!(
                MysqlPhysicalType::parse(&printed),
                physical,
                "{spelling} printed as {printed:?}, which does not parse back to itself"
            );
        }
    }

    #[test]
    fn two_different_contracts_never_print_the_same_text() {
        // The whole point of the report change is to get past `push`, which drops an
        // entry whose two sides are equal strings. A spelling collision would put the
        // difference straight back in the hole it was just pulled out of.
        for (i, left) in PARSEABLE_SPELLINGS.iter().enumerate() {
            for right in &PARSEABLE_SPELLINGS[i + 1..] {
                let (left_type, right_type) = (
                    MysqlPhysicalType::parse(left),
                    MysqlPhysicalType::parse(right),
                );
                if left_type == right_type {
                    continue;
                }
                assert_ne!(
                    super::format_mysql_physical_type(&left_type),
                    super::format_mysql_physical_type(&right_type),
                    "{left} and {right} are different contracts that print the same text"
                );
            }
        }
    }

    #[test]
    fn a_dialect_without_a_contract_keeps_the_portable_spelling() {
        // PostgreSQL and SQLite leave `mysql_physical_type` as `None`, so their reports
        // must read exactly as they did before the contract could be printed.
        let (expected, actual) = super::column_data_type_report(
            &column("character varying(255)", None),
            &column("integer", None),
        );
        assert_eq!(expected, "character varying(255)");
        assert_eq!(actual, "integer");

        // One side only is still the portable spelling: a contract compared against an
        // absent one describes nothing about the database.
        let (expected, actual) = super::column_data_type_report(
            &column("text", Some(MysqlPhysicalType::parse("varchar(255)"))),
            &column("integer", None),
        );
        assert_eq!(expected, "text");
        assert_eq!(actual, "integer");
    }

    #[test]
    fn a_width_change_the_portable_type_cannot_see_reaches_the_report() {
        // The defect, at the unit boundary: both sides read `text`, so the report used
        // to print two equal strings and `diff_attrs`'s `push` dropped the entry.
        let (expected, actual) = super::column_data_type_report(
            &column("text", Some(MysqlPhysicalType::parse("text"))),
            &column("text", Some(MysqlPhysicalType::parse("varchar(64)"))),
        );
        assert_ne!(
            expected, actual,
            "a TEXT -> VARCHAR(64) narrowing must print two sides a reader can tell apart"
        );
        assert_eq!(expected, "text");
        assert_eq!(actual, "varchar(64)");
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
