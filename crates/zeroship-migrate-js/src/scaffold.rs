//! PR4 deliverable (C) — `new` scaffold + the generate-from-diff op.* `.ts`/IR
//! synthesizer (D). Both emit DETERMINISTIC op.* migration SOURCE: any time / uuid
//! seed default is the DB-evaluated `c.fn.now()` / `c.fn.genRandomUuid()` synth
//! scalar, NEVER `Date.now()` / `Math.random()` / `crypto.randomUUID()` — so the
//! scaffold is deterministic BY CONSTRUCTION (§4.3) and recording it surfaces ZERO
//! determinism warnings.

use std::time::{SystemTime, UNIX_EPOCH};

use zeroship_migrate::declarative::{DesiredSchema, SYSTEM_FIELD_NAMES};
use zeroship_migrate::drift::{ColumnSnapshot, SchemaSnapshot};
use zeroship_migrate::ir::{ColType, IrColumn, IrDefault, Op, SynthDefaultFn};
use zeroship_migrate::loader::{is_valid_migration_name, suggest_migration_name};
use zeroship_migrate::MigrationIr;

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
// Authored against the @zeroship/migrate op.* DSL. The build step records this
// into a committed `<version>_{name}.ir.json` artifact (build-once authority).
//
// DETERMINISM (§4.3): any time/uuid seed column MUST default to the DB-evaluated
// synth scalar — use the c.fn.now / c.fn.genRandomUuid synth defaults (shown
// below) so the value is computed at apply time. Do NOT seed defaults from a host
// clock or RNG: a host-side timestamp/random would bake a frozen value into the
// committed artifact and diverge across replays. The determinism lint flags those
// host accessors as a warning the AI loop self-corrects on.
import {{ createTable, dropTable, t }} from "@zeroship/migrate";

export function up() {{
  // Example — replace with your schema change.
  //
  // Determinism-correct seed defaults (the pattern to follow when you DO need a
  // seeded id/timestamp column): use the DB-evaluated synth scalars, e.g.
  //   id: t.uuid().notNull().primaryKey().default({{ fn: "genRandomUuid" }}),
  //   created_at: t.timestamp().notNull().default({{ fn: "now" }}),
  // (the platform also injects the system id/created_at/updated_at columns for you).
  createTable("{name}", {{
    id: t.uuid().notNull().primaryKey(),
    title: t.text().notNull(),
  }});
}}

export function down() {{
  dropTable("{name}");
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
/// default in a `createTable` is a deferred render wave on the engine
/// ([`IrLowerError::ExprRenderDeferred`]). Emitting a default here would make the
/// generated `.ir.json` un-applyable. The drift comparison is on `data_type` +
/// `nullable` only (never `default`), so omitting defaults still re-diffs to zero.
/// Kept as a function (returning `None`) to document the deliberate choice + keep a
/// single seam if a future render wave lands.
fn system_default_for(_col_name: &str, _ty: &ColType) -> Option<IrDefault> {
    None
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
                    });
                }
                ops.push(SynthOp {
                    op: Op::CreateTable {
                        name: table.clone(),
                        columns,
                        constraints: Vec::new(),
                        indexes: Vec::new(),
                    },
                    todo: None,
                });
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
                        },
                        todo,
                    });
                }
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
                        if_exists: None,
                        cascade: None,
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
                            if_exists: None,
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
        ir_version: zeroship_migrate::ir::CURRENT_IR_VERSION,
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
import {{ createTable, dropTable, addColumn, dropColumn, t }} from "@zeroship/migrate";

export function up() {{
{up}}}
"#
    )
}

/// Render one op as a `@zeroship/migrate` named-import call. Mirrors the op.*
/// builder surface so the emitted `.ts`, when recorded, yields the same IR.
fn render_op_call(op: &Op) -> String {
    match op {
        Op::CreateTable { name, columns, .. } => {
            let cols: Vec<String> = columns
                .iter()
                .map(|c| format!("    {}: {}", json_key(&c.name), render_col(c)))
                .collect();
            format!("createTable({}, {{\n{}\n  }});", js_str(name), cols.join(",\n"))
        }
        Op::AddColumn {
            table,
            column,
            ty,
            nullable,
            default,
        } => {
            let mut chain = render_t_for(ty);
            if *nullable == Some(false) {
                chain.push_str(".notNull()");
            }
            if let Some(d) = default {
                chain.push_str(&render_default(d));
            }
            format!("addColumn({}, {}, {});", js_str(table), js_str(column), chain)
        }
        Op::DropColumn { table, column, .. } => {
            format!("dropColumn({}, {});", js_str(table), js_str(column))
        }
        Op::DropTable { table, .. } => format!("dropTable({});", js_str(table)),
        // generate only synthesizes the structural-delta op subset above; any other
        // op kind is authored by hand, not generated. Render a placeholder comment.
        _ => "// (op authored by hand — not generated)".to_string(),
    }
}

/// Render a column as a `t.*` chain inside a `createTable` map.
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
        ColType::Int => "t.int()".into(),
        ColType::BigInt => "t.bigInt()".into(),
        ColType::Float => "t.float()".into(),
        ColType::Bool => "t.boolean()".into(),
        ColType::Json => "t.json()".into(),
        ColType::Timestamp => "t.timestamp()".into(),
        ColType::Uuid => "t.uuid()".into(),
        ColType::Bytea => "t.bytes()".into(),
        ColType::Decimal { precision, scale } => format!("t.numeric({precision}, {scale})"),
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
        // Determinism-correct synth defaults present.
        assert!(ts.contains("c.fn.now()") || ts.contains(r#"{ fn: "now" }"#));
        assert!(
            ts.contains("c.fn.genRandomUuid()") || ts.contains(r#"{ fn: "genRandomUuid" }"#)
        );
        // No host clock / RNG accessors.
        assert!(!ts.contains("Date.now()"));
        assert!(!ts.contains("Math.random()"));
        assert!(!ts.contains("crypto.randomUUID()"));
        assert!(!ts.contains("new Date("));
    }

    #[test]
    fn timestamp_14_is_14_digits() {
        let stamp = timestamp_14(SystemTime::now());
        assert_eq!(stamp.len(), 14);
        assert!(stamp.bytes().all(|b| b.is_ascii_digit()));
    }
}
