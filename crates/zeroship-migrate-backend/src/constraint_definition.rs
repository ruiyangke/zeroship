//! The canonical constraint `definition` body - the ONE normal form a desired
//! snapshot's UNIQUE / PRIMARY KEY / FOREIGN KEY text is written in, on every
//! dialect.
//!
//! # Why it is here and not in the engine
//!
//! It was `pub(crate)` in `zeroship_migrate::render::declarative` until the vendor
//! crates became separately linkable. `crates/zeroship-migrate-mysql/src/backend/`
//! is being extracted into `zeroship-migrate-mysql`, and its `drift_sql.rs` BUILDS this
//! body: MySQL's `information_schema` stores no rendered constraint text - there is
//! no `pg_get_constraintdef` there - so the drift path has to synthesize the
//! comparison form itself. A vendor crate cannot depend on the engine (the engine
//! depends on all three vendors, so the edge back is a cycle Cargo refuses), so an
//! item its drift path reaches has to live at or below this crate.
//!
//! # Comparison text, NEVER an emitted identifier route
//!
//! Read that twice, because the bytes look like DDL and are not. Every constraint
//! body is built ONCE in the `pg_get_constraintdef` normal form - conditional
//! double quotes, `ON UPDATE` before `ON DELETE`, the canonical default action
//! omitted - so that a desired snapshot and a live introspected one compare equal
//! without a per-dialect special case. It is a COMPARISON codec.
//!
//! A vendor that reaches for [`quote_ident_if_needed`] to spell an identifier it is
//! about to EMIT has read this module as a quoting helper, and it is not one:
//! MySQL delimits identifiers with backticks, so emitting `"x"` from here would be
//! wrong in a way no comparison test can see. `zeroship-migrate-mysql`'s DDL half
//! re-spells the body it gets from here (`mysql_requote_sql`) precisely because the
//! two are different jobs. `constraint_definition_is_comparison_text` is the census
//! that keeps that rule after `pub(crate)` stopped being able to state it.
//!
//! # What resolves a vendor, and what does not
//!
//! [`quote_ident_if_needed`], [`constraintdef_cols`], [`normalize_fk_action`] and
//! [`NOT_VALID_DEFINITION_SUFFIX`] name no dialect at all - they are the neutral
//! half, and they moved unchanged.
//!
//! [`fk_definition`] and [`fk_constraint_snapshot`] do need one vendor fact apiece
//! (its canonical FK action fold, its FK target spelling, its capability row), so
//! they take a `&BackendVendor` PARAMETER. They used to resolve one from a
//! `DialectId` through the engine's registry, which is exactly what a vendor crate
//! may not do. The engine RESOLVES and hands the vendor down; a vendor hands its OWN
//! `VENDOR` down and never asks. `zeroship_migrate::render::declarative` keeps the
//! dialect-taking shims its in-engine callers use, the same split
//! `existence_probe::decide` draws.

use std::fmt::Write as _;

use zeroship_migrate_ir::backend::Capability;

use crate::registry::BackendVendor;
use crate::snapshot::{quote_constraint_definition_ident, ConstraintSnapshot};

/// The PG keywords whose category is NOT `UNRESERVED` (i.e. reserved,
/// type/function-name, or column-name keywords). `quote_identifier` - and thus
/// `pg_get_constraintdef` - wraps an identifier in double quotes iff it is not a
/// "safe" bare identifier OR it collides with one of THESE keywords (an unreserved
/// keyword is rendered bare). Sourced from `pg_get_keywords() WHERE catcode<>'U'`
/// on PG 17. Used by [`quote_ident_if_needed`] so the FK referenced-table body we
/// build matches the live catalog byte-for-byte: a table/schema named
/// `order`/`user`/`select` (each passes `validate_collection`/`is_safe_schema_ident`
/// but is reserved) renders QUOTED in the catalog - and now here too - so the
/// desired-vs-live FK body re-diffs clean instead of phantom-dropping.
///
/// NOT A DEFECT WHEN A SQLITE PATH READS THIS CODEC. [`quote_ident_if_needed`] and
/// [`constraintdef_cols`] build the `pg_get_constraintdef` comparison form on
/// purpose. SQLite and MySQL drift normalization compare against that form; this is
/// comparison text, never an emitted identifier route.
const CONSTRAINT_DEFINITION_KEYWORDS_REQUIRING_QUOTES: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "authorization",
    "between",
    "bigint",
    "binary",
    "bit",
    "boolean",
    "both",
    "case",
    "cast",
    "char",
    "character",
    "check",
    "coalesce",
    "collate",
    "collation",
    "column",
    "concurrently",
    "constraint",
    "create",
    "cross",
    "current_catalog",
    "current_date",
    "current_role",
    "current_schema",
    "current_time",
    "current_timestamp",
    "current_user",
    "dec",
    "decimal",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "exists",
    "extract",
    "false",
    "fetch",
    "float",
    "for",
    "foreign",
    "freeze",
    "from",
    "full",
    "grant",
    "greatest",
    "group",
    "grouping",
    "having",
    "ilike",
    "in",
    "initially",
    "inner",
    "inout",
    "int",
    "integer",
    "intersect",
    "interval",
    "into",
    "is",
    "isnull",
    "join",
    "json_array",
    "json_arrayagg",
    "json_object",
    "json_objectagg",
    "lateral",
    "leading",
    "least",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "national",
    "natural",
    "nchar",
    "none",
    "normalize",
    "not",
    "notnull",
    "null",
    "nullif",
    "numeric",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "out",
    "outer",
    "overlaps",
    "overlay",
    "placing",
    "position",
    "precision",
    "primary",
    "real",
    "references",
    "returning",
    "right",
    "row",
    "select",
    "session_user",
    "setof",
    "similar",
    "smallint",
    "some",
    "substring",
    "symmetric",
    "system_user",
    "table",
    "tablesample",
    "then",
    "time",
    "timestamp",
    "to",
    "trailing",
    "treat",
    "trim",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "values",
    "varchar",
    "variadic",
    "verbose",
    "when",
    "where",
    "window",
    "with",
    "xmlattributes",
    "xmlconcat",
    "xmlelement",
    "xmlexists",
    "xmlforest",
    "xmlnamespaces",
    "xmlparse",
    "xmlpi",
    "xmlroot",
    "xmlserialize",
    "xmltable",
];

/// Quote an identifier ONLY when Postgres' own `quote_identifier` would - i.e.
/// mirror what `pg_get_constraintdef` emits. An identifier is left BARE iff it is a
/// "safe" lowercase identifier (starts with `[a-z_]`, all chars `[a-z0-9_]`) AND is
/// not a keyword requiring quotes (the module-private
/// `CONSTRAINT_DEFINITION_KEYWORDS_REQUIRING_QUOTES` table above - deliberately not
/// linked, and deliberately not `pub`: it is this codec's input, not a keyword list
/// any caller should be reading); otherwise it is double-quoted (mixed-case,
/// leading digit, reserved word, ...).
///
/// This is the seam the FK referenced-table body uses so the desired snapshot
/// round-trips byte-for-byte against the live `pg_get_constraintdef` output
/// (an unconditional `quote_ident` would over-quote a normal lowercase name like
/// `parent` -> `"parent"`, which the catalog renders bare -> a phantom FK re-create on
/// every diff). It also closes the latent injection/wrong-resolution seam: a
/// reserved-word or mixed-case schema/target now renders quoted (correct
/// resolution), not as a bare keyword.
#[must_use]
pub fn quote_ident_if_needed(ident: &str) -> String {
    let safe_bare = !ident.is_empty()
        && ident.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && ident
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !CONSTRAINT_DEFINITION_KEYWORDS_REQUIRING_QUOTES.contains(&ident);
    if safe_bare {
        ident.to_string()
    } else {
        quote_constraint_definition_ident(ident)
    }
}

/// Spell a `pg_get_constraintdef`-matching column list for a UNIQUE / PRIMARY KEY
/// constraint `definition` body - `<col>, <col>, ...` with CONDITIONAL per-column
/// quoting ([`quote_ident_if_needed`]: bare for a safe lowercase ident, double-
/// quoted for reserved/mixed-case). This is the SINGLE source of the constraintdef
/// body spelling: the engine's offline fold (`zeroship_migrate::render::fold`), the IR
/// lower's snapshot half (`zeroship_migrate::render::lower`) and every backend's drift
/// normalization consume it, so the folded, the lower-emitted and the introspected
/// UNIQUE/PK `definition` cannot drift apart (an unconditional quote would
/// phantom-diff `UNIQUE ("handle")` against the catalog's `UNIQUE (handle)`).
#[must_use]
pub fn constraintdef_cols(cols: &[String]) -> String {
    cols.iter()
        .map(|c| quote_ident_if_needed(c))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The trailing token `pg_get_constraintdef` renders while a constraint's
/// `convalidated` is false, and the exact substring a `VALIDATE CONSTRAINT` fold
/// removes again.
pub const NOT_VALID_DEFINITION_SUFFIX: &str = " NOT VALID";

/// Normalise an FK action to the SQL keyword form Postgres accepts.
///
/// The DIALECT-NEUTRAL half: it maps the author's spelling (`set_null`, `setNull`,
/// `SET NULL`, ...) onto one keyword, and stops there. Folding that keyword into a
/// vendor's canonical catalog form - InnoDB collapsing `RESTRICT` into `NO ACTION`,
/// say - is the vendor's answer, asked for by [`normalize_fk_action_for_vendor`].
///
/// `zeroship_migrate::schema::query::normalize_fk_action` is the engine's re-export of
/// this, kept so the out-of-repo data plane's import path is unchanged.
#[must_use]
pub fn normalize_fk_action(s: Option<&str>) -> &'static str {
    match s.unwrap_or("no action").to_ascii_lowercase().as_str() {
        "cascade" => "CASCADE",
        "set null" | "set_null" | "setnull" => "SET NULL",
        "set default" | "set_default" | "setdefault" => "SET DEFAULT",
        "no action" | "no_action" | "noaction" => "NO ACTION",
        "restrict" => "RESTRICT",
        _ => "RESTRICT",
    }
}

/// Normalise an FK action into `vendor`'s canonical comparison/render form.
///
/// MySQL/InnoDB has no deferred constraint checks, so `RESTRICT` and `NO ACTION`
/// are the same immediate-reject default there. Postgres and SQLite keep them
/// distinct, because their catalog forms do. Which of those is true is the vendor's
/// fact to state, so it is asked rather than matched on here.
#[must_use]
pub fn normalize_fk_action_for_vendor(s: Option<&str>, vendor: &BackendVendor) -> &'static str {
    vendor.schema.canonical_fk_action(normalize_fk_action(s))
}

/// Build a FOREIGN KEY definition body in `vendor`'s canonical catalog spelling, so
/// the desired snapshot round-trips to the live introspected constraint.
///
/// Empirically (probed against PG 17), `pg_get_constraintdef` renders a FK as:
///
/// ```text
/// FOREIGN KEY (<cols>) REFERENCES <schema>.<target>(<cols>)[ ON UPDATE <u>][ ON DELETE <d>][ DEFERRABLE [INITIALLY DEFERRED]]
/// ```
///
/// with two normalisations the DDL spelling does NOT have:
/// - **`ON UPDATE` precedes `ON DELETE`** (the reverse of plugin-db's emitted
///   DDL, which writes `ON DELETE <d> ON UPDATE <u>`); and
/// - a **`NO ACTION`** action clause is **OMITTED entirely** (it is the catalog
///   default - `confdeltype`/`confupdtype` = `'a'`), so a FK with both actions
///   `NO ACTION` renders with no action clauses at all.
///
/// On Postgres, `RESTRICT`, `CASCADE`, and `SET NULL` are rendered explicitly.
/// On MySQL, `RESTRICT` and `NO ACTION` are semantically identical and both fold
/// to the omitted default; `CASCADE`, `SET NULL`, and `SET DEFAULT` still render.
///
/// Which of those is true is never matched on here: the fold is
/// [`normalize_fk_action_for_vendor`], the target spelling is
/// `SchemaRenderer::canonical_fk_target`, and the two capability gates read
/// `vendor`'s own descriptor row. The caller RESOLVES a vendor; this asks it.
#[must_use]
// Canonical FK rendering needs each independent FK semantic and the vendor rules.
#[allow(clippy::too_many_arguments)]
pub fn fk_definition(
    local_columns: &[String],
    project_schema: &str,
    target: &str,
    references_columns: &[String],
    on_delete: Option<&str>,
    on_update: Option<&str>,
    deferrable: bool,
    initially_deferred: bool,
    not_valid: bool,
    vendor: &BackendVendor,
) -> String {
    // quote the referenced schema + table the SAME way
    // `pg_get_constraintdef` does (conditional: bare for safe lowercase names,
    // double-quoted for reserved-word/mixed-case), so the desired FK body matches
    // the live catalog byte-for-byte (over-quoting would phantom-diff a normal
    // lowercase `parent`) AND a reserved-word/mixed-case schema or target resolves
    // correctly instead of being emitted as a bare keyword.
    //
    // The LOCAL FK column is quoted the SAME conditional way as the schema/target
    // (and as the UNIQUE/PK body via `constraintdef_cols`): `pg_get_constraintdef`
    // renders `FOREIGN KEY ("order")` for a reserved-word/mixed-case column, so a
    // raw `FOREIGN KEY (order)` would phantom-diff the FK `definition` (the fold
    // reuses it, and `ConstraintSnapshot` has FULL Eq) AND mis-resolve `order` as
    // the keyword. Over-quoting a safe lowercase column would equally phantom-diff
    // the catalog's bare body - hence conditional (`quote_ident_if_needed`).
    let ref_cols = if references_columns.is_empty() {
        vec!["id".to_string()]
    } else {
        references_columns.to_vec()
    };
    let target_name = vendor.schema.canonical_fk_target(
        &quote_ident_if_needed(project_schema),
        &quote_ident_if_needed(target),
    );
    let mut def = format!(
        "FOREIGN KEY ({}) REFERENCES {}({})",
        constraintdef_cols(local_columns),
        target_name,
        constraintdef_cols(&ref_cols),
    );
    let on_update = normalize_fk_action_for_vendor(on_update, vendor);
    let on_delete = normalize_fk_action_for_vendor(on_delete, vendor);
    // Catalog definitions render ON UPDATE before ON DELETE and omit the
    // dialect's canonical default action. On MySQL, RESTRICT and NO ACTION both
    // canonicalize to NO ACTION because InnoDB has no deferred checks.
    if on_update != "NO ACTION" {
        let _ = write!(def, " ON UPDATE {on_update}");
    }
    if on_delete != "NO ACTION" {
        let _ = write!(def, " ON DELETE {on_delete}");
    }
    if deferrable
        && vendor
            .descriptor
            .capabilities
            .contains(Capability::DeferrableConstraint)
    {
        def.push_str(" DEFERRABLE");
        if initially_deferred {
            def.push_str(" INITIALLY DEFERRED");
        }
    }
    // `pg_get_constraintdef` appends ` NOT VALID` for as long as `convalidated` is
    // false, so a desired body that omits it phantom-diffs every unvalidated
    // constraint against the catalog. PostgreSQL-only: `NOT VALID` is refused off
    // that dialect at validate, so the flag can never arrive here on another leg,
    // and gating on the vendor keeps a SQLite rebuild from splicing the token into
    // a `CREATE TABLE` clause it would not parse.
    if not_valid
        && vendor
            .descriptor
            .capabilities
            .contains(Capability::AlterTableValidateConstraint)
    {
        def.push_str(NOT_VALID_DEFINITION_SUFFIX);
    }
    def
}

/// The whole FOREIGN KEY [`ConstraintSnapshot`] - [`fk_definition`]'s body under an
/// already-decided `name`.
///
/// The name is a PARAMETER rather than something derived here, and that is the one
/// deliberate change the move made. Deriving an unnamed FK's name is AUTHORING
/// policy: it is `<table>_<cols>_fkey` capped to the target's declared identifier
/// budget, which is why it needs the table (this function otherwise never sees one)
/// and the engine's `plan::author::cap_ident_name`. The engine keeps that half in
/// `zeroship_migrate::render::declarative::ir_fk_constraint_snapshot_for_columns`, which
/// still takes `table` + `explicit_name` and calls through to here.
///
/// A backend never needs the derived half: a constraint it read out of a live
/// catalog always arrives already named.
#[must_use]
// The snapshot adds its stable name while preserving every canonical FK semantic.
#[allow(clippy::too_many_arguments)]
pub fn fk_constraint_snapshot(
    name: String,
    project_schema: &str,
    local_columns: &[String],
    references_table: &str,
    references_columns: &[String],
    on_delete: Option<&str>,
    on_update: Option<&str>,
    deferrable: bool,
    initially_deferred: bool,
    not_valid: bool,
    vendor: &BackendVendor,
) -> ConstraintSnapshot {
    let definition = fk_definition(
        local_columns,
        project_schema,
        references_table,
        references_columns,
        on_delete,
        on_update,
        deferrable,
        initially_deferred,
        not_valid,
        vendor,
    );
    ConstraintSnapshot {
        name,
        kind: "FOREIGN KEY".to_string(),
        definition,
        comment: None,
        cascade_columns: None,
    }
}
