//! PR4 deliverable (C) — `new` scaffold + the generate-from-diff op.* `.ts`/IR
//! synthesizer (D). Both emit DETERMINISTIC op.* migration SOURCE: any time / uuid
//! seed default is the DB-evaluated `c.fn.now()` / `c.fn.genRandomUuid()` synth
//! scalar, NEVER `Date.now()` / `Math.random()` / `crypto.randomUUID()` — so the
//! scaffold is deterministic BY CONSTRUCTION (§4.3) and recording it surfaces ZERO
//! determinism warnings.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::ir::{
    ColType, IndexElement, IrColumn, IrDefault, Op, SynthDefaultFn, CURRENT_IR_VERSION,
    SYSTEM_FIELD_NAMES, TableRuntimeOptions, TableStrictness,
};
use crate::plan::loader::{is_valid_migration_name, suggest_migration_name};
use crate::render::declarative::{
    is_system_managed_constraint, is_system_managed_index, DesiredSchema,
};
use crate::{
    ColumnSnapshot, ConstraintSnapshot, IndexElementSnapshot, IndexSnapshot, MigrationIr,
    SchemaSnapshot, TableSnapshot,
};

/// A scaffold / generate error.
#[derive(Debug, thiserror::Error)]
pub enum ScaffoldError {
    /// The migration name violates the `[A-Za-z0-9_]+` grammar.
    #[error("migration name {name:?} is invalid (must be [A-Za-z0-9_]+); suggested: {suggestion:?}")]
    InvalidName {
        /// The offending name.
        name: String,
        /// A normalized suggestion (may be empty).
        suggestion: String,
    },
    /// The target file already exists — `new` never clobbers.
    #[error("migration file {0} already exists (refusing to clobber)")]
    AlreadyExists(String),
    /// A desired column's `data_type` has no portable op.* `ColType` reverse-mapping
    /// (a goodie type that `generate`'s structural-delta path does not yet synth).
    #[error("column {table}.{column} has data_type {data_type:?} which generate cannot synthesize as a portable op (author it by hand)")]
    UnsupportedColumnType {
        /// The table.
        table: String,
        /// The column.
        column: String,
        /// The unmapped data_type.
        data_type: String,
    },
    /// A desired USER index is not synthesizable as a portable `createIndex` op
    /// (a non-`btree` access method — vector ANN / GIN / GiST / FTS5 — or an
    /// expression / partial index). Rather than SILENTLY drop it (breaking
    /// re-diff-to-zero), `generate` FAILS CLOSED — author the index by hand.
    #[error("index {table}.{name} ({reason}) cannot be synthesized as a portable createIndex op (author it by hand)")]
    UnsupportedIndex {
        /// The owning table.
        table: String,
        /// The index name.
        name: String,
        /// Why it is not synthesizable (the non-btree method / expression).
        reason: String,
    },
    /// A desired USER constraint (FOREIGN KEY / CHECK / standalone UNIQUE) carries
    /// only its `pg_get_constraintdef` definition TEXT, not structured operands, so
    /// `generate` cannot reverse it into a portable `addConstraint` op without an
    /// unsound text parse-back. Rather than SILENTLY drop it (breaking
    /// re-diff-to-zero), `generate` FAILS CLOSED — author the constraint by hand.
    #[error("constraint {table}.{name} (kind {kind:?}) cannot be synthesized as a portable addConstraint op (author it by hand)")]
    UnsupportedConstraint {
        /// The owning table.
        table: String,
        /// The constraint name.
        name: String,
        /// The constraint kind (`FOREIGN KEY` / `CHECK` / `UNIQUE`).
        kind: String,
    },
}

/// Format a 14-digit `YYYYMMDDHHMMSS` UTC timestamp (the migration filename
/// prefix). Civil-from-days, no date crate.
#[must_use]
pub fn timestamp_14(now: SystemTime) -> String {
    let secs: i64 = now
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60,
    );
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}{month:02}{day:02}{hh:02}{mm:02}{ss:02}")
}

const fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The deterministic-by-construction `new <name>` op.* `.ts` scaffold (deliverable
/// C). Validates `name` against the migration grammar (rejects + suggests; NEVER
/// auto-renames). The scaffold demonstrates the determinism-correct default
/// pattern: a uuid seed defaults to `c.fn.genRandomUuid()`, a timestamp seed to
/// `c.fn.now()` — the DB-evaluated synth scalars, never a host clock / RNG.
///
/// # Errors
/// [`ScaffoldError::InvalidName`].
pub fn scaffold_new_ts(name: &str) -> Result<String, ScaffoldError> {
    if !is_valid_migration_name(name) {
        return Err(ScaffoldError::InvalidName {
            name: name.to_string(),
            suggestion: suggest_migration_name(name),
        });
    }
    Ok(format!(
        r#"// Migration: {name}
//
// Authored against the @zeroship/migrate op.* DSL. Build/gen-types record this
// through the sandboxed recorder and keep the canonical IR in memory.
//
// DETERMINISM (§4.3): any time/uuid seed column MUST default to the DB-evaluated
// synth scalar — use the c.fn.now / c.fn.genRandomUuid synth defaults (shown
// below) so the value is computed at apply time. Do NOT seed defaults from a host
// clock or RNG: a host-side timestamp/random would bake a frozen value into each
// transient recording and may diverge across replays. The determinism lint flags
// those host accessors as a warning the AI loop self-corrects on.
import {{ table, t }} from "@zeroship/migrate";

export function up() {{
  // Replace this create() with your schema change. The platform injects the
  // system id/created_at/updated_at columns for you, so the example needs none.
  table("{name}").create({{
    columns: {{
      title: t.text().notNull(),
    }},
  }});

  // When you DO need a SEEDED uuid/timestamp column, default it to the DB-evaluated
  // synth scalar so the value is computed at apply time — deterministic by
  // construction, NEVER a host clock / RNG (those bake a frozen value into the
  // transient recording and may diverge across replays). Uncomment to use:
  //   table("{name}").column("token").add({{ type: t.uuid().notNull().default({{ fn: "genRandomUuid" }}) }});
  //   table("{name}").column("expires_at").add({{ type: t.timestamp().notNull().default({{ fn: "now" }}) }});
}}

export function down() {{
  table("{name}").drop();
}}
"#
    ))
}

/// Reverse-map a DESIRED-snapshot column `data_type` (the PG `information_schema`
/// spelling `desired_snapshot` emits) into the op.* [`ColType`] whose IR lowering
/// reproduces the SAME `data_type` — so the synthesized `createTable`/`addColumn`
/// re-diffs to zero. Limited to the portable structural subset (`generate`'s scope
/// per §7.1); a goodie type (`vector(N)`, `geography(…)`, encrypted/mask sentinels)
/// is rejected (author by hand).
fn col_type_for_data_type(
    table: &str,
    col: &ColumnSnapshot,
) -> Result<ColType, ScaffoldError> {
    let dt = col.data_type.trim().to_ascii_lowercase();
    let ty = match dt.as_str() {
        "text" | "character varying" | "varchar" => ColType::Text,
        "integer" | "int4" | "int" => ColType::Int,
        "bigint" | "int8" => ColType::BigInt,
        "double precision" | "real" | "float8" => ColType::Float,
        "boolean" | "bool" => ColType::Bool,
        "jsonb" | "json" => ColType::Json,
        "timestamp with time zone" | "timestamptz" | "timestamp" => ColType::Timestamp,
        "uuid" => ColType::Uuid,
        "bytea" => ColType::Bytea,
        "numeric" => ColType::Decimal {
            precision: 38,
            scale: 9,
        },
        _ => {
            return Err(ScaffoldError::UnsupportedColumnType {
                table: table.to_string(),
                column: col.name.clone(),
                data_type: col.data_type.clone(),
            })
        }
    };
    Ok(ty)
}

/// The default for a generated column. `generate` emits NO column defaults: the
/// declarative differ (the platform deploy path) injects the system-field defaults
/// (`id`/`created_at`/…) through its own shared builder, and a user-authored synth
/// default in a `createTable` is validate-refused until the default renderer lands.
/// Emitting a default here would make the
/// generated `.ir.json` un-applyable. The drift comparison is on `data_type` +
/// `nullable` only (never `default`), so omitting defaults still re-diffs to zero.
/// Kept as a function (returning `None`) to document the deliberate choice + keep a
/// single seam if a future render wave lands.
fn system_default_for(_col_name: &str, _ty: &ColType) -> Option<IrDefault> {
    None
}

/// Synthesize a standalone `Op::CreateIndex` from a desired USER index, or FAIL
/// CLOSED if it is not a portable plain index. Only a `btree` access-method,
/// column-list, non-partial / non-expression index is synthesizable; a non-btree
/// method (vector ANN / GIN / GiST / FTS5) or an expression / partial index carries
/// shape the portable scaffold path cannot faithfully re-author from catalog SQL —
/// so rather than drop it silently (breaking re-diff-to-zero), reject.
fn synth_index_op(table: &str, idx: &IndexSnapshot) -> Result<Op, ScaffoldError> {
    let method = idx.access_method.trim().to_ascii_lowercase();
    if method != "btree" {
        return Err(ScaffoldError::UnsupportedIndex {
            table: table.to_string(),
            name: idx.name.clone(),
            reason: format!("non-btree access method {:?}", idx.access_method),
        });
    }
    if idx.predicate.is_some()
        || idx
            .elements
            .iter()
            .any(|element| matches!(element, IndexElementSnapshot::Expr(_)))
    {
        return Err(ScaffoldError::UnsupportedIndex {
            table: table.to_string(),
            name: idx.name.clone(),
            reason: "expression / partial index".to_string(),
        });
    }
    if idx.columns.is_empty() {
        // A btree index with no key columns is an expression index in disguise.
        return Err(ScaffoldError::UnsupportedIndex {
            table: table.to_string(),
            name: idx.name.clone(),
            reason: "no key columns".to_string(),
        });
    }
    let columns = if idx.elements.is_empty() {
        idx.columns
            .iter()
            .cloned()
            .map(|name| IndexElement::Column { name })
            .collect()
    } else {
        idx.elements
            .iter()
            .map(|element| match element {
                IndexElementSnapshot::Column(name) => {
                    Ok(IndexElement::Column { name: name.clone() })
                }
                IndexElementSnapshot::Expr(_) => Err(ScaffoldError::UnsupportedIndex {
                    table: table.to_string(),
                    name: idx.name.clone(),
                    reason: "expression / partial index".to_string(),
                }),
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    Ok(Op::CreateIndex {
        table: table.to_string(),
        columns,
        name: Some(idx.name.clone()),
        unique: if idx.unique { Some(true) } else { None },
        using: None,
        r#where: None,
        concurrently: None,
        schema: None,
        existence_guard: None,
    })
}

/// A USER constraint (FK / CHECK / standalone UNIQUE) carries only its
/// `pg_get_constraintdef` definition TEXT, not the structured operands an
/// `addConstraint` op needs — reversing the text would be an unsound parse-back. So
/// `generate` FAILS CLOSED on any user constraint rather than silently dropping it
/// (which would break re-diff-to-zero). Author such a constraint by hand.
fn reject_user_constraint(table: &str, c: &ConstraintSnapshot) -> ScaffoldError {
    ScaffoldError::UnsupportedConstraint {
        table: table.to_string(),
        name: c.name.clone(),
        kind: c.kind.clone(),
    }
}

/// Append the synthesized standalone index/constraint ops for `table`'s desired
/// snapshot that the CREATE-TABLE lowering does NOT auto-inject. Each
/// platform-managed object (pkey + system-field indexes, pkey constraint) is
/// SKIPPED; each not-already-live USER index is synthesized as a `createIndex`;
/// each not-already-live USER constraint FAILS CLOSED (no silent drop). `live_table`
/// is `Some` for an existing table (skip objects already present) / `None` for a
/// fresh table.
fn append_table_index_constraint_ops(
    ops: &mut Vec<SynthOp>,
    table: &str,
    want: &TableSnapshot,
    live_table: Option<&TableSnapshot>,
) -> Result<(), ScaffoldError> {
    for idx in &want.indexes {
        if is_system_managed_index(table, &idx.name) {
            continue; // injected by lower_create_table — never re-emit.
        }
        if live_table.is_some_and(|h| h.indexes.iter().any(|i| i.name == idx.name)) {
            continue; // already live — no delta.
        }
        ops.push(SynthOp {
            op: synth_index_op(table, idx)?,
            todo: None,
        });
    }
    for c in &want.constraints {
        if is_system_managed_constraint(table, &c.name) {
            continue; // the implicit PK — injected by lower_create_table.
        }
        if live_table.is_some_and(|h| h.constraints.iter().any(|x| x.name == c.name)) {
            continue; // already live — no delta.
        }
        return Err(reject_user_constraint(table, c));
    }
    Ok(())
}

/// One synthesized op with its open-obligation marker (if any).
struct SynthOp {
    op: Op,
    /// A machine-readable `// @zeroship-todo backfill: …` marker (§8.8/§7.1) when
    /// this op is a data-open-obligation (a NON-NULL column add with no default).
    todo: Option<String>,
}

/// Synthesize the op.* ops + the open-obligation markers for the structural delta
/// between `desired` and the live `SchemaSnapshot` (deliverable D). New tables →
/// `createTable`, new columns → `addColumn`, dropped tables/columns →
/// `dropTable`/`dropColumn`. A NON-NULL column add with no default emits a
/// machine-readable backfill TODO marker.
fn synth_delta_ops(
    desired: &DesiredSchema,
    live: &SchemaSnapshot,
) -> Result<Vec<SynthOp>, ScaffoldError> {
    let mut ops = Vec::new();

    // New tables + new columns (additive).
    for (table, want) in &desired.snapshot.tables {
        match live.tables.get(table) {
            None => {
                // CREATE TABLE with the USER columns only. The platform system
                // fields (`id`/`created_at`/…) are injected by the IR lowering's
                // shared snapshot-builder, NOT re-declared here — re-declaring `id`
                // as a plain column is rejected at lower (it is the reserved system
                // PK). So filter them out (mirrors how the differ author-builds a
                // createTable: user columns + engine-injected system fields).
                let mut columns = Vec::new();
                for col in &want.columns {
                    if SYSTEM_FIELD_NAMES.contains(&col.name.as_str()) {
                        continue;
                    }
                    let ty = col_type_for_data_type(table, col)?;
                    let default = system_default_for(&col.name, &ty);
                    columns.push(IrColumn {
                        name: col.name.clone(),
                        ty,
                        nullable: if col.nullable { None } else { Some(false) },
                        default,
                        unique: None,
                        // Migration-first P2a (§2b): id_prefix / vector_metric are
                        // DECLARED-ONLY hints DB introspection cannot recover, so a
                        // scaffold from a live catalog leaves them absent (faithful
                        // to the scaffold's fail-closed posture — it never invents a
                        // declared-only facet it cannot observe).
                        id_prefix: None,
                        vector_metric: None,
                        mask: None,
                        generated: None,
                        identity: None,
                    });
                }
                ops.push(SynthOp {
                    op: Op::CreateTable {
                        name: table.clone(),
                        columns,
                        primary_key: None,
                        constraints: Vec::new(),
                        indexes: Vec::new(),
                        runtime_options: Some(want.runtime_options.clone()),
                        schema: None,
                        existence_guard: None,
                    },
                    todo: None,
                });
                // The CREATE-TABLE op carries USER columns only (constraints/indexes
                // are emitted as standalone ops so each lowers through the gated
                // createIndex/addConstraint path). Synthesize the USER indexes +
                // fail-closed on USER constraints — NEVER silently dropped.
                append_table_index_constraint_ops(&mut ops, table, want, None)?;
            }
            Some(have) => {
                // New columns on an existing table.
                for col in &want.columns {
                    if SYSTEM_FIELD_NAMES.contains(&col.name.as_str()) {
                        continue; // system fields are platform-managed, never added here
                    }
                    if have.columns.iter().any(|c| c.name == col.name) {
                        continue;
                    }
                    let ty = col_type_for_data_type(table, col)?;
                    let default = system_default_for(&col.name, &ty);
                    // Open obligation: a NON-NULL add with no default needs a backfill.
                    let todo = if !col.nullable && default.is_none() {
                        Some(format!(
                            "// @zeroship-todo backfill: {table}.{} — NON-NULL column added with no default; backfill existing rows before enforcing NOT NULL",
                            col.name
                        ))
                    } else {
                        None
                    };
                    ops.push(SynthOp {
                        op: Op::AddColumn {
                            table: table.clone(),
                            column: col.name.clone(),
                            ty,
                            nullable: if col.nullable { None } else { Some(false) },
                            default,
                            // The scaffolder synthesizes an AddColumn from a live-snapshot
                            // diff; the snapshot column type carries no declared vector
                            // metric / standalone mask facet at this layer (op.* authoring,
                            // not scaffold, is the source of truth for those — #173/#174).
                            vector_metric: None,
                            mask: None,
                            generated: None,
                            identity: None,
                            schema: None,
                            existence_guard: None,
                        },
                        todo,
                    });
                }
                // Additive indexes/constraints on an existing table: synthesize new
                // USER indexes, fail-closed on new USER constraints, skip what is
                // already live or platform-managed. NEVER silently dropped.
                append_table_index_constraint_ops(&mut ops, table, want, Some(have))?;
            }
        }
    }

    // Dropped tables + dropped columns (destructive — emitted, gated downstream).
    for (table, have) in &live.tables {
        match desired.snapshot.tables.get(table) {
            None => {
                ops.push(SynthOp {
                    op: Op::DropTable {
                        table: table.clone(),
                        cascade: None,
                        schema: None,
                        existence_guard: None,
                    },
                    todo: None,
                });
            }
            Some(want) => {
                for col in &have.columns {
                    if SYSTEM_FIELD_NAMES.contains(&col.name.as_str()) {
                        continue; // never drop platform system fields
                    }
                    if want.columns.iter().any(|c| c.name == col.name) {
                        continue;
                    }
                    ops.push(SynthOp {
                        op: Op::DropColumn {
                            table: table.clone(),
                            column: col.name.clone(),
                            schema: None,
                            existence_guard: None,
                        },
                        todo: None,
                    });
                }
            }
        }
    }

    Ok(ops)
}

/// The result of [`generate_ops`]: the directly-derived IR (the source of truth),
/// the equivalent human-readable op.* `.ts` body, and the open-obligation markers.
#[derive(Debug, Clone)]
pub struct GeneratedMigration {
    /// The directly-derived `MigrationIr` (the `.ir.json` source of truth).
    pub ir: MigrationIr,
    /// The human-readable op.* `.ts` body (named imports, `export default {up,down}`)
    /// whose recorded ops round-trip to `ir`'s checksum (autogenerate parity).
    pub ts_body: String,
    /// The machine-readable `// @zeroship-todo backfill: …` markers (§8.8/§7.1).
    pub todos: Vec<String>,
    /// `true` when the delta is empty (no-op).
    pub is_empty: bool,
}

/// Synthesize the directly-derived IR + the equivalent op.* `.ts` from the
/// declarative diff (deliverable D). The IR is the source of truth; the `.ts` is the
/// human-readable mirror whose recorded checksum must equal the IR's (verified in
/// the e2e). Open-obligation backfill markers ride in `todos` and are embedded as
/// comments in the `.ts`.
///
/// # Errors
/// [`ScaffoldError`] for an unmappable column type.
pub fn generate_ops(
    name: &str,
    owner_app: &str,
    desired: &DesiredSchema,
    live: &SchemaSnapshot,
) -> Result<GeneratedMigration, ScaffoldError> {
    let synth = synth_delta_ops(desired, live)?;
    let ops: Vec<Op> = synth.iter().map(|s| s.op.clone()).collect();
    let todos: Vec<String> = synth.iter().filter_map(|s| s.todo.clone()).collect();
    let is_empty = ops.is_empty();

    let ir = MigrationIr {
        ir_version: CURRENT_IR_VERSION,
        name: name.to_string(),
        owner_app: owner_app.to_string(),
        ops: ops.clone(),
        flags: Default::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    };

    let ts_body = render_ts(name, &synth);
    Ok(GeneratedMigration {
        ir,
        ts_body,
        todos,
        is_empty,
    })
}

/// Render the human-readable op.* `.ts` body from the synthesized ops. Each op
/// becomes a named-import call inside `up()`; backfill TODO markers are embedded as
/// comments immediately above the op they annotate.
fn render_ts(name: &str, synth: &[SynthOp]) -> String {
    let mut up = String::new();
    for s in synth {
        if let Some(todo) = &s.todo {
            up.push_str("  ");
            up.push_str(todo);
            up.push('\n');
        }
        up.push_str("  ");
        up.push_str(&render_op_call(&s.op));
        up.push('\n');
    }
    if up.is_empty() {
        up.push_str("  // (no structural delta — schema already matches the live database)\n");
    }
    format!(
        r#"// Generated migration: {name}
//
// Autogenerated from the declarative schema diff (op.* DSL). The committed
// `.ir.json` is the source of truth; this `.ts` is its human-readable mirror.
// Determinism (§4.3): all defaults are DB-evaluated synth scalars (c.fn.*).
import {{ table, t }} from "@zeroship/migrate";

export function up() {{
{up}}}
"#
    )
}

/// Render one op as a fluent `table()` call. Mirrors the op.* builder surface so
/// the emitted `.ts`, when recorded, yields the same IR.
fn render_op_call(op: &Op) -> String {
    match op {
        Op::CreateTable {
            name,
            columns,
            runtime_options,
            ..
        } => {
            let cols: Vec<String> = columns
                .iter()
                .map(|c| format!("      {}: {}", json_key(&c.name), render_col(c)))
                .collect();
            let mut args = vec![format!("    columns: {{\n{}\n    }}", cols.join(",\n"))];
            if let Some(options) = runtime_options {
                args.extend(render_create_runtime_options(options));
            }
            format!("table({}).create({{\n{},\n  }});", js_str(name), args.join(",\n"))
        }
        Op::AddColumn {
            table,
            column,
            ty,
            nullable,
            default,
            ..
        } => {
            let mut chain = render_t_for(ty);
            if *nullable == Some(false) {
                chain.push_str(".notNull()");
            }
            if let Some(d) = default {
                chain.push_str(&render_default(d));
            }
            format!(
                "table({}).column({}).add({{ type: {} }});",
                js_str(table),
                js_str(column),
                chain
            )
        }
        Op::DropColumn { table, column, .. } => {
            format!("table({}).column({}).drop();", js_str(table), js_str(column))
        }
        Op::CreateIndex { table, columns, name, unique, .. } => {
            let cols: Vec<String> = columns
                .iter()
                .map(|c| match c {
                    IndexElement::Column { name } => js_str(name),
                    IndexElement::Expr { expr } => format!(
                        "{{ kind: \"expr\", expr: {} }}",
                        serde_json::to_string(expr).expect("Expr serializes")
                    ),
                })
                .collect();
            // The index NAME is the selector argument (name-first); `columns`/`unique`
            // ride the args object. The synth path always names the index.
            let idx_name = name
                .clone()
                .unwrap_or_else(|| {
                    let parts = columns
                        .iter()
                        .map(|c| match c {
                            IndexElement::Column { name } => name.as_str(),
                            IndexElement::Expr { .. } => "expr",
                        })
                        .collect::<Vec<_>>()
                        .join("_");
                    format!("{table}_{parts}_idx")
                });
            let mut args = format!("columns: [{}]", cols.join(", "));
            if *unique == Some(true) {
                args.push_str(", unique: true");
            }
            format!(
                "table({}).index({}).add({{ {} }});",
                js_str(table),
                js_str(&idx_name),
                args
            )
        }
        Op::DropTable { table, .. } => format!("table({}).drop();", js_str(table)),
        // generate only synthesizes the structural-delta op subset above; any other
        // op kind is authored by hand, not generated. Render a placeholder comment.
        _ => "// (op authored by hand — not generated)".to_string(),
    }
}

fn render_create_runtime_options(options: &TableRuntimeOptions) -> Vec<String> {
    vec![
        format!("    softDelete: {}", options.soft_delete),
        format!("    versioning: {}", options.versioning),
        format!(
            "    strictness: {}",
            js_str(match options.strictness {
                TableStrictness::Strict => "strict",
                TableStrictness::Lenient => "lenient",
                TableStrictness::Off => "off",
            })
        ),
    ]
}

/// Render a column as a `t.*` chain inside a `create({ columns })` map.
fn render_col(c: &IrColumn) -> String {
    let mut chain = render_t_for(&c.ty);
    if c.nullable == Some(false) {
        chain.push_str(".notNull()");
    }
    if c.name == "id" {
        chain.push_str(".primaryKey()");
    }
    if c.unique == Some(true) {
        chain.push_str(".unique()");
    }
    if let Some(d) = &c.default {
        chain.push_str(&render_default(d));
    }
    chain
}

/// The `t.*` factory for a `ColType` (the portable subset generate synthesizes).
fn render_t_for(ty: &ColType) -> String {
    match ty {
        ColType::Text | ColType::String => "t.text()".into(),
        ColType::Int => "t.integer()".into(),
        ColType::BigInt => "t.bigInt()".into(),
        ColType::Float => "t.float()".into(),
        ColType::Bool => "t.boolean()".into(),
        ColType::Json => "t.json()".into(),
        ColType::Timestamp => "t.timestamp()".into(),
        ColType::Uuid => "t.uuid()".into(),
        ColType::Bytea => "t.bytes()".into(),
        ColType::Decimal { precision, scale } => format!("t.numeric({precision}, {scale})"),
        ColType::Enum { name } => format!("t.enum({})", js_str(name)),
        ColType::Domain { name } => format!("t.domain({})", js_str(name)),
        // Goodies are not generated (rejected earlier); render a hand-author note.
        _ => "t.text() /* TODO: hand-author this column type */".into(),
    }
}

/// Render an `IrDefault` as a `.default(...)` chain call. A synth fn renders to the
/// DB-evaluated `{ fn: "now" | "genRandomUuid" }` (deterministic by construction).
fn render_default(d: &IrDefault) -> String {
    match d {
        IrDefault::Fn { r#fn } => {
            let token = match r#fn {
                SynthDefaultFn::Now => "now",
                SynthDefaultFn::GenRandomUuid => "genRandomUuid",
            };
            format!(".default({{ fn: {} }})", js_str(token))
        }
        IrDefault::Literal { value } => {
            // A typed literal default — render via serde_json (a string/number/bool).
            let v = serde_json::to_string(value).unwrap_or_else(|_| "null".into());
            format!(".default({v})")
        }
    }
}

/// A JS string literal (double-quoted, minimally escaped).
fn js_str(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// A JS object key — quoted if not a bare identifier.
fn json_key(s: &str) -> String {
    if !s.is_empty()
        && s.bytes().next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        s.to_string()
    } else {
        js_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffold_rejects_invalid_name() {
        let err = scaffold_new_ts("bad name!").unwrap_err();
        match err {
            ScaffoldError::InvalidName { suggestion, .. } => {
                assert_eq!(suggestion, "bad_name");
            }
            other => panic!("expected InvalidName, got {other:?}"),
        }
    }

    #[test]
    fn scaffold_is_deterministic_by_construction() {
        let ts = scaffold_new_ts("add_widgets").unwrap();
        // The recommended synth-default pattern is documented in the scaffold.
        assert!(ts.contains("c.fn.now()") || ts.contains(r#"{ fn: "now" }"#));
        assert!(
            ts.contains("c.fn.genRandomUuid()") || ts.contains(r#"{ fn: "genRandomUuid" }"#)
        );
        // Tighten the guarantee (LOW-fix): scan ONLY the EXECUTABLE op body (line
        // comments stripped) for host clock / RNG accessors — so the test genuinely
        // proves the EMITTED ops are deterministic, not that a determinism note
        // happens to live in a comment.
        let code = strip_line_comments(&ts);
        assert!(!code.contains("Date.now()"), "executable body must not call Date.now(); body:\n{code}");
        assert!(!code.contains("Math.random()"), "executable body must not call Math.random(); body:\n{code}");
        assert!(!code.contains("crypto.randomUUID()"), "executable body must not call crypto.randomUUID(); body:\n{code}");
        assert!(!code.contains("new Date("), "executable body must not construct new Date(); body:\n{code}");
    }

    /// Strip `//` line comments from `src` (best-effort, scaffold-shaped: no string
    /// literals contain `//`), leaving only the executable code — so a determinism
    /// assertion scans the emitted ops, not a comment that merely mentions a pattern.
    fn strip_line_comments(src: &str) -> String {
        src.lines()
            .map(|line| match line.find("//") {
                Some(i) => &line[..i],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn timestamp_14_is_14_digits() {
        let stamp = timestamp_14(SystemTime::now());
        assert_eq!(stamp.len(), 14);
        assert!(stamp.bytes().all(|b| b.is_ascii_digit()));
    }
}
