//! The offline `--sql` plan preview (the canonical Alembic `--sql` /
//! Atlas / Flyway / dbmate feature).
//!
//! This module is a **surfacing / formatting layer over the SQL the engine ALREADY
//! lowers** — it re-implements NOTHING. Given a lowered [`AppliedPlan`] (from
//! [`IrAuthor::lower_plan`](crate::render::lower::IrAuthor::lower_plan) for an
//! IR envelope), it walks the steps and prints the SQL strings already held in each
//! step (`Migration.up`, `PlanStep::Dml.template`) verbatim. It does not re-render
//! migration SQL. Executable MySQL previews add a fixed session envelope so copied
//! SQL has the same string-literal grammar as apply, without changing the caller's
//! inherited `sql_mode` after the preview finishes.
//!
//! # The honest boundary (the load-bearing design point)
//!
//! DB-INDEPENDENT ops — `createTable`/`dropTable`/`addColumn`/`dropColumn`/
//! `addForeignKey`/`addUnique`/`addCheck`/`dropConstraint`/`createIndex`/
//! `dropIndex` + one-shot `insert`/`update`/`delete` — render their REAL SQL: their
//! `up`/`template` is fully determined offline (`IrAuthor::lower_*` lowers them with
//! an EMPTY [`LiveSchema`], needing no DB).
//!
//! DB-STATE-DEPENDENT ops CANNOT be faithfully rendered offline. For these the
//! preview emits a CLEARLY-LABELED `-- [runtime-resolved] …` comment line and
//! **NEVER fabricates SQL**:
//!
//! - **online `renameColumn`** — PG expand-contract (E1..C2) carries a windowed
//!   runtime BACKFILL (`BackfillSpec`, exact statement stream depends on live row
//!   count / PK ranges) and a cross-deploy CONTRACT cutover; SQLite needs the live
//!   12-step rebuild (it does not even lower offline — fails closed with
//!   [`IrLowerError::SqliteRenameNeedsLiveTable`](crate::render::lower::IrLowerError)).
//! - **`backfill`** — a runtime windowed loop.
//! - **any DDL migration carrying an existence-guard probe**
//!   (`ifNotExists`/`ifExists`): apply is a
//!   runtime catalog probe + run / satisfied-noop / fail-drift decision
//!   ([`guard_probe`](crate::render::existence_probe), explicitly NOT offline-renderable). The
//!   bare DDL `up` IS real SQL the apply runs when the probe says "run", so we
//!   print it under the label — but we do NOT invent an `IF [NOT] EXISTS` clause
//!   the engine never emits. MySQL additionally refuses a present createTable or
//!   addColumn until its probe can prove modifier-preserving column-type equality.
//!   This is a preview-text distinction only: it changes no lowered statement and
//!   no apply behaviour on any dialect.
//! - **stand-alone SQLite `alterColumn*` / non-FK constraint changes** — require
//!   live structure; named FK add/drop changes lower to the live 12-step rebuild
//!   and are not flattened into ordinary offline SQL
//!   ([`IrLowerError::SqliteRebuildOnly`](crate::render::lower::IrLowerError)).
//!
//! The preview is HONEST that it shows the offline-renderable subset and labels the
//! rest; the header + trailing summary state exactly that.
//!
//! # Policy table-shape resolve
//!
//! The IR-envelope entries run [`resolve_create_table_policy`](crate::resolve_create_table_policy)
//! over the parsed document before validating and lowering it, exactly as the apply
//! path does. A previewed `createTable` therefore shows the charter-injected
//! columns, the pinned primary key, and the injected indexes apply will create. The
//! resolve is idempotent, so an already-resolved envelope previews identically to
//! the raw host-recorder spelling, and a no-inject charter leaves the IR verbatim.
//! An envelope that VIOLATES the charter now fails the preview closed instead of
//! rendering DDL apply would refuse. This applies to the envelope entries only:
//! [`render_plan_sql`] and [`render_set_sql`] take an ALREADY-lowered plan and
//! resolve nothing.

use std::fmt::Write as _;

use crate::model::ir::{CommentTarget, CursorStability, ExistenceGuard, MigrationIr, Op};
use crate::render::lower::{op_kind_tag, IrAuthor, IrLowerError, LiveSchema};
use crate::render::plan::AppliedPlan;
use crate::render::step::{BindValue, PlanStep, RenameStep};
use crate::schema::query::SqlDialect;

/// The label prefix every runtime-resolved line carries — the single sentinel the
/// no-fabrication tests assert on. If you change this, change the tests.
pub const RUNTIME_RESOLVED: &str = "-- [runtime-resolved]";

/// MySQL string literals are authored for standard quote-doubling, and an
/// explicit legacy zero identity must never become an implicit allocation. A
/// copied preview therefore executes under `NO_BACKSLASH_ESCAPES` and
/// `NO_AUTO_VALUE_ON_ZERO`, just like apply. Save and restore the exact inherited
/// mode so preview execution does not leak a session-policy change into the
/// caller's connection.
const MYSQL_PREVIEW_SAVE_SQL_MODE: &str =
    "SET @__zero_migrate_preview_saved_sql_mode = @@SESSION.sql_mode;";
const MYSQL_PREVIEW_PIN_SQL_MODE: &str =
    "SET SESSION sql_mode = CONCAT_WS(',', @@SESSION.sql_mode, 'NO_BACKSLASH_ESCAPES', 'NO_AUTO_VALUE_ON_ZERO');";
const MYSQL_PREVIEW_RESTORE_SQL_MODE: &str =
    "SET SESSION sql_mode = @__zero_migrate_preview_saved_sql_mode;";

/// Options for the offline preview render.
#[derive(Debug, Clone)]
pub struct PreviewOpts {
    /// The trust profile's effective schema for an op that omits its own qualifier.
    /// The general/Trusted CLI default is `public`
    /// ([`DEFAULT_GENERIC_SCHEMA`](crate)); the Confined platform path pins the
    /// project schema. NEVER requires a DB to pick — it is a flag/profile value.
    pub default_schema: String,
    /// The declaring app stamped onto lowered migrations (ownership is enforced
    /// upstream by the load gate; here it only affects DML journal identity, never
    /// the rendered SQL). For the preview it is a cosmetic attribution.
    pub owner_app: String,
    /// The composed policy whose inject rules shape every previewed create-table
    /// operation. The preview runs the table-shape resolve itself against this
    /// policy, so an envelope may arrive raw or already resolved. It is mandatory;
    /// preview has no ambient system-field profile. It does NOT drive anything
    /// beyond create-table injection and the lowering context.
    pub effective_policy: zero_migrate_policy::EffectivePolicy,
}

/// The dialect's human name for the header.
fn dialect_label(d: SqlDialect) -> &'static str {
    match d {
        SqlDialect::Postgres => "postgres",
        SqlDialect::Sqlite => "sqlite",
        SqlDialect::Mysql => "mysql",
    }
}

/// How a plan's body relates to the requested `--dialect` — drives the HONEST header
/// caption. The IR envelope leg is genuinely per-dialect LOWERED, so its
/// header may claim the dialect. The raw `.sql` (Flyway, operator-authored) leg is
/// printed VERBATIM — it is NOT dialect-transformed — so captioning it with a
/// `(dialect: sqlite)` claim would mislead an operator reviewing a SQLite go-live
/// when the file is actually PG-only SQL. That leg gets a verbatim caption instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialectCaption {
    /// The body was lowered for this dialect — claim it.
    Lowered(SqlDialect),
    /// The body is operator-authored raw `.sql`, shown verbatim — NOT transformed.
    /// `requested` is the `--dialect` the operator asked for. It does not transform
    /// the body; MySQL uses it only to select the safe session envelope.
    VerbatimRawSql { requested: SqlDialect },
}

impl DialectCaption {
    /// The parenthetical the header carries. Lowered ⇒ a dialect claim; raw `.sql`
    /// ⇒ an explicit "NOT dialect-transformed" disclaimer (never a bare dialect
    /// claim over verbatim foreign-dialect SQL).
    fn header_suffix(self) -> String {
        match self {
            DialectCaption::Lowered(d) => format!("(dialect: {})", dialect_label(d)),
            DialectCaption::VerbatimRawSql { requested } => format!(
                "(verbatim raw .sql; body NOT dialect-transformed for --dialect {})",
                dialect_label(requested)
            ),
        }
    }
}

/// A single rendered preview line + whether it was a runtime-resolved LABEL (vs a
/// real rendered statement). The set-level renderer tallies these for the summary.
struct Rendered {
    /// The text (a SQL statement, or a `-- [runtime-resolved] …` line, or a comment).
    text: String,
    /// `true` when the text is a runtime-resolved label (counts toward the M tally).
    runtime_resolved: bool,
    /// `true` when the text is a real executable statement (counts toward the N tally).
    statement: bool,
}

impl Rendered {
    fn statement(text: String) -> Self {
        Self {
            text,
            runtime_resolved: false,
            statement: true,
        }
    }
    fn label(text: String) -> Self {
        Self {
            text,
            runtime_resolved: true,
            statement: false,
        }
    }
    fn comment(text: String) -> Self {
        Self {
            text,
            runtime_resolved: false,
            statement: false,
        }
    }
}

/// Render ONE already-lowered [`AppliedPlan`] to its offline SQL preview string.
/// Pure + DB-free: it reads back the SQL the lowering already
/// produced. The plan's steps were already lowered for a dialect by the caller;
/// `dialect` selects the header label and the MySQL session envelope.
#[must_use]
pub fn render_plan_sql(plan: &AppliedPlan, dialect: SqlDialect, _opts: &PreviewOpts) -> String {
    let rendered = render_plan_steps(plan, dialect);
    let wrap_mysql = needs_mysql_session_envelope(dialect, &rendered);
    let mut out = String::new();
    write_plan_header(&mut out, plan, DialectCaption::Lowered(dialect));
    if wrap_mysql {
        write_mysql_session_prologue(&mut out);
    }
    write_rendered(&mut out, &rendered);
    if wrap_mysql {
        write_mysql_session_epilogue(&mut out);
    }
    out
}

/// Render a SET of plans (a whole migration directory) to one preview document
/// with a leading honest header and a trailing tally.
///
/// This is the RAW `.sql` (Flyway, operator-authored) leg: each plan's body is the
/// verbatim operator SQL, which is NOT dialect-transformed. The headers
/// therefore carry a `(verbatim raw .sql — NOT dialect-transformed)` caption rather
/// than a `(dialect: …)` claim, so an operator reviewing a SQLite go-live is never
/// misled into thinking PG-only verbatim SQL was lowered for SQLite. `dialect` does
/// not transform the raw body; MySQL uses it to add the safe session envelope.
#[must_use]
pub fn render_set_sql(plans: &[AppliedPlan], dialect: SqlDialect, _opts: &PreviewOpts) -> String {
    let caption = DialectCaption::VerbatimRawSql { requested: dialect };
    let rendered_plans = plans
        .iter()
        .map(|plan| render_plan_steps(plan, dialect))
        .collect::<Vec<Vec<Rendered>>>();
    let total_statements = rendered_plans
        .iter()
        .flatten()
        .filter(|r| r.statement)
        .count();
    let total_runtime = rendered_plans
        .iter()
        .flatten()
        .filter(|r| r.runtime_resolved)
        .count();
    let wrap_mysql = dialect == SqlDialect::Mysql && total_statements > 0;

    let mut out = String::new();
    write_doc_header(&mut out, caption);
    if wrap_mysql {
        write_mysql_session_prologue(&mut out);
    }
    for (plan, rendered) in plans.iter().zip(&rendered_plans) {
        out.push('\n');
        write_plan_header(&mut out, plan, caption);
        write_rendered(&mut out, rendered);
    }
    if wrap_mysql {
        write_mysql_session_epilogue(&mut out);
    }
    let _ = writeln!(
        out,
        "\n-- preview: {total_statements} statement(s) rendered, {total_runtime} runtime-resolved"
    );
    out
}

/// Load + lower an IR envelope artifact's bytes OFFLINE (no DB — an EMPTY
/// [`LiveSchema`]) for the target dialect, then render the preview. DB-state-
/// dependent ops that cannot lower against the empty live schema (SQLite rename /
/// rebuild-only) are caught PER-OP and emitted as `[runtime-resolved]` labels
/// rather than aborting the whole preview.
///
/// Returns the per-plan preview text; the CLI joins these for a directory.
///
/// # Errors
/// Returns the load/parse error string if the IR document itself is unparseable /
/// rejected by the load gate (a hard, clear non-zero for the CLI). A single op that
/// merely cannot be lowered offline is NOT an error — it degrades to a label.
pub fn render_ir_envelope_sql(
    bytes: &str,
    dialect: SqlDialect,
    opts: &PreviewOpts,
) -> Result<String, String> {
    let (name, rendered) = render_ir_envelope_rendered(bytes, dialect, opts)?;
    let wrap_mysql = needs_mysql_session_envelope(dialect, &rendered);
    let mut out = String::new();
    // Synthesize a plan header from the IR identity (no full AppliedPlan needed —
    // a single un-lowerable op would otherwise make `lower_plan` abort).
    let _ = writeln!(
        out,
        "-- ============================================================"
    );
    let _ = writeln!(
        out,
        "-- plan {:?}  {}",
        name,
        DialectCaption::Lowered(dialect).header_suffix()
    );
    let _ = writeln!(
        out,
        "-- ============================================================"
    );
    if wrap_mysql {
        write_mysql_session_prologue(&mut out);
    }
    write_rendered(&mut out, &rendered);
    if wrap_mysql {
        write_mysql_session_epilogue(&mut out);
    }
    let statements = rendered.iter().filter(|r| r.statement).count();
    let runtime = rendered.iter().filter(|r| r.runtime_resolved).count();
    let _ = writeln!(
        out,
        "\n-- preview: {statements} statement(s) rendered, {runtime} runtime-resolved"
    );
    Ok(out)
}

/// Load + lower an IR envelope artifact through the SAME tolerant path used by
/// [`render_ir_envelope_sql`], returning only executable statement text.
///
/// Returns the plan name plus the statement stream. This is the machine-readable
/// half of the offline preview surface: plan headers, `[runtime-resolved]` labels,
/// and the trailing tally are DROPPED rather than commented out, so a caller that
/// must not see prose does not have to scrape SQL back out of the human preview.
/// For MySQL, a non-empty statement stream includes the same save/pin/restore
/// `sql_mode` envelope as the human preview, so it is also safe to execute as-is.
///
/// What this does NOT do: it does not analyze, classify, or guard-check the
/// statements it returns; it does not consult a live catalog; and it does not
/// report which ops were left out. An op that degrades to a `[runtime-resolved]`
/// label in the human preview has NO entry here at all, so a short stream is not
/// evidence of a short plan; compare against [`render_ir_envelope_sql`] to see
/// what was dropped.
///
/// Nothing in this repository calls it. The TS CLI's `lint` and `plan` commands
/// render the HUMAN preview instead, through the addon's `previewSql`
/// verb (`crates/zero-migrate-node/src/bridge.rs`), which calls
/// [`render_ir_envelope_sql`]. It is retained because it is re-exported from the
/// crate root for out-of-tree embedders that take this crate as a path dependency
/// (see `docs/embedding.md`), and it is the only way to obtain the statement text
/// without re-deriving the statement/label split that
/// [`render_ir_envelope_sql`] folds into one formatted string.
///
/// # Errors
///
/// Returns an error when the IR document cannot be parsed or structurally
/// validated for offline rendering.
pub fn render_ir_envelope_sql_statements(
    bytes: &str,
    dialect: SqlDialect,
    opts: &PreviewOpts,
) -> Result<(String, Vec<String>), String> {
    let (name, rendered) = render_ir_envelope_rendered(bytes, dialect, opts)?;
    let statements = rendered
        .into_iter()
        .filter(|r| r.statement)
        .map(|r| r.text)
        .collect::<Vec<_>>();
    let statements = wrap_mysql_statements(dialect, statements);
    Ok((name, statements))
}

fn render_ir_envelope_rendered(
    bytes: &str,
    dialect: SqlDialect,
    opts: &PreviewOpts,
) -> Result<(String, Vec<Rendered>), String> {
    // Parse the IR document WITHOUT the ownership/registry gate (this is an offline
    // operator preview, not a deploy): `serde` the wire shape, then validate its
    // structure. We deliberately do NOT call `load_ir_document` — that gate stamps
    // server ownership and consults a cross-app registry which has no meaning
    // offline. The structural validator is enough to refuse a malformed artifact.
    let ir: MigrationIr =
        serde_json::from_str(bytes).map_err(|e| format!("parse IR envelope: {e}"))?;

    // Run the policy table-shape resolve the apply path runs
    // (`crates/zero-migrate-node/src/lower.rs`), so the previewed `createTable`
    // carries the charter-injected columns, pinned primary key, and injected indexes
    // apply will actually create. Without this the preview showed the author's bare
    // declaration and silently disagreed with apply.
    //
    // Order matters: resolve BEFORE `validate_ir`. An authored index predicate may
    // reference an injected column (the soft-delete partial unique on `deleted_at`),
    // which only validates once the injected columns are present. This is the same
    // order apply uses.
    //
    // The resolve is idempotent, so an envelope that arrives already resolved (the
    // native on-disk artifact is post-fold) previews identically to the raw
    // host-recorder spelling. A no-inject charter early-returns.
    //
    // This covers `createTable` shape only: it does not resolve anything about other
    // ops, does not consult a live catalog, and does not change which ops degrade to
    // a `[runtime-resolved]` label.
    let ir = crate::model::table_shape::resolve_create_table_policy(
        &ir,
        &opts.effective_policy,
        &opts.default_schema,
    )
    .map_err(|e| format!("table-shape resolve for IR envelope: {e}"))?;

    let target = match dialect {
        SqlDialect::Postgres => crate::model::validate::Dialect::Postgres,
        SqlDialect::Sqlite => crate::model::validate::Dialect::Sqlite,
        SqlDialect::Mysql => crate::model::validate::Dialect::Mysql,
    };
    crate::model::validate::validate_ir(&ir, target, &[])
        .map_err(|e| format!("validate IR envelope: {e}"))?;

    // The general/Trusted operator preview renders into the chosen default schema:
    // bind it as the author's project schema, so an op with NO qualifier (or one
    // matching the default) renders there. A truly FOREIGN explicit qualifier is
    // out of the Confined `Single(default_schema)` scope and fails to lower → it is
    // labeled `[runtime-resolved]` "not offline-renderable" rather than rendered
    // into the wrong schema (honest, fail-closed). NEVER requires a DB to pick.
    let author = IrAuthor::new(
        opts.default_schema.clone(),
        opts.owner_app.clone(),
        dialect,
        &opts.effective_policy,
    );

    let live = LiveSchema::default();
    let rendered = render_ir_ops(&author, &ir, &live, dialect, &opts.default_schema);
    Ok((ir.name, rendered))
}

/// Per-op lowering for the IR envelope path: lower each op in isolation so a single
/// DB-state-dependent op (SQLite rename / rebuild-only) degrades to a label instead
/// of aborting the whole preview. Mirrors the per-op iteration `lower_steps` does,
/// but tolerant: a `lower_plan` error on a one-op IR ⇒ a runtime-resolved label.
fn render_ir_ops(
    author: &IrAuthor,
    ir: &MigrationIr,
    live: &LiveSchema,
    dialect: SqlDialect,
    project_schema: &str,
) -> Vec<Rendered> {
    let mut out = Vec::new();
    let mut working_live = live.clone();
    for op in &ir.ops {
        // A guard-carrying op lowers FINE offline (the probe is stamped, not an
        // error) — but its apply is a runtime catalog-probe decision, so it MUST be
        // labeled. Detect it from the op directly (the lowered `up` is bare DDL with
        // no `IF [NOT] EXISTS`; we print it under the label, never fabricating one).
        let guard = op.existence_guard();
        // Lower JUST this op via a one-op IR clone so an un-lowerable op is local.
        let one = single_op_ir(ir, op.clone());
        match author.lower_plan(&one, &working_live) {
            Ok(plan) => {
                for step in &plan.steps {
                    render_step(op, guard, step, dialect, &mut out);
                }
            }
            Err(e) => {
                // The op cannot be lowered offline — it is DB-state-dependent. NEVER
                // fabricate SQL: emit a labeled, descriptive line citing WHY.
                out.push(Rendered::label(runtime_resolved_for_lower_error(op, &e)));
            }
        }

        // Per-op tolerance must not erase deterministic state established by an
        // earlier declaration in the same envelope. In particular, a createTable
        // FK to an earlier table is an inline create-time constraint, not an
        // unresolved forward edge. Carry the authored logical contracts and table
        // presence forward without inventing catalog snapshots; truly live-state-
        // dependent operations still degrade to a labeled preview line.
        let _ = working_live.advance_logical_columns(&one, dialect, project_schema, None);
        advance_preview_table_presence(op, dialect, &mut working_live.tables);
    }
    out
}

fn advance_preview_table_presence(
    op: &Op,
    dialect: SqlDialect,
    tables: &mut std::collections::BTreeSet<String>,
) {
    match op {
        Op::CreateTable { name, .. } => {
            tables.insert(name.clone());
        }
        Op::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } => {
            let selected = match dialect {
                SqlDialect::Postgres => pg.as_deref(),
                SqlDialect::Sqlite => sqlite.as_deref(),
                SqlDialect::Mysql => mysql.as_deref(),
            }
            .or(default.as_deref());
            if let Some(selected) = selected {
                for nested in selected {
                    advance_preview_table_presence(nested, dialect, tables);
                }
            }
        }
        _ => {}
    }
}

/// Build a one-op `MigrationIr` carrying `op`, sharing the parent's identity so the
/// per-op lower runs in the same author context. Strips cross-op concerns
/// (`depends_on`/`supersedes`/`preconditions`) — they do not affect per-op SQL.
fn single_op_ir(parent: &MigrationIr, op: Op) -> MigrationIr {
    MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: parent.ir_version,
        name: parent.name.clone(),
        owner_app: parent.owner_app.clone(),
        ops: vec![op],
        flags: parent.flags.clone(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

/// Walk an already-lowered plan's steps into render lines (the `.sql`-artifact and
/// AppliedPlan path). Guard detection here is best-effort from the step alone
/// (`Migration.existence_guard`), since a `.sql` plan carries no op list. `dialect`
/// is the dialect the plan was lowered for; it selects the guard label's apply
/// story (see [`guard_label`]) and is REQUIRED rather than defaulted, so a caller
/// cannot silently inherit one dialect's apply semantics for another's preview.
fn render_plan_steps(plan: &AppliedPlan, dialect: SqlDialect) -> Vec<Rendered> {
    let mut out = Vec::new();
    for step in &plan.steps {
        // On the AppliedPlan path we have no `Op`; pass `None` for the op + read the
        // guard off the migration when present.
        render_step_no_op(step, dialect, &mut out);
    }
    out
}

/// Render one step WITH its originating op (the IR envelope path), so existence
/// guards and online renames are labeled with the op's subject. `dialect` reaches
/// [`guard_label`] only; it changes no rendered statement.
fn render_step(
    op: &Op,
    guard: Option<ExistenceGuard>,
    step: &PlanStep,
    dialect: SqlDialect,
    out: &mut Vec<Rendered>,
) {
    match step {
        PlanStep::Ddl(m) => {
            if let Some(g) = guard.or_else(|| authored_probe(m).map(|_| guard_dir(m))) {
                out.push(Rendered::label(guard_label(op, g, dialect)));
            }
            push_statement(&m.up, out);
        }
        PlanStep::Dml {
            template, binds, ..
        } => {
            out.push(Rendered::comment(dml_comment(op, binds)));
            push_statement(template, out);
        }
        PlanStep::Backfill { spec, .. } => {
            out.push(Rendered::label(format!(
                "{RUNTIME_RESOLVED} backfill {:?}.{:?}: windowed runtime loop over batches \
                 (cursorColumns {:?}, batch {}, {}); bounded cohort; concurrent inserts require \
                 a write invariant or a final catch-up with writes stopped; exact statement \
                 stream depends on live row count",
                spec.schema,
                spec.table,
                spec.cursor_columns,
                spec.batch_size,
                cursor_stability_label(&spec.cursor_stability)
            )));
        }
        PlanStep::AlterPrimaryKey(step) => render_alter_primary_key(step, out),
        PlanStep::SynchronizeIdentity(step) => render_synchronize_identity(step, out),
        PlanStep::OnlineRename(rename) => render_online_rename(op, rename, out),
    }
}

/// Render one step with NO op context (the `.sql` / AppliedPlan path). `dialect` is
/// the dialect the plan was lowered for and selects the guard label's apply story,
/// exactly as on the op-carrying path.
fn render_step_no_op(step: &PlanStep, dialect: SqlDialect, out: &mut Vec<Rendered>) {
    match step {
        PlanStep::Ddl(m) => {
            if let Some(p) = authored_probe(m) {
                let kind = probe_kind(p);
                out.push(Rendered::label(match dialect {
                    SqlDialect::Postgres | SqlDialect::Sqlite => format!(
                        "{RUNTIME_RESOLVED} guarded DDL ({kind}): catalog-probed at apply \
                         (run / satisfied-noop / fail-drift); the statement below is the bare DDL"
                    ),
                    SqlDialect::Mysql => format!(
                        "{RUNTIME_RESOLVED} guarded DDL ({kind}): catalog-probed at apply \
                         (run / satisfied-noop / fail-drift); present createTable/addColumn is \
                         refused until MySQL column-type equality is implemented; the statement \
                         below is the bare DDL"
                    ),
                }));
            }
            push_statement(&m.up, out);
        }
        PlanStep::Dml {
            template, binds, ..
        } => {
            out.push(Rendered::comment(format!(
                "-- DML; binds: {} typed value(s) (bound natively, never interpolated)",
                binds.len()
            )));
            push_statement(template, out);
        }
        PlanStep::Backfill { spec, .. } => {
            out.push(Rendered::label(format!(
                "{RUNTIME_RESOLVED} backfill {:?}.{:?}: windowed runtime loop (cursorColumns \
                 {:?}, batch {}, {}); bounded cohort; concurrent inserts require a write \
                 invariant or a final catch-up with writes stopped; exact statement stream \
                 depends on live row count",
                spec.schema,
                spec.table,
                spec.cursor_columns,
                spec.batch_size,
                cursor_stability_label(&spec.cursor_stability)
            )));
        }
        PlanStep::AlterPrimaryKey(step) => render_alter_primary_key(step, out),
        PlanStep::SynchronizeIdentity(step) => render_synchronize_identity(step, out),
        PlanStep::OnlineRename(rename) => render_online_rename_no_op(rename, out),
    }
}

fn cursor_stability_label(stability: &CursorStability) -> String {
    match stability {
        CursorStability::GuardUpdates => {
            "cursor stability guardUpdates (managed database update guard)".to_string()
        }
        CursorStability::ExternalInvariant { name } => format!(
            "CURSOR STABILITY EXTERNAL INVARIANT {name:?} (explicit destructive approval required)"
        ),
    }
}

fn render_alter_primary_key(
    step: &crate::render::step::AlterPrimaryKeyStep,
    out: &mut Vec<Rendered>,
) {
    let (action, authored) = match &step.action {
        crate::model::ir::AlterPrimaryKeyAction::Add { columns } => {
            ("add", format!("columns={columns:?}"))
        }
        crate::model::ir::AlterPrimaryKeyAction::Replace {
            expected_columns,
            columns,
            drop_identity_from,
        } => (
            "replace",
            format!(
                "expectedColumns={expected_columns:?}, columns={columns:?}, dropIdentityFrom={drop_identity_from:?}"
            ),
        ),
        crate::model::ir::AlterPrimaryKeyAction::Drop {
            expected_columns,
            drop_identity_from,
        } => (
            "drop",
            format!(
                "expectedColumns={expected_columns:?}, dropIdentityFrom={drop_identity_from:?}"
            ),
        ),
    };
    out.push(Rendered::label(format!(
        "{RUNTIME_RESOLVED} primary-key {action} {:?}.{:?} ({authored}): exact current-key, candidate-unique, identity, and inbound-foreign-key prerequisites are catalog-validated under the apply lock; target-specific SQL is generated only after validation",
        step.schema, step.table,
    )));
}

fn render_synchronize_identity(
    step: &crate::render::step::SynchronizeIdentityStep,
    out: &mut Vec<Rendered>,
) {
    out.push(Rendered::label(format!(
        "{RUNTIME_RESOLVED} SYNCHRONIZE IDENTITY {:?}.{:?}.{:?}; WRITES QUIESCED ASSERTION {:?}: the engine cannot prove writer quiescence; apply validates the live identity generator and advances it only when behind, never backward",
        step.schema, step.table, step.column, step.writes_quiesced,
    )));
}

/// The PG expand-contract / SQLite rebuild online-rename render (with op context).
fn render_online_rename(op: &Op, rename: &RenameStep, out: &mut Vec<Rendered>) {
    let subject = rename_subject(op);
    match rename {
        RenameStep::PgExpandContract(ec) => {
            out.push(Rendered::label(format!(
                "{RUNTIME_RESOLVED} online rename {subject}: expand-contract (E1..C2); \
                 the BACKFILL is windowed by PK and the CONTRACT cutover is partitioned across \
                 deploys — exact statement stream depends on live state. The additive expand/\
                 contract DDL below is fixed text; the backfill + cutover are runtime-resolved:"
            )));
            for m in ec.expand.iter().chain(ec.contract.iter()) {
                out.push(Rendered::comment(indent_sql(&m.up)));
            }
        }
        RenameStep::SqliteRebuild(rb) => {
            out.push(Rendered::label(format!(
                "{RUNTIME_RESOLVED} online rename {subject}: SQLite 12-step table rebuild; \
                 needs the live table structure — exact rebuild SQL depends on live state"
            )));
            let _ = rb; // the rebuild statements depend on live shape — never printed as the stream
        }
    }
}

/// Online-rename render with NO op context (the `.sql`/AppliedPlan path).
fn render_online_rename_no_op(rename: &RenameStep, out: &mut Vec<Rendered>) {
    match rename {
        RenameStep::PgExpandContract(ec) => {
            out.push(Rendered::label(format!(
                "{RUNTIME_RESOLVED} online rename (expand-contract): backfill windowed by PK + \
                 cross-deploy contract cutover — exact statement stream depends on live state"
            )));
            for m in ec.expand.iter().chain(ec.contract.iter()) {
                out.push(Rendered::comment(indent_sql(&m.up)));
            }
        }
        RenameStep::SqliteRebuild(_) => {
            out.push(Rendered::label(format!(
                "{RUNTIME_RESOLVED} online rename (SQLite 12-step rebuild): needs live table \
                 structure — exact rebuild SQL depends on live state"
            )));
        }
    }
}

/// The `[runtime-resolved]` line for an op that FAILED to lower offline (the
/// genuinely DB-state-dependent SQLite legs). NEVER fabricates SQL.
fn runtime_resolved_for_lower_error(op: &Op, err: &IrLowerError) -> String {
    let kind = op_kind_tag(op);
    let subject = op_subject(op);
    match err {
        IrLowerError::SqliteRenameNeedsLiveTable(_) => format!(
            "{RUNTIME_RESOLVED} {kind} {subject}: SQLite online rename needs the live table \
             structure (12-step rebuild) — not offline-renderable"
        ),
        IrLowerError::SqliteRebuildOnly(_) => format!(
            "{RUNTIME_RESOLVED} {kind} {subject}: reconciled via the SQLite 12-step rebuild \
             (needs live table structure) — not offline-renderable"
        ),
        // An online `renameColumn` (PG expand-contract OR SQLite rebuild) needs the
        // LIVE `from` column's type/structure to lower (reconcile the IR type, author
        // the dual-write / rebuild + the windowed backfill). It is fundamentally
        // runtime-resolved: backfill windowed by PK + (PG) cross-deploy contract
        // cutover. NEVER fabricate the stream.
        IrLowerError::RenameLower(_) | IrLowerError::RenameNeedsLiveColumn(..) => format!(
            "{RUNTIME_RESOLVED} {kind} {subject}: online rename needs the live column \
             structure to lower (expand-contract dual-write / SQLite rebuild); the backfill is \
             windowed by PK and the cutover is partitioned across deploys; exact statement \
             stream depends on live state"
        ),
        other => format!("{RUNTIME_RESOLVED} {kind} {subject}: not offline-renderable ({other})"),
    }
}

/// The runtime-resolved label for a guard-carrying op (op context available).
///
/// The apply story is DIALECT-SPECIFIC and the label states the one that is true
/// for `dialect`. Every backend resolves a DDL migration carrying a guard probe
/// against a live catalog snapshot under the apply lock. MySQL also names its
/// fail-closed column-type boundary.
///
/// Does NOT change the statement rendered beneath the label on any dialect, and
/// does NOT change apply behaviour - this is preview text only.
fn guard_label(op: &Op, g: ExistenceGuard, dialect: SqlDialect) -> String {
    let kind = op_kind_tag(op);
    let subject = op_subject(op);
    let dir = match g {
        ExistenceGuard::IfNotExists => "ifNotExists",
        ExistenceGuard::IfExists => "ifExists",
    };
    let newly_live = newly_live_drop_note(op, g);
    match dialect {
        SqlDialect::Postgres | SqlDialect::Sqlite => format!(
            "{RUNTIME_RESOLVED} {kind} {subject} ({dir}): catalog-probed at apply \
             (run / satisfied-noop / fail-drift); the statement below is the bare DDL the apply \
             runs when the probe says run{newly_live}"
        ),
        SqlDialect::Mysql => format!(
            "{RUNTIME_RESOLVED} {kind} {subject} ({dir}): catalog-probed at apply \
             (run / satisfied-noop / fail-drift); present createTable/addColumn is refused until \
             MySQL column-type equality is implemented; the statement below is the bare DDL the \
             apply runs when the probe says run{newly_live}"
        ),
    }
}

/// The extra plan-time sentence a guarded `dropPartition` carries, or `""`.
///
/// Engines before the shape-aware partition probe resolved a partition guard against
/// the TOP-LEVEL table list, which never holds a child partition, so a guarded
/// `dropPartition` read its target as absent, skipped the `DROP TABLE`, and still
/// journaled the migration completed. Every database that already ran such a
/// migration keeps its orphan partition under a green journal, and the fix cannot
/// repair it - but the NEXT environment rebuilt from the same authored history (a
/// fresh staging DB, a new region, a DR restore, a per-PR database) replays the
/// identical text and now ACTUALLY drops it. Two environments then diverge in the
/// destructive direction from migration text that reads as already-proven.
///
/// The destructive-approval gate cannot warn about this: a guarded `dropPartition`
/// already lowers `destructive + requires_approval`, byte-identically to an
/// unguarded one, and did so throughout the period the drop was being silently
/// cancelled - so approving it never meant "this will drop". This sentence is the
/// only place the plan says the guard weakens the drop's PRECONDITION rather than
/// cancelling the drop.
///
/// Preview text only: it changes no rendered statement and no apply verdict, and it
/// deliberately does NOT predict WHICH verdict this apply will get - that is decided
/// against the live catalog under the apply lock. Covers `dropPartition` only:
/// `detachPartition` carries no existence guard, and the guarded `createPartition`
/// leg is additive. Does NOT appear on the op-less `.sql` preview path, which has no
/// op to inspect.
fn newly_live_drop_note(op: &Op, g: ExistenceGuard) -> &'static str {
    match (op, g) {
        (Op::DropPartition { .. }, ExistenceGuard::IfExists) => {
            "; a run verdict DROPS the partition and every row in it - this guard \
             no-ops only when the child is genuinely absent, never merely because a \
             partition is not a top-level table"
        }
        _ => "",
    }
}

/// A `Dml` comment line (op context): the op kind + native bind count.
fn dml_comment(op: &Op, binds: &[BindValue]) -> String {
    format!(
        "-- {} DML; binds: {} typed value(s) (bound natively, never interpolated)",
        op_kind_tag(op),
        binds.len()
    )
}

/// The `"schema"."table"."column"`-ish subject for a label, best-effort from the op.
fn op_subject(op: &Op) -> String {
    match op {
        Op::CreateTable { name, .. }
        | Op::CreatePartition { name, .. }
        | Op::DropPartition { name, .. }
        | Op::DropTable { table: name, .. } => quote_dotted(&[name]),
        Op::AttachPartition { parent, name, .. } | Op::DetachPartition { parent, name, .. } => {
            format!("{} → {}", quote_dotted(&[parent]), quote_dotted(&[name]))
        }
        Op::SetTableOptions { table, .. } => quote_dotted(&[table]),
        Op::AddColumn { table, column, .. }
        | Op::DropColumn { table, column, .. }
        | Op::SetColumnType { table, column, .. }
        | Op::SetColumnNotNull { table, column, .. }
        | Op::DropColumnNotNull { table, column, .. }
        | Op::SetColumnDefault { table, column, .. }
        | Op::DropColumnDefault { table, column, .. } => quote_dotted(&[table, column]),
        Op::CreateIndex { table, name, .. } => match name {
            Some(n) => quote_dotted(&[table, n]),
            None => quote_dotted(&[table]),
        },
        Op::DropIndex { name, table, .. } => match table {
            Some(t) => quote_dotted(&[t, name]),
            None => quote_dotted(&[name]),
        },
        Op::RenameColumn {
            table, from, to, ..
        } => {
            format!(
                "{} → {}",
                quote_dotted(&[table, from]),
                quote_dotted(&[table, to])
            )
        }
        Op::AlterPrimaryKey { table, .. } => quote_dotted(&[table]),
        Op::SynchronizeIdentity { table, column, .. } => quote_dotted(&[table, column]),
        Op::RenameTable { table, to, .. } => {
            format!("{} → {}", quote_dotted(&[table]), quote_dotted(&[to]))
        }
        Op::CreateEnum { name, .. }
        | Op::DropEnum { name, .. }
        | Op::CreateDomain { name, .. }
        | Op::DropDomain { name, .. }
        | Op::CreateSequence { name, .. }
        | Op::AlterSequence { name, .. }
        | Op::DropSequence { name, .. } => quote_dotted(&[name]),
        Op::AddConstraint { table, .. } => quote_dotted(&[table]),
        Op::DropConstraint { table, name, .. } => quote_dotted(&[table, name]),
        Op::ValidateConstraint { table, name, .. } => quote_dotted(&[table, name]),
        Op::Insert { table, .. } | Op::Update { table, .. } | Op::Delete { table, .. } => {
            quote_dotted(&[table])
        }
        Op::Backfill { table, .. } => quote_dotted(&[table]),
        Op::CreateView { name, .. } | Op::DropView { name, .. } => quote_dotted(&[name]),
        Op::Comment { target, .. } => match target {
            CommentTarget::Table { name, .. }
            | CommentTarget::Index { name, .. }
            | CommentTarget::View { name, .. }
            | CommentTarget::Type { name, .. }
            | CommentTarget::Sequence { name, .. }
            | CommentTarget::Function { name, .. } => quote_dotted(&[name]),
            CommentTarget::Column { table, name, .. }
            | CommentTarget::Constraint { table, name, .. } => quote_dotted(&[table, name]),
        },
        // VENDOR (`zero-migrate`) — the best-effort subject is the named
        // object (schema / extension / role / function) or the table+name for the
        // table-scoped RLS/policy/trigger ops.
        Op::CreateSchema { name, .. }
        | Op::DropSchema { name, .. }
        | Op::CreateExtension { name, .. }
        | Op::DropExtension { name, .. }
        | Op::CreateRole { name, .. }
        | Op::AlterRole { name, .. }
        | Op::DropRole { name, .. }
        | Op::CreateFunction { name, .. }
        | Op::DropFunction { name, .. } => quote_dotted(&[name]),
        Op::DropOwnedBy { roles } => quote_dotted(&[&roles.join(", ")]),
        Op::Grant { .. } | Op::Revoke { .. } => quote_dotted(&["<grant>"]),
        Op::Dialectal { .. } => quote_dotted(&["<dialectal>"]),
        Op::SetRls { table, .. } => quote_dotted(&[table]),
        Op::CreatePolicy { name, table, .. }
        | Op::DropPolicy { name, table, .. }
        | Op::CreateTrigger { name, table, .. }
        | Op::DropTrigger { name, table, .. } => quote_dotted(&[table, name]),
        Op::PgRaw { .. } => quote_dotted(&["<pgRaw>"]),
    }
}

/// The rename subject `"t"."from" → "t"."to"` for an online rename op.
fn rename_subject(op: &Op) -> String {
    match op {
        Op::RenameColumn {
            table, from, to, ..
        } => {
            format!(
                "{} → {}",
                quote_dotted(&[table, from]),
                quote_dotted(&[table, to])
            )
        }
        // Defensive: a non-rename op reaching the online-rename render is a logic
        // error, but never fabricate — fall back to the kind subject.
        other => op_subject(other),
    }
}

/// Join identifiers as `"a"."b"."c"` — a DISPLAY quoting for labels only (NOT the
/// engine's render seam; labels are comments, never executed).
fn quote_dotted(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| format!("{p:?}"))
        .collect::<Vec<_>>()
        .join(".")
}

/// The human probe-kind tag for a guarded DDL step with no op context.
fn probe_kind(p: &crate::model::probe::GuardProbe) -> &str {
    use crate::model::probe::GuardProbe;
    match p {
        GuardProbe::Table { .. } => "table",
        GuardProbe::Partition { .. } => "partition",
        GuardProbe::Column { .. } => "column",
        GuardProbe::Index { .. } => "index",
        GuardProbe::Constraint { .. } => "constraint",
        GuardProbe::View { .. } => "view",
        GuardProbe::Sequence { .. } => "sequence",
        GuardProbe::NamedType { kind, .. } => kind,
        GuardProbe::ColumnPresence { .. } => "column-presence",
    }
}

/// The probe a preview may describe as a guard the AUTHOR wrote. An ownership-only
/// index probe is engine-side fail-closed wiring stamped onto an UNGUARDED create,
/// so labeling it would announce an `ifNotExists` the migration text does not
/// contain. Does NOT alter the statement rendered beneath the label, and does NOT
/// hide a real guard on any op.
fn authored_probe(
    m: &crate::model::migration::Migration,
) -> Option<&crate::model::probe::GuardProbe> {
    match m.existence_guard.as_ref()? {
        crate::model::probe::GuardProbe::Index {
            ownership_only: true,
            ..
        } => None,
        probe => Some(probe),
    }
}

/// A placeholder `ExistenceGuard` direction derived from a migration's probe, used
/// only on the op-less path where we know a guard exists but not its op direction.
fn guard_dir(m: &crate::model::migration::Migration) -> ExistenceGuard {
    use crate::model::probe::{GuardDir, GuardProbe};
    let dir = match &m.existence_guard {
        Some(
            GuardProbe::Table { direction, .. }
            | GuardProbe::Partition { direction, .. }
            | GuardProbe::Column { direction, .. }
            | GuardProbe::Index { direction, .. }
            | GuardProbe::Constraint { direction, .. }
            | GuardProbe::View { direction, .. }
            | GuardProbe::Sequence { direction, .. }
            | GuardProbe::NamedType { direction, .. }
            | GuardProbe::ColumnPresence { direction, .. },
        ) => *direction,
        None => GuardDir::IfNotExists,
    };
    match dir {
        GuardDir::IfNotExists => ExistenceGuard::IfNotExists,
        GuardDir::IfExists => ExistenceGuard::IfExists,
    }
}

/// Push a SQL `up`/`template` body as a terminated statement, splitting nothing
/// (the engine's `up` may bundle multiple `;`-separated statements verbatim). We
/// print it as-is + ensure a trailing `;` for readability if absent.
fn push_statement(sql: &str, out: &mut Vec<Rendered>) {
    let trimmed = sql.trim_end();
    if trimmed.is_empty() {
        return;
    }
    let text = if trimmed.ends_with(';') {
        trimmed.to_string()
    } else {
        format!("{trimmed};")
    };
    out.push(Rendered::statement(text));
}

/// Indent a (possibly multi-line) SQL body two spaces, as a `--` comment block, for
/// the labeled expand/contract additive-DDL sub-block.
fn indent_sql(sql: &str) -> String {
    sql.trim_end()
        .lines()
        .map(|l| format!("--   {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Write the per-plan header block. `caption` decides whether the parenthetical is a
/// dialect claim (lowered IR envelope) or a verbatim-raw-`.sql` disclaimer.
fn write_plan_header(out: &mut String, plan: &AppliedPlan, caption: DialectCaption) {
    let _ = writeln!(
        out,
        "-- ============================================================"
    );
    let _ = writeln!(
        out,
        "-- plan {} {:?}  {}",
        plan.version.as_str(),
        plan.name,
        caption.header_suffix()
    );
    let _ = writeln!(
        out,
        "-- ============================================================"
    );
}

/// Write the document-level honest header. `caption` carries the dialect claim (for
/// the lowered legs) or the verbatim-raw-`.sql` disclaimer (the raw `.sql` leg).
fn write_doc_header(out: &mut String, caption: DialectCaption) {
    let _ = writeln!(
        out,
        "-- zero-migrate offline SQL preview {}",
        caption.header_suffix()
    );
    let _ = writeln!(
        out,
        "-- Shows the offline-renderable SQL the pending set WOULD run. DB-state-dependent ops"
    );
    let _ = writeln!(
        out,
        "-- (online-rename backfill/cutover, SQLite rebuild, existence-guarded ops) are LABELED"
    );
    let _ = writeln!(
        out,
        "-- `{RUNTIME_RESOLVED} …`, never fabricated. NOT executed; for operator go-live review."
    );
}

/// Whether this preview needs the MySQL session envelope. A label-only preview
/// stays comment-only: it must not gain executable SQL solely from formatting.
fn needs_mysql_session_envelope(dialect: SqlDialect, rendered: &[Rendered]) -> bool {
    dialect == SqlDialect::Mysql && rendered.iter().any(|r| r.statement)
}

/// Write the MySQL session setup before any author SQL. The mode append preserves
/// every inherited mode while pinning the literal grammar required by lowering.
fn write_mysql_session_prologue(out: &mut String) {
    write_preview_statement(out, MYSQL_PREVIEW_SAVE_SQL_MODE);
    write_preview_statement(out, MYSQL_PREVIEW_PIN_SQL_MODE);
}

/// Restore the exact `sql_mode` value captured before the preview ran.
fn write_mysql_session_epilogue(out: &mut String) {
    write_preview_statement(out, MYSQL_PREVIEW_RESTORE_SQL_MODE);
}

fn write_preview_statement(out: &mut String, statement: &str) {
    out.push('\n');
    let _ = writeln!(out, "{statement}");
}

/// Apply the same save/pin/restore contract to the programmatic executable-
/// statement preview. An empty author stream remains empty.
fn wrap_mysql_statements(dialect: SqlDialect, statements: Vec<String>) -> Vec<String> {
    if dialect != SqlDialect::Mysql || statements.is_empty() {
        return statements;
    }

    let mut wrapped = Vec::with_capacity(statements.len() + 3);
    wrapped.push(MYSQL_PREVIEW_SAVE_SQL_MODE.to_string());
    wrapped.push(MYSQL_PREVIEW_PIN_SQL_MODE.to_string());
    wrapped.extend(statements);
    wrapped.push(MYSQL_PREVIEW_RESTORE_SQL_MODE.to_string());
    wrapped
}

/// Flush rendered lines into the output, one blank line between statements.
fn write_rendered(out: &mut String, rendered: &[Rendered]) {
    for r in rendered {
        out.push('\n');
        let _ = writeln!(out, "{}", r.text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_IR: &str = r#"{
      "ir_version": 1,
      "name": "preview_mode",
      "ops": [
        {"op":"dropTable","table":"widgets"}
      ]
    }"#;

    fn opts() -> PreviewOpts {
        PreviewOpts {
            default_schema: "public".to_string(),
            owner_app: "app_preview".to_string(),
            effective_policy: crate::test_fixtures::no_inject("public"),
        }
    }

    fn assert_appears_in_order(text: &str, needles: &[&str]) {
        let mut remainder = text;
        for needle in needles {
            let offset = remainder
                .find(needle)
                .unwrap_or_else(|| panic!("missing {needle:?} in preview:\n{text}"));
            remainder = &remainder[offset + needle.len()..];
        }
    }

    #[test]
    fn mysql_human_preview_saves_pins_and_restores_sql_mode() {
        let out = render_ir_envelope_sql(SIMPLE_IR, SqlDialect::Mysql, &opts())
            .expect("MySQL IR renders offline");

        assert_appears_in_order(
            &out,
            &[
                MYSQL_PREVIEW_SAVE_SQL_MODE,
                MYSQL_PREVIEW_PIN_SQL_MODE,
                "DROP TABLE",
                MYSQL_PREVIEW_RESTORE_SQL_MODE,
            ],
        );
        assert_eq!(out.matches(MYSQL_PREVIEW_SAVE_SQL_MODE).count(), 1, "{out}");
        assert_eq!(
            out.matches(MYSQL_PREVIEW_RESTORE_SQL_MODE).count(),
            1,
            "{out}"
        );
        assert!(
            out.contains("-- preview: 1 statement(s) rendered"),
            "the safety envelope must not inflate the migration-statement tally:\n{out}"
        );
    }

    #[test]
    fn mysql_executable_statement_preview_includes_session_envelope() {
        let (name, statements) =
            render_ir_envelope_sql_statements(SIMPLE_IR, SqlDialect::Mysql, &opts())
                .expect("MySQL executable preview renders offline");

        assert_eq!(name, "preview_mode");
        assert_eq!(statements.len(), 4, "{statements:#?}");
        assert_eq!(statements[0], MYSQL_PREVIEW_SAVE_SQL_MODE);
        assert_eq!(statements[1], MYSQL_PREVIEW_PIN_SQL_MODE);
        assert!(statements[2].starts_with("DROP TABLE"), "{statements:#?}");
        assert_eq!(statements[3], MYSQL_PREVIEW_RESTORE_SQL_MODE);
    }

    #[test]
    fn mysql_plan_and_set_previews_wrap_the_whole_author_stream() {
        let ir: MigrationIr = serde_json::from_str(SIMPLE_IR).expect("fixture parses");
        let effective = crate::test_fixtures::no_inject("public");
        let author = IrAuthor::new("public", "app_preview", SqlDialect::Mysql, &effective);
        let plan = author
            .lower_plan(&ir, &LiveSchema::default())
            .expect("fixture lowers offline");

        for out in [
            render_plan_sql(&plan, SqlDialect::Mysql, &opts()),
            render_set_sql(&[plan], SqlDialect::Mysql, &opts()),
        ] {
            assert_appears_in_order(
                &out,
                &[
                    MYSQL_PREVIEW_SAVE_SQL_MODE,
                    MYSQL_PREVIEW_PIN_SQL_MODE,
                    "DROP TABLE",
                    MYSQL_PREVIEW_RESTORE_SQL_MODE,
                ],
            );
            assert_eq!(out.matches(MYSQL_PREVIEW_SAVE_SQL_MODE).count(), 1, "{out}");
            assert_eq!(
                out.matches(MYSQL_PREVIEW_RESTORE_SQL_MODE).count(),
                1,
                "{out}"
            );
        }
    }

    #[test]
    fn non_mysql_and_empty_statement_previews_do_not_gain_an_envelope() {
        let (_, postgres) =
            render_ir_envelope_sql_statements(SIMPLE_IR, SqlDialect::Postgres, &opts())
                .expect("Postgres executable preview renders offline");
        assert_eq!(postgres.len(), 1, "{postgres:#?}");
        assert!(!postgres.iter().any(|s| s.contains("sql_mode")));

        assert!(wrap_mysql_statements(SqlDialect::Mysql, Vec::new()).is_empty());
    }

    #[test]
    fn alter_primary_key_preview_carries_every_authored_review_fact() {
        let ir = r#"{
          "ir_version": 1,
          "name": "review_key_swap",
          "ops": [{
            "op": "alterPrimaryKey",
            "table": "orders",
            "action": {
              "kind": "replace",
              "expectedColumns": ["id"],
              "columns": ["tenant_id", "order_id"],
              "dropIdentityFrom": ["id"]
            }
          }]
        }"#;
        let out = render_ir_envelope_sql(ir, SqlDialect::Postgres, &opts())
            .expect("runtime-resolved lifecycle preview renders");
        for fact in [
            "expectedColumns=[\"id\"]",
            "columns=[\"tenant_id\", \"order_id\"]",
            "dropIdentityFrom=Some([\"id\"])",
        ] {
            assert!(out.contains(fact), "missing {fact:?} from preview:\n{out}");
        }
        assert!(out.contains(RUNTIME_RESOLVED), "{out}");
    }
}
