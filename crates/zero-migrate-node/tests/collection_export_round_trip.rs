//! **The structured export survives the crossing: `engine -> dto -> engine` is the
//! identity over a folded corpus.**
//!
//! `genArtifacts` now returns the folded schema as TYPED collections beside the two
//! artifact strings, so a host can render its own files instead of re-parsing
//! `schema.runtime.json` out of the reply it already got. That export is only worth
//! anything if it carries what the fold recovered, and the failure mode is silent: a
//! facet with no slot on the wire does not error, it simply is not there, and a
//! consumer reading the export cannot tell an absent facet from an undeclared one.
//!
//! The oracle is therefore a ROUND TRIP rather than a shape assertion. Both directions
//! are hand-written conversions in [`zero_migrate_node::descriptors`] — the same
//! functions the addon calls, not test copies of them — so this is a genuine
//! discovery instrument for the wire, not a serde tautology: a serde round trip would
//! only prove the derive is symmetric with itself, while this compares an INDEPENDENT
//! outbound projection against an INDEPENDENT inbound one and fails whenever one of
//! them has a field the other does not.
//!
//! WHEN THIS WAS WRITTEN IT WAS RED, on four facets, and each is a real export loss
//! rather than a test artefact: `refColumn`, `refName`, `charLen` and `maxLength` had
//! no DTO slot at all. They were absent for a reason that was correct while the DTO
//! was INBOUND-ONLY — `bridge.rs` even documented the width pair as unread by
//! `gen_types` — and stopped being correct the moment the same type had to carry an
//! export outward, because the fold's `ir_column_to_field` populates all four.
//!
//! # What this does NOT prove
//!
//! * NOT that the export is CORRECT. It compares the export against the fold's own
//!   answer, so a facet the fold recovers WRONGLY round-trips just as happily. The
//!   fold's own correctness is adjudicated elsewhere (`schema.runtime.json` goldens,
//!   and the live-server suites).
//! * NOT that re-importing the export reproduces the schema. It cannot in general: the
//!   descriptor-to-ops producer speaks its own vocabulary and drops what falls outside
//!   it. One facet HAS been carried all the way through and is measured below by
//!   [`a_varchar_width_survives_the_wire_and_the_producer`] — the `VARCHAR(n)` width,
//!   which used to reach `token_to_col_type` and die there. That case is the whole
//!   extent of the end-to-end claim; nothing here generalises it.
//! * NOT anything about `env.db.ts`. That artifact is rendered from the richer
//!   authoring IR, and no descriptor vocabulary can reconstruct it.
//! * NOT that a JS caller sees the fields. This is the napi-free build; the `.node`
//!   surface is `index.d.ts` and the host suite.

mod support;

use zero_migrate::model::expr::{BinaryOp, Expr};
use zero_migrate::model::ir::{ColType, GeneratedCol, IdentityCol, IrColumn, Op};
use zero_migrate::render::declarative::{CollectionDescriptor, FieldDescriptor, IndexDescriptor};
use zero_migrate::{SqlDialect, TableRuntimeOptions};

use zero_migrate_node::descriptors::{
    descriptor_dto_to_engine, descriptor_to_dto, field_dto_to_engine, field_to_dto,
};

const SCHEMA: &str = "public";

// ---------------------------------------------------------------------------
// The corpus.
// ---------------------------------------------------------------------------

/// Seed descriptors covering the facets that are EASY TO LOSE: the ones carried as
/// opaque sub-objects (`encrypted`, `mask`, `generated`, `identity`), the ones
/// recovered from a constraint rather than a column (`min`/`max`, `enum`, `unique`,
/// the FK actions), and the reference identity a `ref` brand cannot express.
///
/// Everything here goes through the PRODUCER (`descriptors -> ops`) and then the fold,
/// so what comes back is what the engine recovered, not what this function typed in.
fn seed_descriptors() -> Vec<CollectionDescriptor> {
    let text = |name: &str, ty: &str| FieldDescriptor {
        name: name.to_string(),
        ty: ty.to_string(),
        ..Default::default()
    };

    let owners = CollectionDescriptor {
        name: "owners".to_string(),
        owner_app: "app_export".to_string(),
        fields: vec![
            FieldDescriptor {
                name: "id".to_string(),
                ty: "string".to_string(),
                required: true,
                ..Default::default()
            },
            text("label", "string"),
        ],
        indexes: Vec::new(),
        runtime_options: TableRuntimeOptions::default(),
    };

    let facets = CollectionDescriptor {
        name: "facets".to_string(),
        owner_app: "app_export".to_string(),
        fields: vec![
            FieldDescriptor {
                name: "id".to_string(),
                ty: "string".to_string(),
                required: true,
                ..Default::default()
            },
            // The FK identity a `ColType::Ref` brand cannot carry: an explicit target
            // COLUMN, an explicit constraint NAME, and both referential actions.
            FieldDescriptor {
                name: "owner_id".to_string(),
                ty: "ref".to_string(),
                references: Some("owners".to_string()),
                reference_column: Some("id".to_string()),
                reference_name: Some("facets_owner_fk".to_string()),
                on_delete: Some("cascade".to_string()),
                on_update: Some("restrict".to_string()),
                ..Default::default()
            },
            // Recovered from a CHECK the producer emits, not from the column.
            FieldDescriptor {
                name: "score".to_string(),
                ty: "int".to_string(),
                min: Some(0.0),
                max: Some(100.0),
                ..Default::default()
            },
            FieldDescriptor {
                name: "status".to_string(),
                ty: "string".to_string(),
                enum_values: Some(vec!["draft".into(), "live".into()]),
                ..Default::default()
            },
            // Opaque sub-object facets, carried verbatim across the wire.
            FieldDescriptor {
                name: "secret".to_string(),
                ty: "string".to_string(),
                encrypted: Some(serde_json::json!({
                    "mode": "randomised", "keyId": "default", "wraps": "string",
                })),
                ..Default::default()
            },
            FieldDescriptor {
                name: "email".to_string(),
                ty: "string".to_string(),
                mask: Some(serde_json::json!({ "kind": "email", "classification": "pii" })),
                ..Default::default()
            },
            FieldDescriptor {
                name: "seq".to_string(),
                ty: "bigInt".to_string(),
                identity: Some(IdentityCol { always: true }),
                ..Default::default()
            },
            // Fixed-width char: the producer DOES thread this one (`token_to_col_type`
            // reads `char_len`), so it exercises the wire rather than the producer.
            FieldDescriptor {
                name: "code".to_string(),
                ty: "char".to_string(),
                char_len: Some(8),
                ..Default::default()
            },
            FieldDescriptor {
                name: "embedding".to_string(),
                ty: "vector".to_string(),
                vector_dims: Some(3),
                vector_metric: Some("cosine".to_string()),
                ..Default::default()
            },
            FieldDescriptor {
                name: "handle".to_string(),
                ty: "string".to_string(),
                unique: true,
                case_sensitive: Some(false),
                ..Default::default()
            },
            FieldDescriptor {
                name: "kind".to_string(),
                ty: "string".to_string(),
                required: true,
                default: Some(serde_json::json!("note")),
                ..Default::default()
            },
            FieldDescriptor {
                name: "tenant".to_string(),
                ty: "id".to_string(),
                id_prefix: Some("ten".to_string()),
                ..Default::default()
            },
            // A genuine unbounded text column — the one facet the wire deliberately
            // does not carry, measured on its own below.
            //
            // The token is `"string"`, not `"text"`: the descriptor lexicon has no
            // `"text"` token at all (`token_to_col_type` refuses it outright, which is
            // how this line first read and what it cost to find out), and `"string"`
            // with no width IS the unbounded `ColType::Text` column.
            text("body", "string"),
        ],
        indexes: vec![IndexDescriptor {
            name: "ix_facets_status".to_string(),
            columns: vec!["status".to_string()],
            unique: false,
        }],
        runtime_options: TableRuntimeOptions {
            soft_delete: true,
            versioning: true,
            strictness: zero_migrate::TableStrictness::Lenient,
        },
    };

    vec![owners, facets]
}

/// A hand-built typed `createTable` for the facets the DESCRIPTOR producer cannot
/// express: `ColType::String { length }` (a `VARCHAR(n)` width) and a generated
/// column. Authored as ops because that is the only source that can reach them —
/// which is itself the reason a descriptor round trip alone would have measured
/// neither.
fn width_and_generated_ops() -> Vec<Op> {
    let column = |name: &str, ty: ColType| IrColumn {
        name: name.to_string(),
        ty,
        nullable: None,
        default: None,
        unique: None,
        value_format: None,
        references: None,
        id_prefix: None,
        collation: None,
        vector_metric: None,
        case_sensitive: None,
        mask: None,
        generated: None,
        identity: None,
    };
    let mut first = column("first_name", ColType::String { length: 64 });
    let mut last = column("last_name", ColType::String { length: 64 });
    first.nullable = Some(false);
    last.nullable = Some(false);
    let mut full = column("full_name", ColType::Text);
    full.generated = Some(GeneratedCol {
        expr: Expr::BinOp {
            op: BinaryOp::Concat,
            lhs: Box::new(Expr::col("first_name")),
            rhs: Box::new(Expr::col("last_name")),
        },
        stored: true,
    });

    vec![Op::CreateTable {
        name: "widths".to_string(),
        columns: vec![
            column("id", ColType::Text),
            first,
            last,
            full,
            column("initials", ColType::Char { length: 2 }),
        ],
        primary_key: None,
        constraints: Vec::new(),
        indexes: Vec::new(),
        partition_by: None,
        runtime_options: None,
        schema: None,
        existence_guard: None,
    }]
}

/// Fold the FULL corpus (both arms) under Postgres and return every recovered field,
/// tagged `table.column` so a failure names the column rather than an index.
///
/// Postgres-only, and not by preference. The seed set's `min`/`max`/`enum` facets lower
/// to table-level CHECK constraints, which the fold refuses outside PostgreSQL
/// (`createTable table-level CHECK is PostgreSQL-only`) — so a corpus carrying them
/// simply cannot be folded under MySQL or SQLite. Discovered by writing the round trip
/// as a three-dialect loop and watching it refuse; `the_wire_is_dialect_independent`
/// below carries the portable arm across all three instead of pretending this one does.
fn folded_fields() -> Vec<(String, FieldDescriptor)> {
    let policy = support::no_inject(SCHEMA);
    let dialect = SqlDialect::Postgres;

    let from_descriptors = zero_migrate::render_schema_export_from_descriptors(
        &seed_descriptors(),
        dialect,
        SCHEMA,
        &policy,
    )
    .expect("the seed descriptor set folds");
    let from_ops = folded_portable_fields(dialect);

    let mut out = Vec::new();
    for (table, collection) in &from_descriptors.collections {
        for field in &collection.fields {
            out.push((format!("{table}.{}", field.name), field.clone()));
        }
    }
    out.extend(from_ops);
    out
}

/// The PORTABLE arm: the typed width/generated ops, which carry no CHECK and therefore
/// fold under every dialect.
fn folded_portable_fields(dialect: SqlDialect) -> Vec<(String, FieldDescriptor)> {
    let policy = support::no_inject(SCHEMA);
    let export =
        zero_migrate::render_schema_export(&width_and_generated_ops(), dialect, SCHEMA, &policy)
            .expect("the typed width/generated ops fold on every dialect");
    let mut out = Vec::new();
    for (table, collection) in &export.collections {
        for field in &collection.fields {
            out.push((format!("{table}.{}", field.name), field.clone()));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The floor: the corpus must actually carry the facets it claims to.
// ---------------------------------------------------------------------------

/// The facets this corpus is here to protect. Named rather than counted so a
/// regression reports WHICH one stopped being exercised.
const COVERED_FACETS: [&str; 17] = [
    "required",
    "unique",
    "references",
    "reference_column",
    "reference_name",
    "on_delete",
    "on_update",
    "default",
    "min",
    "max",
    "enum_values",
    "id_prefix",
    "vector_dims",
    "char_len",
    "max_length",
    "case_sensitive",
    "encrypted",
];

/// Which of [`COVERED_FACETS`] this field carries a non-default value for.
fn facets_present(field: &FieldDescriptor) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut mark = |present: bool, name: &'static str| {
        if present {
            out.push(name);
        }
    };
    mark(field.required, "required");
    mark(field.unique, "unique");
    mark(field.references.is_some(), "references");
    mark(field.reference_column.is_some(), "reference_column");
    mark(field.reference_name.is_some(), "reference_name");
    mark(field.on_delete.is_some(), "on_delete");
    mark(field.on_update.is_some(), "on_update");
    mark(field.default.is_some(), "default");
    mark(field.min.is_some(), "min");
    mark(field.max.is_some(), "max");
    mark(field.enum_values.is_some(), "enum_values");
    mark(field.id_prefix.is_some(), "id_prefix");
    mark(field.vector_dims.is_some(), "vector_dims");
    mark(field.char_len.is_some(), "char_len");
    mark(field.max_length.is_some(), "max_length");
    mark(field.case_sensitive.is_some(), "case_sensitive");
    mark(field.encrypted.is_some(), "encrypted");
    out
}

/// **The instrument's own floor, and it is TWO-SIDED.**
///
/// A round trip over a corpus that exercises nothing passes trivially, so the corpus
/// is measured before it is trusted. Both directions matter and they catch different
/// regressions: fewer facets than pinned means the corpus quietly stopped covering one
/// (or the FOLD stopped recovering it), and MORE means a facet entered the corpus
/// without anyone deciding the round trip should carry it.
///
/// The numbers below were measured against the UNCHANGED tree first, on the folded
/// output rather than on the seeds — `mask`, `generated` and `identity` are asserted
/// separately because the fold synthesises them (an encrypted column gains the
/// fail-safe `{ full, pii }` auto-mask it was never given) and a census would report
/// coverage this corpus did not author.
#[test]
fn the_corpus_exercises_every_facet_it_claims_to() {
    let fields = folded_fields();

    let mut seen: Vec<&'static str> = Vec::new();
    for (_, field) in &fields {
        for facet in facets_present(field) {
            if !seen.contains(&facet) {
                seen.push(facet);
            }
        }
    }
    seen.sort_unstable();
    let mut expected = COVERED_FACETS.to_vec();
    expected.sort_unstable();
    assert_eq!(
        seen,
        expected,
        "the corpus no longer covers exactly the facets it claims: \
         missing {:?}, unexpected {:?}",
        expected
            .iter()
            .filter(|f| !seen.contains(f))
            .collect::<Vec<_>>(),
        seen.iter()
            .filter(|f| !expected.contains(f))
            .collect::<Vec<_>>(),
    );

    // The sub-object facets, counted rather than merely present: each is carried
    // VERBATIM as opaque JSON, so a wire that dropped one would still pass a
    // presence check on the others.
    let masked = fields.iter().filter(|(_, f)| f.mask.is_some()).count();
    let encrypted = fields.iter().filter(|(_, f)| f.encrypted.is_some()).count();
    let generated = fields.iter().filter(|(_, f)| f.generated.is_some()).count();
    let identity = fields.iter().filter(|(_, f)| f.identity.is_some()).count();
    // TWO masks: the one authored on `email`, plus the fail-safe `{ full, pii }` the
    // fold synthesises for the encrypted column. Pinned two-sided so losing the
    // synthesised one does not read as "still covered".
    assert_eq!((masked, encrypted, generated, identity), (2, 1, 1, 1));

    // A floor on the corpus SIZE, so a fold that silently stops emitting a table
    // cannot leave a smaller corpus passing every assertion above.
    assert_eq!(
        fields.len(),
        20,
        "corpus size moved; re-measure before re-pinning"
    );
}

// ---------------------------------------------------------------------------
// The round trip.
// ---------------------------------------------------------------------------

/// `engine -> dto -> engine`, normalising ONLY `unbounded_text`.
///
/// That one facet is `#[serde(skip)]` in the engine and documented render-only: it is
/// DERIVED from the column's own type, so it has no wire slot by design. Normalising
/// it here rather than quietly comparing around it keeps the exclusion visible, and
/// `unbounded_text_is_re_derived_rather_than_carried` measures the derivation claim
/// instead of assuming it.
///
/// The comparison is `FieldDescriptor`'s `PartialEq`, which its own definition
/// reserves for tests ("the differ compares SNAPSHOTS, not descriptors") — this is
/// that use, and it is why no gate anywhere is built on descriptor equality.
fn assert_round_trips(label: &str, original: &FieldDescriptor) {
    let mut back = field_dto_to_engine(field_to_dto(original))
        .unwrap_or_else(|e| panic!("{label}: the exported dto does not convert back: {e}"));
    let mut expected = original.clone();
    back.unbounded_text = false;
    expected.unbounded_text = false;
    assert_eq!(
        back, expected,
        "{label}: a facet did not survive engine -> dto -> engine"
    );
}

#[test]
fn every_folded_field_survives_the_export_round_trip() {
    for (label, field) in folded_fields() {
        assert_round_trips(&label, &field);
    }
}

/// The COLLECTION level round-trips too — name, owner, indexes and runtime options.
///
/// A separate assertion because `assert_round_trips` is per-FIELD and would pass with
/// every collection-level facet dropped. The strictness token in particular crosses as
/// a STRING through a hand-written pair of match arms in
/// [`zero_migrate_node::descriptors`], one per direction, and only one of its three
/// values is exercised anywhere else — so all three are driven here.
#[test]
fn a_whole_collection_survives_the_export_round_trip() {
    use zero_migrate::TableStrictness;

    let policy = support::no_inject(SCHEMA);
    for strictness in [
        TableStrictness::Strict,
        TableStrictness::Lenient,
        TableStrictness::Off,
    ] {
        let mut seeds = seed_descriptors();
        for seed in &mut seeds {
            seed.runtime_options.strictness = strictness;
        }
        let export = zero_migrate::render_schema_export_from_descriptors(
            &seeds,
            SqlDialect::Postgres,
            SCHEMA,
            &policy,
        )
        .expect("the seed descriptor set folds");

        for (name, collection) in &export.collections {
            let back =
                descriptor_dto_to_engine(descriptor_to_dto(collection)).unwrap_or_else(|e| {
                    panic!("{name}: the exported collection will not convert back: {e}")
                });
            // `unbounded_text` has no wire slot; normalise it on both sides exactly as
            // the per-field assertion does, and for the same measured reason.
            let mut expected = collection.clone();
            let mut back = back;
            for field in &mut expected.fields {
                field.unbounded_text = false;
            }
            for field in &mut back.fields {
                field.unbounded_text = false;
            }
            assert_eq!(
                back, expected,
                "{name} ({strictness:?}): a collection-level facet did not survive the round trip"
            );
        }
    }
}

/// The wire carries the same facets whichever dialect the fold ran under.
///
/// A weaker claim than the one above and deliberately so: leg selection changes WHICH
/// COLUMNS EXIST, so there is no dialect-independent schema to compare. What is
/// dialect-independent is the CROSSING — whatever a dialect's fold recovered must
/// survive it. This runs the portable corpus arm under all three targets and asserts
/// exactly that.
#[test]
fn the_wire_is_dialect_independent() {
    for dialect in [SqlDialect::Postgres, SqlDialect::Mysql, SqlDialect::Sqlite] {
        let fields = folded_portable_fields(dialect);
        assert_eq!(
            fields.len(),
            5,
            "{dialect:?}: the portable arm no longer folds five columns"
        );
        for (label, field) in fields {
            assert_round_trips(&format!("{dialect:?} {label}"), &field);
        }
    }
}

/// The refusal that shaped the test above, pinned so it is a measured constraint
/// rather than a remembered one.
///
/// The CHECK-bearing corpus is PostgreSQL-only. If that ever changes, this test fails
/// and the full corpus can be carried across all three dialects.
#[test]
fn the_check_bearing_corpus_is_postgres_only() {
    let policy = support::no_inject(SCHEMA);
    for dialect in [SqlDialect::Mysql, SqlDialect::Sqlite] {
        let refused = zero_migrate::render_schema_export_from_descriptors(
            &seed_descriptors(),
            dialect,
            SCHEMA,
            &policy,
        );
        assert!(
            refused.is_err(),
            "{dialect:?} now folds a table-level CHECK; widen the round trip's corpus"
        );
    }
}

/// The one deliberate exclusion, measured rather than asserted.
///
/// `unbounded_text` has no wire slot, so the round trip above normalises it away. That
/// is only safe if it is genuinely re-derivable, and it is: `ir_column_to_field` sets
/// it from `ColType::Text` plus the absence of a value-format / id-prefix facet, so
/// re-folding a descriptor that lost it gets it back. If that ever stops being true,
/// this fails and the exclusion has to be revisited.
#[test]
fn unbounded_text_is_re_derived_rather_than_carried() {
    let policy = support::no_inject(SCHEMA);
    let export = zero_migrate::render_schema_export_from_descriptors(
        &seed_descriptors(),
        SqlDialect::Postgres,
        SCHEMA,
        &policy,
    )
    .expect("the seed descriptor set folds");

    let body = export.collections["facets"]
        .fields
        .iter()
        .find(|f| f.name == "body")
        .expect("the corpus declares an unbounded text column")
        .clone();
    assert!(
        body.unbounded_text,
        "the fold no longer marks a plain t.text() column unbounded"
    );

    // Cross the wire, which drops it, then fold the result again.
    let crossed = field_dto_to_engine(field_to_dto(&body)).expect("the dto converts back");
    assert!(
        !crossed.unbounded_text,
        "unbounded_text is not on the wire; if it now is, delete this test's premise"
    );

    let refolded = zero_migrate::render_schema_export_from_descriptors(
        &[CollectionDescriptor {
            name: "refolded".to_string(),
            owner_app: "app_export".to_string(),
            fields: vec![crossed],
            indexes: Vec::new(),
            runtime_options: TableRuntimeOptions::default(),
        }],
        SqlDialect::Postgres,
        SCHEMA,
        &policy,
    )
    .expect("the round-tripped descriptor re-folds");
    assert!(
        refolded.collections["refolded"].fields[0].unbounded_text,
        "unbounded_text was neither carried nor re-derived — the exclusion is unsound"
    );
}

/// **A scope exclusion, stated as a measurement.**
///
/// The wire has no `literalValue` slot and this change does not add one. The reason is
/// not that it seemed unimportant: it is UNREACHABLE from both `genArtifacts` sources.
/// The fold never writes `literal_value` (`ir_column_to_field` leaves it at its
/// default, and no `ColType` carries a literal), and the producer refuses a `literal`
/// field outright — so a wire slot for it would be surface no call can populate.
///
/// If either half of that changes this test fails, and the slot becomes worth adding.
#[test]
fn a_literal_field_is_unreachable_from_both_gen_artifacts_sources() {
    let policy = support::no_inject(SCHEMA);

    // Half one: the producer refuses the token, so the manual source cannot carry it.
    let refused = zero_migrate::render_schema_export_from_descriptors(
        &[CollectionDescriptor {
            name: "literals".to_string(),
            owner_app: "app_export".to_string(),
            fields: vec![FieldDescriptor {
                name: "kind".to_string(),
                ty: "literal".to_string(),
                literal_value: Some(serde_json::json!("only")),
                ..Default::default()
            }],
            indexes: Vec::new(),
            runtime_options: TableRuntimeOptions::default(),
        }],
        SqlDialect::Postgres,
        SCHEMA,
        &policy,
    );
    assert!(
        refused.is_err(),
        "the producer now accepts a `literal` field; the export needs a literalValue slot"
    );

    // Half two: nothing the fold emits carries one, so the generated source cannot
    // either.
    for (label, field) in folded_fields() {
        assert!(
            field.literal_value.is_none(),
            "{label}: the fold now recovers a literal value; the export must carry it"
        );
    }
}

/// **A `VARCHAR(n)` width survives the wire AND the producer, end to end.**
///
/// This test used to be called `the_producer_not_the_wire_is_where_a_varchar_width_dies`
/// and it asserted the opposite of its last line: `token_to_col_type` mapped every
/// `"string"` token to `ColType::Text` without consulting `max_length`, so re-importing
/// an exported `VARCHAR(64)` produced an unbounded `TEXT`. It was written as a sighted
/// pin — "fixing the producer is announced by this test turning red" — and that is
/// exactly how it went: the producer now reads the facet, and this file's failure was
/// the notice.
///
/// So the claim it makes is now the STRONG one its own message asked for, and it holds
/// three links in a row: the fold recovers the width, the DTO carries it across, and
/// re-folding the crossed descriptor gets it back. The consequence of the middle link
/// having been broken was measured against a live PostgreSQL in
/// `zero-migrate/tests/fold_live/pg_bounded_string_producer_live.rs` — the server
/// stored a 200-character value in a column the author bounded at 64.
#[test]
fn a_varchar_width_survives_the_wire_and_the_producer() {
    let policy = support::no_inject(SCHEMA);
    let export = zero_migrate::render_schema_export(
        &width_and_generated_ops(),
        SqlDialect::Postgres,
        SCHEMA,
        &policy,
    )
    .expect("the typed width ops fold");

    let first = export.collections["widths"]
        .fields
        .iter()
        .find(|f| f.name == "first_name")
        .expect("the corpus declares a bounded string column")
        .clone();
    assert_eq!(first.max_length, Some(64), "the fold recovers the width");

    // The WIRE keeps it — that is what this change bought.
    let crossed = field_dto_to_engine(field_to_dto(&first)).expect("the dto converts back");
    assert_eq!(
        crossed.max_length,
        Some(64),
        "the export wire dropped a VARCHAR width"
    );

    // And the PRODUCER keeps it: re-importing the crossed descriptor yields a
    // VARCHAR(64) again, not an unbounded TEXT.
    let refolded = zero_migrate::render_schema_export_from_descriptors(
        &[CollectionDescriptor {
            name: "refolded".to_string(),
            owner_app: "app_export".to_string(),
            fields: vec![crossed],
            indexes: Vec::new(),
            runtime_options: TableRuntimeOptions::default(),
        }],
        SqlDialect::Postgres,
        SCHEMA,
        &policy,
    )
    .expect("the round-tripped descriptor re-folds");
    assert_eq!(
        refolded.collections["refolded"].fields[0].max_length,
        Some(64),
        "the producer dropped the width again: an exported VARCHAR(64) re-imports as \
         an unbounded TEXT, which on PostgreSQL is a column with no bound at all"
    );
}
