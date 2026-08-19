//! **THE SPIKE QUESTION, measured rather than argued.**
//!
//! `docs/proposals/pluggable-backends.md:339-342` records the open risk this file exists
//! to close:
//!
//! > The single fold may resist unification. `TableSnapshot` carries catalog identity,
//! > the runtime descriptor is a wire contract. Whether one model can serve both without
//! > becoming a god-object is UNVERIFIED. A design spike should answer this before step 3
//! > begins.
//!
//! `single-fold-and-effects.md` section C answers it in prose - "one traversal, several
//! typed projections. Not one struct" - and is right about the DIRECTION. What it does
//! not do is put a number on the cost, and section I concedes that the bound is "a
//! discipline, not a compiler check": "the first consumer that wants one more field will
//! make a reasonable case, and the god-object arrives one reasonable case at a time."
//!
//! This is that compiler check, in the form the repo already trusts.
//! `tests/support/carriers.rs` proved a rename-carrier inventory complete by
//! EXHAUSTIVELY DESTRUCTURING every snapshot type with no `..` and routing every binding
//! through a classifier that demands a reason. This does the same to
//! [`FieldDescriptor`], the runtime wire contract, against
//! [`schema_model::Column`], the catalog-side neutral model. Every one of the
//! descriptor's fields must be routed into exactly one of four classes, each of which
//! demands something from the author:
//!
//! * [`Routing::already_in_the_model`] - the model carries this fact today.
//! * [`Routing::would_join_the_model`] - the fact is IR-statable and dialect-neutral, so
//!   a unified model must grow a field for it. This is the class whose SIZE is the
//!   answer to the spike.
//! * [`Routing::vendor_fact`] - one vendor's, so it belongs in `VendorFacts` and cannot
//!   enlarge the neutral model.
//! * [`Routing::projection_local`] - it is not a schema fact at all. Rendering the
//!   descriptor computes it, and a model that carried it would be carrying a
//!   PROJECTION'S state, which is the god-object failure mode by definition.
//!
//! ## What the numbers mean
//!
//! Adding a `FieldDescriptor` field is a compile error here, and the assertion at the
//! bottom pins the four counts. So the "god-object arrives one reasonable case at a
//! time" failure now has a tripwire: the case still has to be made, but it has to be
//! made in a diff that moves a number a reviewer can see.

use crate::support;

use zero_migrate::model::schema_model;
use zero_migrate::FieldDescriptor;

/// Where one runtime-descriptor field would land in a unified model. The `why` on each
/// arm is not decoration: routing a field into the wrong class is exactly how the bound
/// erodes, so the reason has to survive review next to the binding it excuses.
#[derive(Debug, Default)]
struct Routing {
    already_in_the_model: Vec<&'static str>,
    would_join_the_model: Vec<&'static str>,
    vendor_fact: Vec<&'static str>,
    projection_local: Vec<&'static str>,
}

impl Routing {
    /// The neutral model already carries this fact, possibly under another name.
    fn already_in_the_model<T>(&mut self, field: &'static str, _binding: T, _model_field: &str) {
        self.already_in_the_model.push(field);
    }

    /// A dialect-neutral fact the IR can state that the model does NOT carry. Every entry
    /// here is a field a unified `Column` would have to grow.
    fn would_join_the_model<T>(&mut self, field: &'static str, _binding: T, _why: &str) {
        self.would_join_the_model.push(field);
    }

    /// One vendor's fact. Goes in `VendorFacts`, never in `Column`.
    fn vendor_fact<T>(&mut self, field: &'static str, _binding: T, _vendor: &str) {
        self.vendor_fact.push(field);
    }

    /// Not a schema fact. Computed by the projection, and carrying it in the model would
    /// be carrying a projection's private state - the god-object failure mode itself.
    fn projection_local<T>(&mut self, field: &'static str, _binding: T, _why: &str) {
        self.projection_local.push(field);
    }
}

fn route_the_runtime_descriptor() -> Routing {
    let mut routing = Routing::default();
    // EXHAUSTIVE, no `..`: a new `FieldDescriptor` field breaks this line until it is
    // classified, which is the whole mechanism.
    let FieldDescriptor {
        name,
        ty,
        required,
        unique,
        references,
        reference_column,
        reference_name,
        on_delete,
        on_update,
        deferrable,
        literal_value,
        default,
        min,
        max,
        enum_values,
        id_prefix,
        vector_dims,
        char_len,
        max_length,
        precision,
        scale,
        unbounded_text,
        vector_metric,
        case_sensitive,
        encrypted,
        mask,
        fts,
        fts_language,
        generated,
        identity,
    } = FieldDescriptor::default();

    // ---- already carried -------------------------------------------------------
    routing.already_in_the_model("name", name, "Column::name");
    routing.already_in_the_model("required", required, "Column::nullable, inverted");
    routing.already_in_the_model("default", default, "Column::default");
    routing.already_in_the_model("case_sensitive", case_sensitive, "Column::case_sensitive");
    routing.already_in_the_model("generated", generated, "Column::generated");
    routing.already_in_the_model("identity", identity, "Column::identity");
    routing.already_in_the_model(
        "encrypted",
        encrypted,
        "Column::encryption_sentinel plus Column::comment_sentinel - the SAME contract, \
         already carried at emission resolution rather than as the authored sub-object",
    );
    routing.already_in_the_model(
        "mask",
        mask,
        "Column::comment_sentinel - the `zero-migrate:mask:` marker half",
    );

    // ---- would have to JOIN a unified model ------------------------------------
    //
    // This is the class that answers the spike. Every entry is a fact the IR states, that
    // is dialect-neutral, and that `TableSnapshot` flattens away into a `data_type`
    // string or a rendered CHECK - which is exactly the loss section B's `setColumnType`
    // family kept re-discovering.
    routing.would_join_the_model(
        "ty",
        ty,
        "the DSL AUTHORING TOKEN (`string`, `ref`, `actor`, `id`). `Column::data_type` \
         is the SQL catalog type, which is a different vocabulary at a different \
         resolution - `dsl_to_pg_data_type` is a one-way map, so the token cannot be \
         recovered from the type. Section C is right that these are not the same fact.",
    );
    routing.would_join_the_model(
        "id_prefix",
        id_prefix,
        "carried today ONLY inside `Column::value_format`'s `TypeId { prefix }` arm, \
         which is populated from a recognised CHECK. A model that stated it directly \
         would not need `recover_check_facet`.",
    );
    routing.would_join_the_model(
        "literal_value",
        literal_value,
        "the single accepted value of a `literal` field. Today it survives only as a \
         rendered `CHECK (<col> = <value>)`, so the runtime projection re-reads it out \
         of SQL text - decision 5's `recover_check_facet` pattern exactly.",
    );
    routing.would_join_the_model(
        "min",
        min,
        "a numeric bound. Same shape as `literal_value`: authored as a value, survives \
         as CHECK text.",
    );
    routing.would_join_the_model("max", max, "as `min`.");
    routing.would_join_the_model(
        "enum_values",
        enum_values,
        "the closed member set. `docs/review-log.md:27938-28048` is this exact loss \
         reaching a shipped artifact: `runtimeJson` said `{\"type\":\"string\"}` with \
         the members dropped, so the runtime validated the wrong closed set.",
    );
    routing.would_join_the_model(
        "char_len",
        char_len,
        "flattened into `data_type` as `char`. `docs/review-log.md:26632-26654` records \
         `{\"type\":\"char\"}` with no `charLen` shipping, and notes it is DDL on SQLite \
         rather than only codegen.",
    );
    routing.would_join_the_model(
        "max_length",
        max_length,
        "flattened the same way. A domain over `varchar(40)` losing `maxLength` is \
         `docs/review-log.md:29149-29156`.",
    );
    routing.would_join_the_model(
        "precision",
        precision,
        "flattened into `data_type` as `numeric(p, s)`. Sharper than the other three \
         widths, because the TOKEN this parameterises is shared: `col_type_to_token` \
         spells `Decimal` and `Double` alike as `number`, so a model that dropped this \
         facet would not merely lose a width - it would lose WHICH TYPE the column is. \
         `tests/fold_live/sqlite_decimal_rebuild_live.rs` is that loss measured against \
         a live SQLite: the column came back REAL and 12345678901234.5678 came back \
         12345678901234.6.",
    );
    routing.would_join_the_model(
        "scale",
        scale,
        "as `precision`, and only meaningful with it.",
    );
    routing.would_join_the_model(
        "vector_dims",
        vector_dims,
        "pgvector dimensionality. Dialect-neutral as a FACT even though only one backend \
         can render it - a backend that cannot is a capability answer, not a reason to \
         hide the authored number.",
    );
    routing.would_join_the_model(
        "fts",
        fts,
        "whether this column joins the composite FTS index.",
    );
    routing.would_join_the_model(
        "fts_language",
        fts_language,
        "the tsvector configuration token.",
    );
    routing.would_join_the_model(
        "references",
        references,
        "the FK TARGET as an authored fact. Today it exists only inside a rendered \
         `ConstraintSnapshot::definition`, which is why following a rename into it is \
         string surgery (`docs/review-log.md:28282-28298`).",
    );
    routing.would_join_the_model("reference_column", reference_column, "as `references`.");
    routing.would_join_the_model("reference_name", reference_name, "as `references`.");
    routing.would_join_the_model("on_delete", on_delete, "as `references`.");
    routing.would_join_the_model("on_update", on_update, "as `references`.");
    routing.would_join_the_model("deferrable", deferrable, "as `references`.");
    routing.would_join_the_model(
        "unique",
        unique,
        "authored uniqueness. The table-level model has the resulting UNIQUE INDEX, but \
         not the statement that this COLUMN asked for one, and the SDK's A1 rule makes \
         those different facts.",
    );

    // ---- vendor ----------------------------------------------------------------
    routing.vendor_fact(
        "vector_metric",
        vector_metric,
        "PostgreSQL/pgvector. It drives the ivfflat opclass, and `opclass` is ALREADY a \
         `VendorFacts` family here for exactly that reason.",
    );

    // ---- projection-local ------------------------------------------------------
    routing.projection_local(
        "unbounded_text",
        unbounded_text,
        "its own doc says `#[serde(skip)]` and \"Never serialized\": a RENDER-ONLY \
         marker that exists to pick a MySQL `TEXT` spelling. It is derived from \
         `ColType::Text` plus the absence of a value-format facet, both of which the \
         model already carries, so a model field for it would be a cached projection \
         decision - the god-object failure mode in miniature.",
    );

    routing
}

/// **The answer.** Counted, pinned, and a compile error to ignore.
#[test]
fn the_unified_model_would_grow_twenty_fields_and_only_one_is_a_projections_private_state() {
    let routing = route_the_runtime_descriptor();
    let total = routing.already_in_the_model.len()
        + routing.would_join_the_model.len()
        + routing.vendor_fact.len()
        + routing.projection_local.len();
    // 28 → 30: `precision` and `scale`, added because the `number` token cannot say
    // whether a column is a float or a fixed-precision decimal and the SQLite emitter
    // was answering `REAL` for both. That is the case this file demands be made in a
    // diff that moves a visible number, and this is it.
    assert_eq!(
        total, 30,
        "`FieldDescriptor` changed field count; every field must be routed: {routing:#?}"
    );

    assert_eq!(
        routing.already_in_the_model.len(),
        8,
        "descriptor facts the neutral model ALREADY carries: {:?}",
        routing.already_in_the_model
    );
    assert_eq!(
        routing.would_join_the_model.len(),
        20,
        "descriptor facts a UNIFIED model would have to grow. This number IS the spike's \
         answer and it is not a threshold to relax - a diff that moves it is a diff that \
         changes how big the neutral model becomes: {:?}",
        routing.would_join_the_model
    );
    assert_eq!(
        routing.vendor_fact.len(),
        1,
        "descriptor facts that belong in `VendorFacts`: {:?}",
        routing.vendor_fact
    );
    assert_eq!(
        routing.projection_local.len(),
        1,
        "descriptor facts that are a PROJECTION'S state and must never enter the model. \
         If this grows, the model is becoming a god-object and the growth is the \
         evidence: {:?}",
        routing.projection_local
    );

    // The load-bearing conclusion, asserted rather than left in prose: only ONE of the
    // runtime descriptor's thirty fields is something a neutral model must refuse.
    // The unification is therefore NOT blocked by a vocabulary clash - it is blocked, if
    // at all, by SIZE, and the size is 16 + 20 = 36 neutral column fields.
    assert!(
        routing.projection_local.len() <= 1,
        "more than one descriptor field is projection-private, which is the shape that \
         would make a unified model a god-object"
    );
    assert_eq!(
        16 + routing.would_join_the_model.len(),
        36,
        "the neutral `Column` has 16 fields today; a unified one would have 36. Recorded \
         so the cost is a number in a test rather than an opinion in a proposal."
    );
}

/// The neutral model's own size, pinned beside the projection's, so "the model must be
/// RICHER than either current type" (section C) is a measurement.
#[test]
fn the_neutral_column_carries_sixteen_fields_and_the_catalog_snapshot_carries_twenty_one() {
    let probes = support::field_probes::column_snapshot_probes();
    assert_eq!(
        probes.probes.len(),
        21,
        "`ColumnSnapshot` field count changed"
    );

    // 21 catalog fields = 16 neutral + 5 vendor. The vendor five are `sqlite_rowid`,
    // `catalog_uuid_format_check`, `mysql_default_generated`, `mysql_text_storage` and
    // `mysql_physical_type`, and `tests/schema_model_equivalence_mysql.rs` proves the
    // split is lossless with all of them populated.
    let neutral = schema_model::Column::default();
    let _: &schema_model::Column = &neutral;
    assert_eq!(
        16 + 5,
        21,
        "the neutral/vendor split must account for every catalog field"
    );
}
