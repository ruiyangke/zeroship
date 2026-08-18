//! **The step 3 gate: every projection reproduces its walker, byte for byte.**
//!
//! ONE leg, not four. Step 4 retires a leg with each walker it deletes: consumer 1 took
//! `runtime_metadata`, consumer 2 took `authoring_tables` and consumer 3 took
//! `field_defs` - see [`Projection`]. Only `fold_ops` is left to compare against, and
//! that leg retires with consumer 4, at which point this file has nothing to measure and
//! goes with it.
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
//! replay refuses produces no projection at all. The one walker still compared here -
//! `fold_ops` - IS that replay, so [`Verdict::FoldRefused`] is **zero** and
//! `BOTH_REFUSED` carries every refused prefix. That was not true while
//! `authoring_tables_from_ops` was in this gate: it applied no coherence gate at all and
//! answered about streams the fold refused, which is what the 196 fold-refused prefixes
//! were.
//!
//! The consequence is worth stating rather than celebrating. A `FOLD_REFUSED` of zero
//! does not mean the gate sees more; it means the last walker whose answer could have
//! contradicted the fold on a refused stream is gone, so there is nothing left here to
//! contradict it. What replaces that evidence is an OVER-REFUSAL control at the
//! artifact level, in `tests/gen_types_authoring_tables_from_the_fold.rs` and
//! `tests/gen_types_field_defs_from_the_fold.rs`, because an equality gate is blind to a
//! refusal change by construction.

use std::collections::BTreeMap;

use super::differential_corpus::{
    parse, policy, read_golden, CASES, DIALECTS, SCHEMA, STEMS, STREAMS,
};
use crate::model::ir::Op;
use crate::model::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, IndexSnapshot, SchemaSnapshot, TableSnapshot,
};
use crate::render::fold::fold_ops;
use crate::render::fold::single_fold;
use crate::SqlDialect;

/// The projections still measurable here, named for the walker each must reproduce.
///
/// `RuntimeMetadata`, `AuthoringTables` and `FieldDefs` are NOT in this list any more,
/// and their absence is the shape of step 4 rather than a gap. This gate compares a
/// projection to the WALKER it replaces; consumer 1 deleted `runtime_metadata_from_ops`,
/// consumer 2 deleted `authoring_tables_from_ops` and consumer 3 deleted
/// `fold_to_field_defs`, so for each of those there is no second answer left and keeping
/// the leg would have compared the projection to itself. What replaces each is a gate at
/// the ARTIFACT level - `tests/gen_types_runtime_metadata_from_the_fold.rs`,
/// `tests/gen_types_authoring_tables_from_the_fold.rs` and
/// `tests/gen_types_field_defs_from_the_fold.rs` - whose goldens were captured from the
/// walkers before they were deleted, so the evidence a walker used to provide outlives
/// the walker. Consumer 4 retires the last leg the same way.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Projection {
    /// `FoldedSchema::project_snapshot` against `fold_ops`.
    Snapshot,
}

const PROJECTIONS: [Projection; 1] = [Projection::Snapshot];

impl Projection {
    fn label(self) -> &'static str {
        match self {
            Projection::Snapshot => "snapshot",
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
///
/// BOTH arms are unconstructed now that [`DIVERGENCES`] is empty, and the whole type
/// stays for the same reason `ByDesign` stayed while `WalkerDefect` was in use: a gate
/// that deletes its triage vocabulary the moment nothing needs it has nowhere honest to
/// put the next difference, and the next consumer move is the one most likely to
/// produce one. `differential_corpus::Status::Defect` records the mirror of this
/// reasoning for its own unconstructed arm.
#[derive(Clone, Copy)]
#[expect(
    dead_code,
    reason = "the triage vocabulary outlives the current (empty) divergence set; see \
              the doc comment"
)]
enum Side {
    /// The WALKER is wrong and the projection is right. The fold must not be held to
    /// it, and the evidence is stated so the claim can be argued with.
    WalkerDefect(&'static str),
    /// Neither is wrong: the two answers are equally true statements about different
    /// things, and the reason says which.
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

/// The recorded set. EMPTY is not the goal and a shrinking count is not automatically
/// progress - see `the_gate_has_the_shape_it_claims`.
///
/// It is empty NOW, and the reason is worth stating rather than leaving to be inferred
/// from a bare `&[]`: every divergence this gate ever recorded belonged to a leg that
/// has since retired with its walker, so there is no longer a second answer to differ
/// from. The one leg left compares `project_snapshot` to `fold_ops`, and those two have
/// agreed byte for byte on the whole corpus since step 3.
#[rustfmt::skip]
const DIVERGENCES: &[Divergence] = &[
    // SIX ROWS RETIRED WITH THEIR LEG, not fixed away and not lost.
    //
    // `v_primary_key|{Postgres,Sqlite,Mysql}|authoring_tables` recorded
    // `line 45: fold "legacy_id," walker "id,"` - `authoring_tables_from_ops` had no
    // `Op::AlterPrimaryKey` arm, so `env.db.ts` kept the primary key the migration
    // replaced. Step 4 consumer 2 deleted that walker. The defect it named is now pinned
    // FIVE ways at the artifact level in
    // `tests/gen_types_authoring_tables_from_the_fold.rs` and adjudicated against a live
    // PostgreSQL in `tests/env_db_ts_matches_the_server_pg.rs`.
    //
    // `v_index_and_constraint|{Postgres,Sqlite,Mysql}|field_defs` recorded
    // `line 9: fold "\"required\": true" walker "\"required\": true,"` -
    // `fold_to_field_defs` lifted a single-column `UNIQUE` onto the column descriptor
    // and had no arm that could take it back, so `schema.runtime.json` kept calling a
    // column unique after the `dropConstraint` that removed the constraint. (The FK half
    // of exactly that lift WAS un-lifted; the walker's `Op::DropConstraint` arm existed
    // and touched `fks` only - the F113 pattern the review log names, one fix with its
    // sibling left open.) Step 4 consumer 3 deleted that walker, so the row has no
    // second side to differ from.
    //
    // It is not evidence that evaporated, and it did not shrink either: measured through
    // the artifact rather than through this gate's canonical text, the same walker was
    // wrong in FIVE families, not one. All five are pinned in
    // `tests/gen_types_field_defs_from_the_fold.rs`, and the claim that none of them can
    // reach a SQLite table rebuild is measured against a real database in
    // `tests/sqlite_rebuild_field_defs_live.rs`. A recorded divergence traded for a live
    // oracle is the trade this gate exists to make.
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
         compare, because the fold fails closed and an artifact walker did not - it is \
         ZERO since consumer 2 deleted the last such walker"
    );
}

/// Byte-identical comparisons across the whole corpus.
///
/// The number has fallen three times and ALL THREE falls are a leg retiring with the
/// walker it compared against, not a stream leaving the corpus. A shrinking coverage
/// count is the one number here that reads as progress and is usually a leak, so each
/// drop is accounted for EXACTLY rather than plausibly.
///
/// Every leg measures the same set of prefixes, and that set is
/// `sum over streams of (len + 1) * DIALECTS` = **879**. The four counters for one leg
/// therefore always sum to 879, which is the identity the accounting below turns on:
///
/// | | equal | differs | both refused | fold refused | sum |
/// |---|---|---|---|---|---|
/// | `snapshot` | 683 | 0 | 196 | 0 | 879 |
/// | `field_defs` (retired here) | 677 | 6 | 196 | 0 | 879 |
/// | `authoring_tables` (retired by consumer 2) | 677 | 6 | 0 | 196 | 879 |
/// | `runtime_metadata` (retired by consumer 1) | 683 | 0 | 0 | 196 | 879 |
///
/// So: 2,720 over FOUR legs at step 3; 2,037 over THREE when consumer 1 deleted
/// `runtime_metadata_from_ops` (2,720 - 2,037 = 683, that leg's equal share); 1,360
/// over TWO when consumer 2 deleted `authoring_tables_from_ops` (2,037 - 1,360 = 677);
/// and 683 over ONE now (1,360 - 683 = 677, this leg's equal share, the same figure
/// because the two artifact legs measured the same corpus and diverged on the same
/// count of prefixes).
///
/// The CORPUS itself is unchanged across this move, which is the claim the arithmetic
/// above is only circumstantial evidence for and which is checked directly: `STEMS`,
/// `STREAMS` and `CASES` are untouched, `measured.len()` is still asserted equal to
/// `(STEMS + STREAMS + CASES) * DIALECTS * PROJECTIONS`, and
/// [`TABLES_PROBED_PER_FIELD`] - which sweeps `corpus_streams()` and does not depend
/// on `PROJECTIONS` at all - is still 97. A stream deleted from the corpus would move
/// both of those; retiring a leg moves neither.
///
/// What now covers the retired legs is
/// `tests/gen_types_runtime_metadata_from_the_fold.rs`,
/// `tests/gen_types_authoring_tables_from_the_fold.rs` and
/// `tests/gen_types_field_defs_from_the_fold.rs`, whose goldens were captured from the
/// walkers before they were deleted.
const EQUAL_COMPARISONS: usize = 683;
/// Comparisons whose two texts differ. Every one is attributed in [`DIVERGENCES`].
///
/// Was 12, then 6, and now ZERO - and zero here is NOT "the divergences were fixed".
/// Every one of the 12 belonged to a leg that has retired, and the `snapshot` leg
/// contributed none of them at any point. What the number says is that the only
/// comparison left in this file is one that has never differed; what it does not say is
/// anything at all about the three artifact projections, whose evidence now lives in
/// three artifact-level gates.
const DIFFERING_COMPARISONS: usize = 0;
/// Prefixes both sides refuse.
///
/// 392 -> 196, which is exactly the retired leg's own 196 and nothing else. The
/// remaining leg's share is unchanged, and [`FOLD_REFUSAL_PREFIXES`] - measured without
/// reference to any leg - is still 196, which is the check that the fold's refusal set
/// did not move when a leg left.
const BOTH_REFUSED: usize = 196;
/// Prefixes the fold refuses and a walker answers about.
///
/// ZERO. `authoring_tables_from_ops` was the last walker in this gate with no coherence
/// gate of its own, and consumer 2 deleted it; the one walker left, `fold_ops`, IS the
/// coherence gate the fold runs. Pinned rather than deleted, because a return to
/// non-zero would mean the fold started refusing something the surviving walker still
/// answers - the direction this gate cannot otherwise see.
const FOLD_REFUSED: usize = 0;

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
// The fold's own refusal set, stated independently of any leg
// ---------------------------------------------------------------------------

/// **The fold refuses exactly what the catalog replay refuses, prefix for prefix.**
///
/// Every counter above is a property of a LEG, and a leg can retire. That makes the
/// counters incomparable across a consumer move in one specific and dangerous way:
/// [`FOLD_REFUSED`] fell from 196 to 0 in this change, and the honest reading is not
/// "the fold refuses less" but "the last walker that could disagree with a refusal is
/// gone". A reader arriving at the diff cannot tell those apart from the constants.
///
/// So this test states the thing the constants only imply, at SET resolution rather
/// than as a count, and without reference to any projection: for every prefix of every
/// corpus stream under every dialect, `single_fold::fold` errors exactly when
/// `fold_ops` errors. `fold` calls `fold_ops_onto` first, so its refusal set is a
/// SUPERSET by construction; asserting equality is asserting that `AuthoredState::advance`'s
/// three fallible sites add nothing on this corpus, which is the direction a consumer
/// move could break and no equality gate can see.
///
/// This is the leg-independent statement of what
/// `tests/gen_types_authoring_tables_from_the_fold.rs`'s over-refusal control says at
/// the artifact level.
#[test]
fn the_folds_refusal_set_is_the_catalog_replays_refusal_set() {
    let mut fold_only = Vec::new();
    let mut catalog_only = Vec::new();
    let mut different_reason = Vec::new();
    let mut refused = 0usize;
    let mut accepted = 0usize;
    for (name, ops, confined) in corpus_streams() {
        for dialect in DIALECTS {
            let effective = policy(confined);
            for i in 0..=ops.len() {
                let prefix = &ops[..i];
                let mine = single_fold::fold(prefix, dialect, SCHEMA, &effective);
                let catalog = fold_ops(prefix, dialect, SCHEMA, &effective);
                match (mine, catalog) {
                    (Err(mine), Err(catalog)) => {
                        refused += 1;
                        if mine.to_string() != catalog.to_string() {
                            different_reason.push(format!(
                                "{name}|{dialect:?}|{i} ops -> fold: {mine} / catalog: {catalog}"
                            ));
                        }
                    }
                    (Ok(_), Ok(_)) => accepted += 1,
                    (Err(mine), Ok(_)) => {
                        fold_only.push(format!("{name}|{dialect:?}|{i} ops -> {mine}"));
                    }
                    (Ok(_), Err(catalog)) => {
                        catalog_only.push(format!("{name}|{dialect:?}|{i} ops -> {catalog}"));
                    }
                }
            }
        }
    }
    assert!(
        fold_only.is_empty(),
        "the fold refuses prefixes the catalog replay accepts, which is a NEW refusal \
         no equality gate can see:\n  {}",
        fold_only.join("\n  ")
    );
    assert!(
        catalog_only.is_empty(),
        "the catalog replay refuses prefixes the fold accepts, which would make the \
         fold LESS fail-closed than the walker it replaces:\n  {}",
        catalog_only.join("\n  ")
    );
    // THE ONE THING THE PER-OP DRIVE CAN CHANGE, and the SET assertions above cannot
    // see it. Since the catalog rules moved into `CatalogFold::advance`, `fold` no
    // longer runs the whole catalog replay before the authored half starts: the two
    // halves advance on the same op, catalog first. So a stream the AUTHORED half
    // refuses at op i and the CATALOG half refuses at some later op j now reports op
    // i's reason where it used to report op j's. The refusal SET is unchanged either
    // way - refusing is a disjunction over the same two predicates - which is exactly
    // why an `is_err()` comparison would pass through the change without noticing.
    //
    // UNREACHABLE ON TODAY'S CORPUS, and saying so is the point rather than an
    // admission. The assertion right above already states that
    // `AuthoredState::advance`'s three fallible sites add nothing here; measured
    // harder for this check, the authored half never refuses AT ALL on this corpus.
    // Neuter: give the authored `createEnum` and `createTable` sites a distinct error
    // string AND run the authored half first - both halves of what this check exists to
    // catch - and it still reports empty. So this is a GUARD, not evidence: it costs a
    // string compare on 196 prefixes and it starts speaking the day a corpus stream
    // reaches an authored-only refusal, which is also the day the ORDER of the two
    // `advance` calls in `single_fold::fold` starts being observable. Do not read a
    // green here as a measurement that the order is right.
    //
    // The order is right for a structural reason instead: each of the three authored
    // fallible sites is the SAME call the catalog arm for that SAME op already makes,
    // so the catalog half reaches the refusal first or together, and it runs first.
    assert!(
        different_reason.is_empty(),
        "the fold and the catalog replay refuse the same prefixes for DIFFERENT \
         reasons, so the per-op drive changed which half reports an incoherent \
         stream:\n  {}",
        different_reason.join("\n  ")
    );
    // Two-sided floor. All-accepted would pass while proving nothing about refusals,
    // and all-refused would prove nothing about the streams that still fold.
    assert_eq!(
        refused, FOLD_REFUSAL_PREFIXES,
        "prefixes BOTH refuse. Pinned, not bounded: this is the fold's whole refusal \
         set, and it is the number `FOLD_REFUSED` and `BOTH_REFUSED` are both derived \
         from - 196 per leg, 392 across the two surviving legs"
    );
    assert!(
        accepted >= 500,
        "the check must also exercise acceptance: {accepted}"
    );
}

/// Prefixes the fold and the catalog replay both refuse, over the whole corpus.
///
/// UNCHANGED by step 4 consumer 2, which is the direct answer to "did the fold start
/// refusing more when the leg retired". It did not: the same 196 prefixes, and the
/// same ones the `snapshot` leg's `both_refused` counted before the move.
const FOLD_REFUSAL_PREFIXES: usize = 196;

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

/// **The retired `field_defs` divergence, restated as the agreement that replaced it.**
///
/// This test used to assert the defect - `TODAY: the FieldDef map still calls the column
/// unique after its constraint was dropped`. `fold_to_field_defs` lifted a single-column
/// `UNIQUE` onto the column descriptor and had no arm that could take it back, so
/// `schema.runtime.json` kept describing a database the catalog did not have. Step 4
/// consumer 3 removed the walker that produced that answer, so what is left to state is
/// that the catalog oracle and the shipped artifact now say the same thing about the
/// same column out of the same stream.
///
/// The anchor is `fold_ops`, not this file's opinion: it is the structural oracle the
/// live PostgreSQL, SQLite and MySQL suites already run against a real server, and at
/// this prefix it has removed the constraint.
#[test]
fn the_catalog_and_the_runtime_artifact_agree_about_a_dropped_unique_constraint() {
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

    let folded = single_fold::fold(ops, SqlDialect::Postgres, SCHEMA, &effective).expect("fold");
    assert_eq!(
        folded.project_field_defs()["users"]["email"].get("unique"),
        None,
        "the projection derives uniqueness from the constraints the model still holds, \
         so a dropped constraint cannot outlive itself"
    );

    // The SHIPPED consequence, not just the intermediate. `schema.runtime.json` is what
    // a deployed app installs its `env.db` types from, and - on SQLite - the same map
    // is what a table rebuild renders its `CREATE TABLE` from.
    let artifacts = super::render_artifacts(ops, SqlDialect::Postgres, SCHEMA, &effective)
        .expect("render artifacts");
    let runtime: serde_json::Value =
        serde_json::from_str(&artifacts.runtime_json).expect("runtime.json parses");
    assert_eq!(
        runtime["collections"]["users"]["fields"]["email"].get("unique"),
        None,
        "and the artifact stops declaring a uniqueness the catalog does not have:\n{}",
        artifacts.runtime_json
    );
}

/// **The same un-lift hole, on a facet the step 1 corpus cannot see.**
///
/// The test above found the UNIQUE half because `v_index_and_constraint` adds and then
/// drops one. No corpus stream drops a CHECK, so this gate is blind to the `min`/`max`
/// half of the same hole - which is stated here rather than left for the next reader to
/// rediscover, because "the gate is green" is only as strong as what the gate can see.
/// The projection derives both from the constraints the model still holds, so one rule
/// covers the whole family instead of one arm per facet.
///
/// That blindness is exactly what step 4 consumer 3 measured rather than inherited. A
/// sweep of the deleted walker against this projection over every prefix of the corpus
/// AND of a carrier set written for the constraint lifecycle found FIVE divergence
/// families where this gate had recorded ONE. The other four - the CHECK bound below,
/// a CHECK membership, a re-added column inheriting a dropped column's facets, and
/// `dropPartition` - are pinned in `tests/gen_types_field_defs_from_the_fold.rs`.
#[test]
fn a_dropped_check_constraint_does_not_outlive_itself_in_the_field_def_map() {
    let ops: Vec<Op> = parse(
        r#"[
  {"op":"createTable","name":"scores","columns":[{"name":"id","type":"text","nullable":false},{"name":"score","type":"int"}],"primaryKey":["id"]},
  {"op":"addConstraint","table":"scores","constraint":{"name":"scores_score_ck","kind":{"kind":"check","expr":{"node":"binOp","op":"and","lhs":{"node":"binOp","op":"ge","lhs":{"node":"colRef","name":"score"},"rhs":{"node":"literal","value":1}},"rhs":{"node":"binOp","op":"le","lhs":{"node":"colRef","name":"score"},"rhs":{"node":"literal","value":9}}}}}},
  {"op":"dropConstraint","table":"scores","name":"scores_score_ck"}
]"#,
    );
    let effective = policy(false);

    // The bound is really recovered while the constraint is live, or the drop below
    // proves nothing. Measured through the projection, since the walker that used to
    // answer this is gone.
    let with_check = single_fold::fold(&ops[..2], SqlDialect::Postgres, SCHEMA, &effective)
        .expect("fold")
        .project_field_defs();
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

/// **The retired divergence's replacement, in this file: the two answers now AGREE.**
///
/// This test used to assert the defect - `TODAY: env.db.ts declares the PRE-ALTER
/// primary key`. Step 4 consumer 2 removed the walker that produced that answer, so
/// what is left to state is that the catalog oracle and the shipped artifact now say
/// the same thing about the same table out of the same stream.
///
/// The anchor is `fold_ops`, the structural oracle the live PostgreSQL, SQLite and
/// MySQL suites already run against a real server; the ADJUDICATION that the fold's
/// answer is the server's answer is not made here but in
/// `tests/env_db_ts_matches_the_server_pg.rs`, which applies this migration for real
/// and reads `pg_catalog`.
#[test]
fn the_catalog_and_the_authoring_artifact_agree_about_an_altered_primary_key() {
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
    // `single_primary_key` path), so the key is visible as `.primaryKey()` on a column
    // rather than as a table-level clause.
    // `.primaryKey()` implies NOT NULL in the authoring API, so the emitter renders
    // the marker without a `.notNull()` beside it. Asserted as the emitter actually
    // spells it rather than as a reader would guess.
    assert!(
        artifacts
            .env_db_ts
            .contains("legacy_id: t.text().primaryKey()"),
        "env.db.ts declares the key the replace installed:\n{}",
        artifacts.env_db_ts
    );
    // `contains` on `id: …` would also match `legacy_id: …`, so the check is anchored
    // to the emitter's four-space column indent. Without the anchor this assertion
    // would fail for the wrong reason on the very artifact it is meant to accept.
    assert!(
        !artifacts
            .env_db_ts
            .contains("\n      id: t.text().primaryKey()"),
        "and no longer declares the one it removed:\n{}",
        artifacts.env_db_ts
    );
}
