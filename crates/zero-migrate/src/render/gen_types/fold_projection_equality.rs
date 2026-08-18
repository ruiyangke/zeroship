//! **The step 3 gate: every projection reproduces its walker, byte for byte.**
//!
//! `docs/proposals/single-fold-and-effects.md` section G step 3:
//!
//! > Write `fold(ops) -> SchemaModel` and the four projections. Gate: every projection
//! > must reproduce its current walker byte-for-byte on the step 1 corpus. Where they
//! > differ, the difference is a section B defect and is triaged individually.
//!
//! This file is that gate. It replays every stream the step 1 corpus records - and
//! every PREFIX of every stream - through both sides and compares the two canonical
//! texts BYTE FOR BYTE. The corpus's fixtures are reused rather than re-authored:
//! `differential_corpus` owns the streams and this file owns the comparison, so a
//! stream added there is measured here without being written twice.
//!
//! # Byte-for-byte, and what a byte is here
//!
//! Each side is canonicalised exactly as [`super::differential_corpus`] canonicalises
//! it: `{:#?}` for the three Rust-typed answers, `to_string_pretty` for the wire
//! `FieldDef` map. Comparing THE SAME text the corpus compares means a difference this
//! gate reports is a difference that file could have reported, and neither can hide
//! behind a comparator of its own.
//!
//! # Where they differ
//!
//! A difference is one of exactly two things, and the distinction is the point:
//!
//! * a bug in the fold, which is FIXED rather than recorded; or
//! * a defect in the walker, which is RECORDED with its evidence, because step 3 must
//!   not hold the single fold to a shipped bug. `docs/review-log.md` calls a corpus
//!   that only says "the walkers still do what they did" a bug-preservation machine,
//!   and this file inherits that rule from the one next to it.
//!
//! Every recorded row therefore states which side it believes and why. Nothing here
//! is allowed to say "they differ" and stop.
//!
//! # What this gate CANNOT see
//!
//! The fold fails CLOSED: it runs the structural catalog replay, so a stream that
//! replay refuses produces no projection at all. `authoring_tables_from_ops` and
//! `runtime_metadata_from_ops` apply no coherence gate and answer about such a stream
//! anyway - `differential_corpus`'s `ONLY_THE_FOLD_BACKED_WALKERS_FAIL_CLOSED` records
//! the same asymmetry. Those observations are counted separately as
//! [`Verdict::FoldRefused`] and are NOT equality evidence; the count is pinned so the
//! gate cannot quietly lose coverage by refusing more.

use std::collections::BTreeMap;

use super::differential_corpus::{
    parse, policy, read_golden, CASES, DIALECTS, SCHEMA, STEMS, STREAMS,
};
use super::{authoring_tables_from_ops, runtime_metadata_from_ops};
use crate::model::ir::Op;
use crate::model::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, IndexSnapshot, SchemaSnapshot, TableSnapshot,
};
use crate::render::fold::single_fold;
use crate::render::fold::{fold_ops, fold_to_field_defs};
use crate::SqlDialect;

/// The four projections, named for the walker each must reproduce.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Projection {
    /// `FoldedSchema::project_snapshot` against `fold_ops`.
    Snapshot,
    /// `FoldedSchema::project_field_defs` against `fold_to_field_defs`.
    FieldDefs,
    /// `FoldedSchema::project_authoring_tables` against `authoring_tables_from_ops`.
    AuthoringTables,
    /// `FoldedSchema::project_runtime_metadata` against `runtime_metadata_from_ops`.
    RuntimeMetadata,
}

const PROJECTIONS: [Projection; 4] = [
    Projection::Snapshot,
    Projection::FieldDefs,
    Projection::AuthoringTables,
    Projection::RuntimeMetadata,
];

impl Projection {
    fn label(self) -> &'static str {
        match self {
            Projection::Snapshot => "snapshot",
            Projection::FieldDefs => "field_defs",
            Projection::AuthoringTables => "authoring_tables",
            Projection::RuntimeMetadata => "runtime_metadata",
        }
    }
}

/// One walker's answer, canonicalised. `Err` is the walker's refusal message.
type Answer = Result<String, String>;

fn walker_answer(
    projection: Projection,
    ops: &[Op],
    dialect: SqlDialect,
    confined: bool,
) -> Answer {
    let effective = policy(confined);
    match projection {
        Projection::Snapshot => fold_ops(ops, dialect, SCHEMA, &effective)
            .map(|v| format!("{v:#?}"))
            .map_err(|e| e.to_string()),
        Projection::FieldDefs => fold_to_field_defs(ops, dialect, SCHEMA, &effective)
            .map(|v| serde_json::to_string_pretty(&v).expect("field defs serialize"))
            .map_err(|e| e.to_string()),
        Projection::AuthoringTables => authoring_tables_from_ops(ops, dialect)
            .map(|v| format!("{v:#?}"))
            .map_err(|e| e.to_string()),
        Projection::RuntimeMetadata => runtime_metadata_from_ops(ops, dialect)
            .map(|v| format!("{v:#?}"))
            .map_err(|e| e.to_string()),
    }
}

fn projection_answer(
    projection: Projection,
    folded: &Result<single_fold::FoldedSchema, String>,
) -> Answer {
    let folded = match folded {
        Ok(folded) => folded,
        Err(error) => return Err(error.clone()),
    };
    Ok(match projection {
        Projection::Snapshot => format!("{:#?}", folded.project_snapshot()),
        Projection::FieldDefs => serde_json::to_string_pretty(&folded.project_field_defs())
            .expect("field defs serialize"),
        Projection::AuthoringTables => format!("{:#?}", folded.project_authoring_tables()),
        Projection::RuntimeMetadata => format!("{:#?}", folded.project_runtime_metadata()),
    })
}

/// How one comparison came out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Both produced an answer and the two texts are byte-identical.
    Equal,
    /// Both produced an answer and the texts differ.
    Differs,
    /// Both refused. No equality evidence, and no disagreement either.
    BothRefused,
    /// The fold refused a stream the walker answered about. Counted, never equality
    /// evidence. Only reachable for the two walkers that apply no coherence gate.
    FoldRefused,
    /// The walker refused a stream the fold answered about. Would mean the fold is
    /// LESS fail-closed than the walker, which is the dangerous direction.
    WalkerRefused,
}

/// The first line at which two canonical texts diverge, with BOTH spellings.
///
/// Both sides are printed because a one-sided report is unreadable: "the fold said
/// `case_sensitive: None`" says nothing until the walker's `case_sensitive: Some(false)`
/// is beside it. Truncated per line so a 4,000-line snapshot diff still fits in a
/// failure message.
fn first_difference(mine: &str, theirs: &str) -> String {
    let mut mine_lines = mine.lines();
    let mut theirs_lines = theirs.lines();
    let mut line = 0usize;
    loop {
        line += 1;
        match (mine_lines.next(), theirs_lines.next()) {
            (None, None) => return "texts are equal".to_string(),
            (a, b) if a == b => {}
            (a, b) => {
                let cut = |v: Option<&str>| {
                    let text = v.unwrap_or("<end of text>").trim();
                    if text.len() > 120 {
                        format!("{}…", &text[..120])
                    } else {
                        text.to_string()
                    }
                };
                return format!("line {line}: fold {:?} walker {:?}", cut(a), cut(b));
            }
        }
    }
}

/// Every corpus stream, named, with the charter it folds under.
///
/// The 27 recorded op fixtures are real drained envelopes and are already
/// policy-resolved, so they fold under the confined charter that produced them; the
/// authored vehicles fold under the no-inject charter. Both rules are the ones
/// `differential_corpus::measured_reach` uses, restated here because this file drives
/// the same streams through a different comparison.
fn corpus_streams() -> Vec<(String, Vec<Op>, bool)> {
    let mut out = Vec::new();
    for stem in STEMS {
        out.push((format!("g_{stem}"), read_golden(stem).ops, true));
    }
    for stream in STREAMS.iter().chain(CASES) {
        out.push((stream.name.to_string(), parse(stream.ops), false));
    }
    out
}

/// One aggregated observation: `stream|Dialect|projection`, over every PREFIX.
struct Measured {
    key: String,
    equal: usize,
    differs: usize,
    both_refused: usize,
    fold_refused: usize,
    walker_refused: usize,
    first_difference: Option<(usize, String)>,
}

fn measure() -> Vec<Measured> {
    let mut out = Vec::new();
    for (name, ops, confined) in corpus_streams() {
        for dialect in DIALECTS {
            let effective = policy(confined);
            // ONE fold per prefix, four projections read from it. Folding once per
            // projection would be four traversals, which is the thing being removed.
            let folded: Vec<Result<single_fold::FoldedSchema, String>> = (0..=ops.len())
                .map(|i| {
                    single_fold::fold(&ops[..i], dialect, SCHEMA, &effective)
                        .map_err(|e| e.to_string())
                })
                .collect();
            for projection in PROJECTIONS {
                let mut measured = Measured {
                    key: format!("{name}|{dialect:?}|{}", projection.label()),
                    equal: 0,
                    differs: 0,
                    both_refused: 0,
                    fold_refused: 0,
                    walker_refused: 0,
                    first_difference: None,
                };
                for (i, folded) in folded.iter().enumerate() {
                    let theirs = walker_answer(projection, &ops[..i], dialect, confined);
                    let mine = projection_answer(projection, folded);
                    let verdict = match (&mine, &theirs) {
                        (Ok(mine), Ok(theirs)) if mine == theirs => Verdict::Equal,
                        (Ok(_), Ok(_)) => Verdict::Differs,
                        (Err(_), Err(_)) => Verdict::BothRefused,
                        (Err(_), Ok(_)) => Verdict::FoldRefused,
                        (Ok(_), Err(_)) => Verdict::WalkerRefused,
                    };
                    match verdict {
                        Verdict::Equal => measured.equal += 1,
                        Verdict::Differs => {
                            measured.differs += 1;
                            if measured.first_difference.is_none() {
                                let detail = first_difference(
                                    mine.as_ref().expect("ok"),
                                    theirs.as_ref().expect("ok"),
                                );
                                measured.first_difference = Some((i, detail));
                            }
                        }
                        Verdict::BothRefused => measured.both_refused += 1,
                        Verdict::FoldRefused => measured.fold_refused += 1,
                        Verdict::WalkerRefused => measured.walker_refused += 1,
                    }
                }
                out.push(measured);
            }
        }
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

// ---------------------------------------------------------------------------
// The recorded divergences
// ---------------------------------------------------------------------------

/// Which side a recorded divergence believes. There is no "unknown" arm on purpose:
/// a difference this gate cannot attribute is a difference nobody has triaged, and
/// step 3 says every one is triaged individually.
#[derive(Clone, Copy)]
enum Side {
    /// The WALKER is wrong and the projection is right. The fold must not be held to
    /// it, and the evidence is stated so the claim can be argued with.
    WalkerDefect(&'static str),
    /// Neither is wrong: the two answers are equally true statements about different
    /// things, and the reason says which.
    ///
    /// Currently UNCONSTRUCTED. The variant stays because both recorded divergences
    /// happen to be walker defects, and a gate that deletes the "neither is wrong"
    /// vocabulary the moment nothing needs it has nowhere honest to put the next
    /// difference - which is the same reasoning `differential_corpus::Status::Defect`
    /// records for its own unconstructed arm, in the mirror direction.
    #[expect(
        dead_code,
        reason = "the by-design vocabulary outlives the current divergence set; see the \
                  doc comment"
    )]
    ByDesign(&'static str),
}

/// One recorded divergence, `stream|Dialect|projection`.
struct Divergence {
    key: &'static str,
    /// The first prefix length at which the two texts differ.
    at: usize,
    /// The first differing line, with BOTH spellings, verbatim from the measurement.
    detail: &'static str,
    side: Side,
}

/// `authoring_tables_from_ops` does not handle `Op::AlterPrimaryKey` at all - it falls
/// through the walker's `_ => {}`, measured `S` for ATO in the step 1 reach matrix
/// (`alterPrimaryKey|Postgres|RRSS`). So `env.db.ts` keeps declaring the primary key
/// the migration replaced or dropped, and `render_table` reads `primary_key`
/// (`gen_types.rs:1162-1195`) to emit it. `fold_to_field_defs` DOES handle the op (it
/// clears the identity facet), so one `renderArtifacts` call emits two artifacts that
/// disagree about the same table - which is section B row 2's shape exactly, on an op
/// nobody had looked at.
const ATO_IGNORES_ALTER_PRIMARY_KEY: &str =
    "authoring_tables_from_ops has no Op::AlterPrimaryKey arm, so env.db.ts keeps the \
     pre-alter primary key and the pre-alter identity facet. The projection follows the \
     op, because a model that ignored it would be stating a key the database does not \
     have. Measured on the alter_primary_key fixture and on v_primary_key; \
     docs/review-log.md has no entry for this one, so this gate is its first record.";

/// `fold_to_field_defs` lifts a single-column `UNIQUE` onto the column descriptor and
/// never un-lifts it. The FK half of exactly this lift IS un-lifted - the walker's
/// `Op::DropConstraint` arm exists and its comment says why ("the policy outlived the
/// constraint and gen-types kept emitting an ON DELETE the database no longer has") -
/// but the arm only touches `fks`, so the uniqueness the same `dropConstraint` removed
/// survives in the artifact. One fix, one instance, the sibling left open: the F113
/// pattern the review log names.
const FFD_NEVER_UNLIFTS_A_DROPPED_UNIQUE: &str =
    "fold_to_field_defs keeps `unique: true` on a column whose UNIQUE constraint the \
     same stream dropped. fold_ops - the catalog oracle the live suites anchor - has \
     already removed the constraint at that prefix, so the two disagree about the \
     database. The projection derives uniqueness from the constraints the model still \
     holds, so it cannot outlive one. Measured by \
     `a_dropped_unique_constraint_outlives_itself_in_the_field_def_map`; \
     docs/review-log.md has no entry for this one, so this gate is its first record.";

/// The recorded set. EMPTY is not the goal and a shrinking count is not automatically
/// progress - see `the_gate_has_the_shape_it_claims`.
#[rustfmt::skip]
const DIVERGENCES: &[Divergence] = &[
    // `[createTable(users), createIndex, addConstraint UNIQUE(email), validateConstraint,
    //  dropConstraint]`. The walker's descriptor still carries `unique`, so the
    // projection's object ends one key earlier and the first differing line is the one
    // BEFORE it - which is why the detail reads `"required": true` against
    // `"required": true,`. Both spellings are shown verbatim rather than paraphrased.
    Divergence { key: "v_index_and_constraint|Postgres|field_defs", at: 5, detail: "line 9: fold \"\\\"required\\\": true\" walker \"\\\"required\\\": true,\"", side: Side::WalkerDefect(FFD_NEVER_UNLIFTS_A_DROPPED_UNIQUE) },
    Divergence { key: "v_index_and_constraint|Sqlite|field_defs", at: 5, detail: "line 9: fold \"\\\"required\\\": true\" walker \"\\\"required\\\": true,\"", side: Side::WalkerDefect(FFD_NEVER_UNLIFTS_A_DROPPED_UNIQUE) },
    Divergence { key: "v_index_and_constraint|Mysql|field_defs", at: 5, detail: "line 9: fold \"\\\"required\\\": true\" walker \"\\\"required\\\": true,\"", side: Side::WalkerDefect(FFD_NEVER_UNLIFTS_A_DROPPED_UNIQUE) },

    // `[createTable(orders PRIMARY KEY id), alterPrimaryKey replace -> legacy_id]`.
    Divergence { key: "v_primary_key|Postgres|authoring_tables", at: 2, detail: "line 45: fold \"\\\"legacy_id\\\",\" walker \"\\\"id\\\",\"", side: Side::WalkerDefect(ATO_IGNORES_ALTER_PRIMARY_KEY) },
    Divergence { key: "v_primary_key|Sqlite|authoring_tables", at: 2, detail: "line 45: fold \"\\\"legacy_id\\\",\" walker \"\\\"id\\\",\"", side: Side::WalkerDefect(ATO_IGNORES_ALTER_PRIMARY_KEY) },
    Divergence { key: "v_primary_key|Mysql|authoring_tables", at: 2, detail: "line 45: fold \"\\\"legacy_id\\\",\" walker \"\\\"id\\\",\"", side: Side::WalkerDefect(ATO_IGNORES_ALTER_PRIMARY_KEY) },
];

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

#[test]
fn every_projection_reproduces_its_walker_byte_for_byte() {
    let measured = measure();
    let recorded: BTreeMap<&str, &Divergence> = DIVERGENCES.iter().map(|d| (d.key, d)).collect();
    assert_eq!(
        recorded.len(),
        DIVERGENCES.len(),
        "the recorded divergence keys are unique"
    );

    let mut report = String::new();
    // A recorded key that names no measured row at all - a typo, or a stream renamed
    // out from under it - would otherwise pass every branch below by matching nothing.
    let keys: std::collections::BTreeSet<&str> =
        measured.iter().map(|row| row.key.as_str()).collect();
    for key in recorded.keys() {
        if !keys.contains(key) {
            report.push_str(&format!(
                "  RECORDED, NOT MEASURED AT ALL: {key} names no stream this gate runs\n"
            ));
        }
    }
    for row in &measured {
        match (row.differs, recorded.get(row.key.as_str())) {
            (0, None) => {}
            (0, Some(_)) => {
                report.push_str(&format!(
                    "  RECORDED, NOT MEASURED: {} no longer differs\n",
                    row.key
                ));
            }
            (_, None) => {
                let (at, detail) = row
                    .first_difference
                    .as_ref()
                    .expect("a differing row has a first difference");
                report.push_str(&format!(
                    "  MEASURED, NOT RECORDED: {} differs on {} of {} prefixes; \
                     first after {at} ops -> {detail}\n",
                    row.key,
                    row.differs,
                    row.equal + row.differs
                ));
            }
            (_, Some(divergence)) => {
                let (at, detail) = row
                    .first_difference
                    .as_ref()
                    .expect("a differing row has a first difference");
                if *at != divergence.at || detail != divergence.detail {
                    report.push_str(&format!(
                        "  MOVED: {} first difference is now after {at} ops -> {detail}\n\
                         \t(recorded: after {} ops -> {})\n",
                        row.key, divergence.at, divergence.detail
                    ));
                }
            }
        }
    }
    assert!(
        report.is_empty(),
        "the step 3 byte-for-byte gate moved:\n{report}"
    );
}

/// A recorded divergence must say which side it believes, and say why.
#[test]
fn every_recorded_divergence_names_the_side_it_believes() {
    for divergence in DIVERGENCES {
        let (Side::WalkerDefect(why) | Side::ByDesign(why)) = divergence.side;
        assert!(
            !why.trim().is_empty(),
            "{}: a recorded divergence states its reason",
            divergence.key
        );
        assert!(
            divergence.detail.contains("fold ") && divergence.detail.contains("walker "),
            "{}: the recorded detail shows BOTH spellings, so a reader can see what \
             each side said rather than being told one of them is wrong",
            divergence.key
        );
    }
}

/// The gate's own shape, pinned. A gate that quietly stops comparing still passes
/// every assertion above, because every assertion above compares the measurement
/// against itself. This one says how much was compared.
#[test]
fn the_gate_has_the_shape_it_claims() {
    let measured = measure();
    let total = |f: fn(&Measured) -> usize| measured.iter().map(f).sum::<usize>();

    assert_eq!(
        measured.len(),
        (STEMS.len() + STREAMS.len() + CASES.len()) * DIALECTS.len() * PROJECTIONS.len(),
        "one aggregated row per stream, dialect and projection"
    );
    assert_eq!(
        total(|m| m.walker_refused),
        0,
        "a walker refusing a stream the fold accepts would mean the single fold is \
         LESS fail-closed than the walker it replaces, which is the dangerous \
         direction and is not a difference this gate may record away"
    );
    // ONE assertion over all four counts, so a run that moves two of them prints both
    // rather than stopping at the first and hiding the second.
    assert_eq!(
        (
            total(|m| m.equal),
            total(|m| m.differs),
            total(|m| m.both_refused),
            total(|m| m.fold_refused),
        ),
        (
            EQUAL_COMPARISONS,
            DIFFERING_COMPARISONS,
            BOTH_REFUSED,
            FOLD_REFUSED,
        ),
        "(byte-identical, differing, both-refused, fold-refused). PINNED, not bounded: \
         any move is a change a reviewer should see. EQUAL is the gate's coverage, and \
         a FALL in it specifically means a stream was deleted or the fold started \
         refusing something it used to answer - neither of which reads as a failure \
         anywhere else. FOLD-REFUSED counts the prefixes this gate did NOT get to \
         compare, because the fold fails closed and the two artifact walkers do not"
    );
}

/// Byte-identical comparisons across the whole corpus.
const EQUAL_COMPARISONS: usize = 2720;
/// Comparisons whose two texts differ. Every one is attributed in [`DIVERGENCES`].
const DIFFERING_COMPARISONS: usize = 12;
/// Prefixes both sides refuse.
const BOTH_REFUSED: usize = 392;
/// Prefixes the fold refuses and a walker answers about.
const FOLD_REFUSED: usize = 392;

// ---------------------------------------------------------------------------
// The per-field probe
// ---------------------------------------------------------------------------

/// Compare two snapshots FIELD BY FIELD and return every field name that differs.
///
/// Not `==`. A whole-object comparison stops at the first difference, so a probe
/// written that way measures the FIRST field and nothing after it -
/// `docs/review-log.md` records a behaviour-preservation proof that passed while a
/// comparator field was deleted, because two distinct objects always differ in `name`
/// and no later term ever executed. Every field is compared here, and every one that
/// differs is named.
///
/// The destructuring is EXHAUSTIVE with no `..`: a field added to any of these four
/// snapshot types is a compile error until it is routed, which is the same mechanism
/// `tests/support/carriers.rs` uses to prove a carrier inventory complete.
fn column_field_differences(mine: &ColumnSnapshot, theirs: &ColumnSnapshot) -> Vec<&'static str> {
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
    } = mine;
    let mut out = Vec::new();
    let mut check = |field: &'static str, equal: bool| {
        if !equal {
            out.push(field);
        }
    };
    check("name", *name == theirs.name);
    check("data_type", *data_type == theirs.data_type);
    check("nullable", *nullable == theirs.nullable);
    check("default", *default == theirs.default);
    check(
        "ddl_type_override",
        *ddl_type_override == theirs.ddl_type_override,
    );
    check("inline_checks", *inline_checks == theirs.inline_checks);
    check("generated", *generated == theirs.generated);
    check("generated_kind", *generated_kind == theirs.generated_kind);
    check("identity", *identity == theirs.identity);
    check("sqlite_rowid", *sqlite_rowid == theirs.sqlite_rowid);
    check("value_format", *value_format == theirs.value_format);
    check(
        "catalog_uuid_format_check",
        *catalog_uuid_format_check == theirs.catalog_uuid_format_check,
    );
    check("id_default", *id_default == theirs.id_default);
    check(
        "mysql_default_generated",
        *mysql_default_generated == theirs.mysql_default_generated,
    );
    check("case_sensitive", *case_sensitive == theirs.case_sensitive);
    check("collation", *collation == theirs.collation);
    check(
        "mysql_text_storage",
        *mysql_text_storage == theirs.mysql_text_storage,
    );
    check(
        "mysql_physical_type",
        *mysql_physical_type == theirs.mysql_physical_type,
    );
    check(
        "encryption_sentinel",
        *encryption_sentinel == theirs.encryption_sentinel,
    );
    check(
        "comment_sentinel",
        *comment_sentinel == theirs.comment_sentinel,
    );
    check("comment", *comment == theirs.comment);
    out
}

fn index_field_differences(mine: &IndexSnapshot, theirs: &IndexSnapshot) -> Vec<&'static str> {
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
    } = mine;
    let mut out = Vec::new();
    let mut check = |field: &'static str, equal: bool| {
        if !equal {
            out.push(field);
        }
    };
    check("name", *name == theirs.name);
    check("unique", *unique == theirs.unique);
    check("columns", *columns == theirs.columns);
    check("elements", *elements == theirs.elements);
    check("access_method", *access_method == theirs.access_method);
    check("predicate", *predicate == theirs.predicate);
    check("include", *include == theirs.include);
    check("with", *with == theirs.with);
    check("only", *only == theirs.only);
    check("opclass", *opclass == theirs.opclass);
    check(
        "nulls_not_distinct",
        *nulls_not_distinct == theirs.nulls_not_distinct,
    );
    check("comment", *comment == theirs.comment);
    check(
        "expr_cascade_columns",
        *expr_cascade_columns == theirs.expr_cascade_columns,
    );
    out
}

fn constraint_field_differences(
    mine: &ConstraintSnapshot,
    theirs: &ConstraintSnapshot,
) -> Vec<&'static str> {
    let ConstraintSnapshot {
        name,
        kind,
        definition,
        comment,
        cascade_columns,
    } = mine;
    let mut out = Vec::new();
    let mut check = |field: &'static str, equal: bool| {
        if !equal {
            out.push(field);
        }
    };
    check("name", *name == theirs.name);
    check("kind", *kind == theirs.kind);
    check("definition", *definition == theirs.definition);
    check("comment", *comment == theirs.comment);
    check(
        "cascade_columns",
        *cascade_columns == theirs.cascade_columns,
    );
    out
}

fn table_field_differences(mine: &TableSnapshot, theirs: &TableSnapshot) -> Vec<String> {
    let TableSnapshot {
        columns,
        indexes,
        constraints,
        runtime_options,
        partition_by,
        comment,
        stored_create_sql,
    } = mine;
    let mut out = Vec::new();
    if columns.len() != theirs.columns.len() {
        out.push("columns.len".to_string());
    }
    for (mine, theirs) in columns.iter().zip(&theirs.columns) {
        for field in column_field_differences(mine, theirs) {
            out.push(format!("column.{field}"));
        }
    }
    if indexes.len() != theirs.indexes.len() {
        out.push("indexes.len".to_string());
    }
    for (mine, theirs) in indexes.iter().zip(&theirs.indexes) {
        for field in index_field_differences(mine, theirs) {
            out.push(format!("index.{field}"));
        }
    }
    if constraints.len() != theirs.constraints.len() {
        out.push("constraints.len".to_string());
    }
    for (mine, theirs) in constraints.iter().zip(&theirs.constraints) {
        for field in constraint_field_differences(mine, theirs) {
            out.push(format!("constraint.{field}"));
        }
    }
    if *runtime_options != theirs.runtime_options {
        out.push("runtime_options".to_string());
    }
    if *partition_by != theirs.partition_by {
        out.push("partition_by".to_string());
    }
    if *comment != theirs.comment {
        out.push("comment".to_string());
    }
    if *stored_create_sql != theirs.stored_create_sql {
        out.push("stored_create_sql".to_string());
    }
    out
}

/// **The per-field probe.** Projection 1 routes every table through the neutral /
/// vendor split, so this asserts, per FIELD, that the split loses nothing on
/// FOLD-PRODUCED shapes.
///
/// The existing equivalence suites (`tests/schema_model_equivalence_pg.rs`,
/// `..._mysql.rs`) only ever ran the split on LIVE-INTROSPECTED snapshots. An
/// introspected snapshot populates a different set of vendor families from an authored
/// one - `catalog_uuid_format_check` and `mysql_text_storage` are introspection-only,
/// and `ddl_type_override` and `encryption_sentinel` are author-only - so this is the
/// first measurement of the split on the half of the input space that authoring
/// produces.
#[test]
fn the_neutral_vendor_split_loses_no_field_on_a_folded_shape() {
    let mut differences: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut compared = 0usize;
    for (name, ops, confined) in corpus_streams() {
        for dialect in DIALECTS {
            let effective = policy(confined);
            let Ok(folded) = single_fold::fold(&ops, dialect, SCHEMA, &effective) else {
                continue;
            };
            let Ok(walker) = fold_ops(&ops, dialect, SCHEMA, &effective) else {
                continue;
            };
            let projected: SchemaSnapshot = folded.project_snapshot();
            for (table, theirs) in &walker.tables {
                let Some(mine) = projected.tables.get(table) else {
                    differences
                        .entry(format!("{name}|{dialect:?}|{table}"))
                        .or_default()
                        .push("<table missing from the projection>".to_string());
                    continue;
                };
                compared += 1;
                let fields = table_field_differences(mine, theirs);
                if !fields.is_empty() {
                    differences
                        .entry(format!("{name}|{dialect:?}|{table}"))
                        .or_default()
                        .extend(fields);
                }
            }
        }
    }
    assert!(
        differences.is_empty(),
        "the neutral/vendor split dropped a field on a folded shape: {differences:#?}"
    );
    assert_eq!(
        compared, TABLES_PROBED_PER_FIELD,
        "tables compared field by field. Pinned, not bounded: a FALL means the probe \
         stopped looking, which is indistinguishable from a green run unless the number \
         is stated"
    );
}

/// Tables compared field by field by the probe above.
const TABLES_PROBED_PER_FIELD: usize = 97;

// ---------------------------------------------------------------------------
// The evidence behind the two recorded divergences
// ---------------------------------------------------------------------------

/// One corpus stream by name, so the evidence below drives the SAME ops the gate
/// measured rather than a hand-written lookalike that could drift from it.
fn corpus_stream(name: &str) -> Vec<Op> {
    STREAMS
        .iter()
        .chain(CASES)
        .find(|stream| stream.name == name)
        .map(|stream| parse(stream.ops))
        .unwrap_or_else(|| panic!("the step 1 corpus has no stream named {name}"))
}

/// **Evidence for [`FFD_NEVER_UNLIFTS_A_DROPPED_UNIQUE`].**
///
/// The anchor is `fold_ops`, not this file's opinion: it is the structural oracle the
/// live PostgreSQL, SQLite and MySQL suites already run against a real server, and at
/// this prefix it has removed the constraint. So the artifact and the catalog disagree
/// about whether the column is unique, and the artifact is the one describing a
/// database that does not exist.
#[test]
fn a_dropped_unique_constraint_outlives_itself_in_the_field_def_map() {
    let stream = corpus_stream("v_index_and_constraint");
    // createTable, createIndex, addConstraint UNIQUE(email), validateConstraint,
    // dropConstraint - the prefix that ADDS and then DROPS the uniqueness.
    let ops = &stream[..5];
    let effective = policy(false);

    let catalog = fold_ops(ops, SqlDialect::Postgres, SCHEMA, &effective).expect("fold_ops");
    assert!(
        !catalog.tables["users"]
            .constraints
            .iter()
            .any(|constraint| constraint.name == "users_email_uq"),
        "the catalog oracle must agree the constraint is gone, or this row is not \
         evidence of anything"
    );

    let defs = fold_to_field_defs(ops, SqlDialect::Postgres, SCHEMA, &effective)
        .expect("fold_to_field_defs");
    assert_eq!(
        defs["users"]["email"].get("unique"),
        Some(&serde_json::json!(true)),
        "TODAY: the FieldDef map still calls the column unique after its constraint \
         was dropped"
    );

    let folded = single_fold::fold(ops, SqlDialect::Postgres, SCHEMA, &effective).expect("fold");
    assert_eq!(
        folded.project_field_defs()["users"]["email"].get("unique"),
        None,
        "the projection derives uniqueness from the constraints the model still holds, \
         so a dropped constraint cannot outlive itself"
    );
}

/// **The same un-lift hole, on a facet the step 1 corpus cannot see.**
///
/// The gate above found the UNIQUE half because `v_index_and_constraint` adds and then
/// drops one. No corpus stream drops a CHECK, so the gate is blind to the `min`/`max`
/// half of the same hole - which is stated here rather than left for the next reader
/// to rediscover, because "the gate is green" is only as strong as what the gate can
/// see. The projection derives both from the constraints the model still holds, so one
/// rule covers the whole family instead of one arm per facet.
#[test]
fn a_dropped_check_constraint_outlives_itself_in_the_field_def_map() {
    let ops: Vec<Op> = parse(
        r#"[
  {"op":"createTable","name":"scores","columns":[{"name":"id","type":"text","nullable":false},{"name":"score","type":"int"}],"primaryKey":["id"]},
  {"op":"addConstraint","table":"scores","constraint":{"name":"scores_score_ck","kind":{"kind":"check","expr":{"node":"binOp","op":"and","lhs":{"node":"binOp","op":"ge","lhs":{"node":"colRef","name":"score"},"rhs":{"node":"literal","value":1}},"rhs":{"node":"binOp","op":"le","lhs":{"node":"colRef","name":"score"},"rhs":{"node":"literal","value":9}}}}}},
  {"op":"dropConstraint","table":"scores","name":"scores_score_ck"}
]"#,
    );
    let effective = policy(false);

    // The bound is really recovered while the constraint is live, or the drop below
    // proves nothing.
    let with_check = fold_to_field_defs(&ops[..2], SqlDialect::Postgres, SCHEMA, &effective)
        .expect("fold_to_field_defs");
    assert_eq!(
        with_check["scores"]["score"].get("min"),
        Some(&serde_json::json!(1.0)),
        "the CHECK's lower bound reaches the FieldDef map while the constraint exists"
    );

    let catalog = fold_ops(&ops, SqlDialect::Postgres, SCHEMA, &effective).expect("fold_ops");
    assert!(
        !catalog.tables["scores"]
            .constraints
            .iter()
            .any(|constraint| constraint.name == "scores_score_ck"),
        "the catalog oracle must agree the constraint is gone"
    );

    let defs =
        fold_to_field_defs(&ops, SqlDialect::Postgres, SCHEMA, &effective).expect("field defs");
    assert_eq!(
        defs["scores"]["score"].get("min"),
        Some(&serde_json::json!(1.0)),
        "TODAY: the recovered min/max outlives the CHECK that granted it"
    );

    let folded = single_fold::fold(&ops, SqlDialect::Postgres, SCHEMA, &effective).expect("fold");
    let projected = folded.project_field_defs();
    assert_eq!(
        projected["scores"]["score"].get("min"),
        None,
        "the projection reads the constraints the model still holds, so the bound goes \
         when the constraint does"
    );
    assert_eq!(
        projected["scores"]["score"].get("max"),
        None,
        "and the upper bound with it"
    );
}

/// **Evidence for [`ATO_IGNORES_ALTER_PRIMARY_KEY`], including the shipped artifact.**
///
/// What the server does after an exact-order `alterPrimaryKey` replace is already
/// live-proven in `tests/pg_primary_key.rs`
/// (`replace_accepts_only_exact_order_and_supports_single_composite_round_trip`, which
/// introspects `primary_key_columns` off a real PostgreSQL). This test does not
/// re-prove it; it shows that `env.db.ts` - the artifact an app is regenerated from -
/// still declares the key the replace removed.
#[test]
fn an_altered_primary_key_never_reaches_the_authoring_artifact() {
    let stream = corpus_stream("v_primary_key");
    // createTable(orders, PRIMARY KEY (id)), alterPrimaryKey replace -> (legacy_id)
    let ops = &stream[..2];
    let effective = policy(false);

    let catalog = fold_ops(ops, SqlDialect::Postgres, SCHEMA, &effective).expect("fold_ops");
    let primary_key = catalog.tables["orders"]
        .constraints
        .iter()
        .find(|constraint| constraint.kind == "PRIMARY KEY")
        .map(|constraint| constraint.definition.clone())
        .expect("the catalog oracle carries a primary key");
    assert!(
        primary_key.contains("legacy_id") && !primary_key.contains("\"id\""),
        "the catalog oracle must have moved the key, or this row is not evidence of \
         anything: {primary_key}"
    );

    let walker = authoring_tables_from_ops(ops, SqlDialect::Postgres).expect("authoring tables");
    assert_eq!(
        walker["orders"].primary_key.as_deref(),
        Some(["id".to_string()].as_slice()),
        "TODAY: the authoring tables still declare the REPLACED key"
    );

    let folded = single_fold::fold(ops, SqlDialect::Postgres, SCHEMA, &effective).expect("fold");
    assert_eq!(
        folded.project_authoring_tables()["orders"]
            .primary_key
            .as_deref(),
        Some(["legacy_id".to_string()].as_slice()),
        "the projection follows the op, because a model that ignored it would state a \
         key the database does not have"
    );

    // The shipped consequence, not just the intermediate. `env.db.ts` is what an app
    // is regenerated from.
    let artifacts = super::render_artifacts(ops, SqlDialect::Postgres, SCHEMA, &effective)
        .expect("render artifacts");
    // A single-column key renders as a COLUMN modifier (`render_table`'s
    // `single_primary_key` path), so the stale key is visible as `.primaryKey()` on the
    // wrong column rather than as a table-level clause.
    assert!(
        artifacts.env_db_ts.contains("id: t.text().primaryKey()"),
        "TODAY: env.db.ts declares the PRE-ALTER primary key, so regenerating an app \
         from it reinstates a key the migration removed:\n{}",
        artifacts.env_db_ts
    );
    assert!(
        !artifacts
            .env_db_ts
            .contains("legacy_id: t.text().notNull().primaryKey()"),
        "TODAY: the column the replace made the primary key is not marked as one:\n{}",
        artifacts.env_db_ts
    );
}
