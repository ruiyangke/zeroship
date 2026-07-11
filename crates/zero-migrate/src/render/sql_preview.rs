//! **PR14 — the offline `--sql` plan preview** (the canonical Alembic `--sql` /
//! Atlas / Flyway / dbmate feature, here for OPERATOR GO-LIVE REVIEW).
//!
//! This module is a **surfacing / formatting layer over the SQL the engine ALREADY
//! lowers** — it re-implements NOTHING. Given a lowered [`AppliedPlan`] (from
//! [`loader::load_dir`](crate::plan::loader::load_dir) for a `.sql` artifact, or from
//! [`IrAuthor::lower_plan`](crate::render::lower::IrAuthor::lower_plan) for an
//! `.ir.json`), it walks the steps and prints the SQL strings already held in each
//! step (`Migration.up`, `PlanStep::Dml.template`) verbatim. It NEVER renders SQL
//! itself.
//!
//! # The honest boundary (the load-bearing design point, deliverable 2)
//!
//! DB-INDEPENDENT ops — `createTable`/`dropTable`/`addColumn`/`dropColumn`/
//! `addForeignKey`/`addUnique`/`addCheck`/`dropConstraint`/`createIndex`/
//! `dropIndex` + one-shot `insert`/`update`/`delete` — render their REAL SQL: their
//! `up`/`template` is fully determined offline (`IrAuthor::lower_*` lowers them with
//! an EMPTY [`LiveSchema`](crate::render::lower::LiveSchema), needing no DB).
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
//! - **any existence-guard-carrying op** (`ifNotExists`/`ifExists`) — the apply is
//!   a runtime catalog probe + run / satisfied-noop / fail-drift decision
//!   ([`guard_probe`](crate::render::existence_probe), explicitly NOT offline-renderable). The
//!   bare DDL `up` IS real SQL the apply runs when the probe says "run", so we
//!   print it under the label — but we do NOT invent an `IF [NOT] EXISTS` clause
//!   the engine never emits.
//! - **stand-alone SQLite `alterColumn*` / `addConstraint` / `dropConstraint`** —
//!   reconciled by the live 12-step rebuild; do not lower offline
//!   ([`IrLowerError::SqliteRebuildOnly`](crate::render::lower::IrLowerError)).
//!
//! The preview is HONEST that it shows the offline-renderable subset and labels the
//! rest; the header + trailing summary state exactly that.

use std::fmt::Write as _;

use crate::model::ir::{CommentTarget, ExistenceGuard, MigrationIr, Op};
use crate::render::lower::{op_kind_tag, IrAuthor, IrLowerError, LiveSchema};
use crate::render::plan::AppliedPlan;
use crate::render::step::{BindValue, PlanStep, RenameStep};
use zero_migrate_schema::query::SqlDialect;

/// The label prefix every runtime-resolved line carries — the single sentinel the
/// no-fabrication tests assert on. If you change this, change the tests.
pub const RUNTIME_RESOLVED: &str = "-- [runtime-resolved]";

/// Options for the offline preview render.
#[derive(Debug, Clone)]
pub struct PreviewOpts {
    /// The trust profile's effective schema for an op that omits its own qualifier
    /// (§2.7). The general/Trusted CLI default is `public`
    /// ([`DEFAULT_GENERIC_SCHEMA`](crate)); the Confined platform path pins the
    /// project schema. NEVER requires a DB to pick — it is a flag/profile value.
    pub default_schema: String,
    /// The declaring app stamped onto lowered migrations (ownership is enforced
    /// upstream by the load gate; here it only affects DML journal identity, never
    /// the rendered SQL). For the preview it is a cosmetic attribution.
    pub owner_app: String,
}

impl Default for PreviewOpts {
    fn default() -> Self {
        Self { default_schema: "public".to_string(), owner_app: "app_preview".to_string() }
    }
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
/// caption (MED-1). The `.ir.json` leg is genuinely per-dialect LOWERED, so its
/// header may claim the dialect. The raw `.sql` (Flyway, operator-authored) leg is
/// printed VERBATIM — it is NOT dialect-transformed — so captioning it with a
/// `(dialect: sqlite)` claim would mislead an operator reviewing a SQLite go-live
/// when the file is actually PG-only SQL. That leg gets a verbatim caption instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialectCaption {
    /// The body was lowered for this dialect — claim it.
    Lowered(SqlDialect),
    /// The body is operator-authored raw `.sql`, shown verbatim — NOT transformed.
    /// `requested` is the `--dialect` the operator asked for (named only to be
    /// explicit that we did NOT honour it for raw SQL).
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
                "(verbatim raw .sql — NOT dialect-transformed; --dialect {} ignored for raw SQL)",
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
        Self { text, runtime_resolved: false, statement: true }
    }
    fn label(text: String) -> Self {
        Self { text, runtime_resolved: true, statement: false }
    }
    fn comment(text: String) -> Self {
        Self { text, runtime_resolved: false, statement: false }
    }
}

/// Render ONE already-lowered [`AppliedPlan`] to its offline SQL preview string
/// (deliverable 1). Pure + DB-free: it reads back the SQL the lowering already
/// produced. `dialect` is used only for the header label (the plan's steps were
/// already lowered for a dialect by the caller).
#[must_use]
pub fn render_plan_sql(plan: &AppliedPlan, dialect: SqlDialect, _opts: &PreviewOpts) -> String {
    let rendered = render_plan_steps(plan);
    let mut out = String::new();
    write_plan_header(&mut out, plan, DialectCaption::Lowered(dialect));
    write_rendered(&mut out, &rendered);
    out
}

/// Render a SET of plans (a whole migration directory) to one preview document
/// with a leading honest header and a trailing tally.
///
/// This is the RAW `.sql` (Flyway, operator-authored) leg: each plan's body is the
/// verbatim operator SQL, which is NOT dialect-transformed (MED-1). The headers
/// therefore carry a `(verbatim raw .sql — NOT dialect-transformed)` caption rather
/// than a `(dialect: …)` claim, so an operator reviewing a SQLite go-live is never
/// misled into thinking PG-only verbatim SQL was lowered for SQLite. `dialect` here
/// is only the operator's REQUESTED target (surfaced as "ignored for raw SQL").
#[must_use]
pub fn render_set_sql(plans: &[AppliedPlan], dialect: SqlDialect, _opts: &PreviewOpts) -> String {
    let caption = DialectCaption::VerbatimRawSql { requested: dialect };
    let mut out = String::new();
    write_doc_header(&mut out, caption);
    let mut total_statements = 0usize;
    let mut total_runtime = 0usize;
    for plan in plans {
        let rendered = render_plan_steps(plan);
        total_statements += rendered.iter().filter(|r| r.statement).count();
        total_runtime += rendered.iter().filter(|r| r.runtime_resolved).count();
        out.push('\n');
        write_plan_header(&mut out, plan, caption);
        write_rendered(&mut out, &rendered);
    }
    let _ = writeln!(
        out,
        "\n-- preview: {total_statements} statement(s) rendered, {total_runtime} runtime-resolved"
    );
    out
}

/// Load + lower an `.ir.json` artifact's bytes OFFLINE (no DB — an EMPTY
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
pub fn render_ir_json_sql(
    bytes: &str,
    dialect: SqlDialect,
    opts: &PreviewOpts,
) -> Result<String, String> {
    let (name, rendered) = render_ir_json_rendered(bytes, dialect, opts)?;
    let mut out = String::new();
    // Synthesize a plan header from the IR identity (no full AppliedPlan needed —
    // a single un-lowerable op would otherwise make `lower_plan` abort).
    let _ = writeln!(out, "-- ============================================================");
    let _ = writeln!(
        out,
        "-- plan {:?}  {}",
        name,
        DialectCaption::Lowered(dialect).header_suffix()
    );
    let _ = writeln!(out, "-- ============================================================");
    write_rendered(&mut out, &rendered);
    let statements = rendered.iter().filter(|r| r.statement).count();
    let runtime = rendered.iter().filter(|r| r.runtime_resolved).count();
    let _ = writeln!(
        out,
        "\n-- preview: {statements} statement(s) rendered, {runtime} runtime-resolved"
    );
    Ok(out)
}

/// Load + lower an `.ir.json` artifact through the SAME tolerant path used by
/// [`render_ir_json_sql`], returning only executable statement text.
///
/// This is the DB-free lint seam: callers can feed these statements to the
/// advisory analyzers without scraping SQL back out of the human preview, while
/// runtime-resolved labels and comments stay out of the analyzer input.
///
/// # Errors
///
/// Returns an error when the IR document cannot be parsed or structurally
/// validated for offline rendering.
pub fn render_ir_json_sql_statements(
    bytes: &str,
    dialect: SqlDialect,
    opts: &PreviewOpts,
) -> Result<(String, Vec<String>), String> {
    let (name, rendered) = render_ir_json_rendered(bytes, dialect, opts)?;
    let statements = rendered
        .into_iter()
        .filter(|r| r.statement)
        .map(|r| r.text)
        .collect();
    Ok((name, statements))
}

fn render_ir_json_rendered(
    bytes: &str,
    dialect: SqlDialect,
    opts: &PreviewOpts,
) -> Result<(String, Vec<Rendered>), String> {
    // Parse the IR document WITHOUT the ownership/registry gate (this is an offline
    // operator preview, not a deploy): `serde` the wire shape, then validate its
    // structure. We deliberately do NOT call `load_ir_document` — that gate stamps
    // server ownership and consults a cross-app registry which has no meaning
    // offline. The structural validator is enough to refuse a malformed artifact.
    let ir: MigrationIr = serde_json::from_str(bytes)
        .map_err(|e| format!("parse .ir.json: {e}"))?;
    let target = match dialect {
        SqlDialect::Postgres => crate::model::validate::Dialect::Postgres,
        SqlDialect::Sqlite => crate::model::validate::Dialect::Sqlite,
        SqlDialect::Mysql => crate::model::validate::Dialect::Mysql,
    };
    crate::model::validate::validate_ir(&ir, target, &[])
        .map_err(|e| format!("validate .ir.json: {e}"))?;

    // The general/Trusted operator preview renders into the chosen default schema:
    // bind it as the author's project schema, so an op with NO qualifier (or one
    // matching the default) renders there. A truly FOREIGN explicit qualifier is
    // out of the Confined `Single(default_schema)` scope and fails to lower → it is
    // labeled `[runtime-resolved]` "not offline-renderable" rather than rendered
    // into the wrong schema (honest, fail-closed). NEVER requires a DB to pick.
    let author = IrAuthor::new(opts.default_schema.clone(), opts.owner_app.clone(), dialect);

    let live = LiveSchema::default();
    let rendered = render_ir_ops(&author, &ir, &live);
    Ok((ir.name, rendered))
}

/// Per-op lowering for the `.ir.json` path: lower each op in isolation so a single
/// DB-state-dependent op (SQLite rename / rebuild-only) degrades to a label instead
/// of aborting the whole preview. Mirrors the per-op iteration `lower_steps` does,
/// but tolerant: a `lower_plan` error on a one-op IR ⇒ a runtime-resolved label.
fn render_ir_ops(author: &IrAuthor, ir: &MigrationIr, live: &LiveSchema) -> Vec<Rendered> {
    let mut out = Vec::new();
    for op in &ir.ops {
        // A guard-carrying op lowers FINE offline (the probe is stamped, not an
        // error) — but its apply is a runtime catalog-probe decision, so it MUST be
        // labeled. Detect it from the op directly (the lowered `up` is bare DDL with
        // no `IF [NOT] EXISTS`; we print it under the label, never fabricating one).
        let guard = op.existence_guard();
        // Lower JUST this op via a one-op IR clone so an un-lowerable op is local.
        let one = single_op_ir(ir, op.clone());
        match author.lower_plan(&one, live) {
            Ok(plan) => {
                for step in &plan.steps {
                    render_step(op, guard, step, &mut out);
                }
            }
            Err(e) => {
                // The op cannot be lowered offline — it is DB-state-dependent. NEVER
                // fabricate SQL: emit a labeled, descriptive line citing WHY.
                out.push(Rendered::label(runtime_resolved_for_lower_error(op, &e)));
            }
        }
    }
    out
}

/// Build a one-op `MigrationIr` carrying `op`, sharing the parent's identity so the
/// per-op lower runs in the same author context. Strips cross-op concerns
/// (`depends_on`/`supersedes`/`preconditions`) — they do not affect per-op SQL.
fn single_op_ir(parent: &MigrationIr, op: Op) -> MigrationIr {
    MigrationIr {
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
/// (`Migration.existence_guard`), since a `.sql` plan carries no op list.
fn render_plan_steps(plan: &AppliedPlan) -> Vec<Rendered> {
    let mut out = Vec::new();
    for step in &plan.steps {
        // On the AppliedPlan path we have no `Op`; pass `None` for the op + read the
        // guard off the migration when present.
        render_step_no_op(step, &mut out);
    }
    out
}

/// Render one step WITH its originating op (the `.ir.json` path), so existence
/// guards and online renames are labeled with the op's subject.
fn render_step(op: &Op, guard: Option<ExistenceGuard>, step: &PlanStep, out: &mut Vec<Rendered>) {
    match step {
        PlanStep::Ddl(m) => {
            if let Some(g) = guard.or_else(|| m.existence_guard.as_ref().map(|_| guard_dir(m))) {
                out.push(Rendered::label(guard_label(op, g)));
            }
            push_statement(&m.up, out);
        }
        PlanStep::Dml { template, binds, .. } => {
            out.push(Rendered::comment(dml_comment(op, binds)));
            push_statement(template, out);
        }
        PlanStep::Backfill(spec) => {
            out.push(Rendered::label(format!(
                "{RUNTIME_RESOLVED} backfill {:?}.{:?}: windowed runtime loop over batches \
                 (cursor {:?}, batch {}); exact statement stream depends on live row count",
                spec.schema, spec.table, spec.cursor_column, spec.batch_size
            )));
        }
        PlanStep::OnlineRename(rename) => render_online_rename(op, rename, out),
    }
}

/// Render one step with NO op context (the `.sql` / AppliedPlan path).
fn render_step_no_op(step: &PlanStep, out: &mut Vec<Rendered>) {
    match step {
        PlanStep::Ddl(m) => {
            if let Some(p) = &m.existence_guard {
                out.push(Rendered::label(format!(
                    "{RUNTIME_RESOLVED} guarded DDL ({}): catalog-probed at apply \
                     (run / satisfied-noop / fail-drift); the statement below is the bare DDL",
                    probe_kind(p)
                )));
            }
            push_statement(&m.up, out);
        }
        PlanStep::Dml { template, binds, .. } => {
            out.push(Rendered::comment(format!(
                "-- DML; binds: {} typed value(s) (bound natively, never interpolated)",
                binds.len()
            )));
            push_statement(template, out);
        }
        PlanStep::Backfill(spec) => {
            out.push(Rendered::label(format!(
                "{RUNTIME_RESOLVED} backfill {:?}.{:?}: windowed runtime loop (cursor {:?}, \
                 batch {}); exact statement stream depends on live row count",
                spec.schema, spec.table, spec.cursor_column, spec.batch_size
            )));
        }
        PlanStep::OnlineRename(rename) => render_online_rename_no_op(rename, out),
    }
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
            "{RUNTIME_RESOLVED} {kind} {subject}: online rename — needs the live column \
             structure to lower (expand-contract dual-write / SQLite rebuild); the backfill is \
             windowed by PK and the cutover is partitioned across deploys — exact statement \
             stream depends on live state"
        ),
        other => format!(
            "{RUNTIME_RESOLVED} {kind} {subject}: not offline-renderable ({other})"
        ),
    }
}

/// The runtime-resolved label for a guard-carrying op (op context available).
fn guard_label(op: &Op, g: ExistenceGuard) -> String {
    let kind = op_kind_tag(op);
    let subject = op_subject(op);
    let dir = match g {
        ExistenceGuard::IfNotExists => "ifNotExists",
        ExistenceGuard::IfExists => "ifExists",
    };
    format!(
        "{RUNTIME_RESOLVED} {kind} {subject} ({dir}): catalog-probed at apply \
         (run / satisfied-noop / fail-drift); the statement below is the bare DDL the apply runs \
         when the probe says run"
    )
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
        Op::RenameColumn { table, from, to, .. } => {
            format!("{} → {}", quote_dotted(&[table, from]), quote_dotted(&[table, to]))
        }
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
        // VENDOR (`@zeroship/migrate`) — the best-effort subject is the named
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
        Op::RenameColumn { table, from, to, .. } => {
            format!("{} → {}", quote_dotted(&[table, from]), quote_dotted(&[table, to]))
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
        GuardProbe::Column { .. } => "column",
        GuardProbe::Index { .. } => "index",
        GuardProbe::Constraint { .. } => "constraint",
        GuardProbe::View { .. } => "view",
        GuardProbe::Sequence { .. } => "sequence",
        GuardProbe::NamedType { kind, .. } => kind,
        GuardProbe::ColumnPresence { .. } => "column-presence",
    }
}

/// A placeholder `ExistenceGuard` direction derived from a migration's probe, used
/// only on the op-less path where we know a guard exists but not its op direction.
fn guard_dir(m: &crate::model::migration::Migration) -> ExistenceGuard {
    use crate::model::probe::{GuardDir, GuardProbe};
    let dir = match &m.existence_guard {
        Some(
            GuardProbe::Table { direction, .. }
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
/// dialect claim (lowered `.ir.json`) or a verbatim-raw-`.sql` disclaimer (MED-1).
fn write_plan_header(out: &mut String, plan: &AppliedPlan, caption: DialectCaption) {
    let _ = writeln!(out, "-- ============================================================");
    let _ = writeln!(
        out,
        "-- plan {} {:?}  {}",
        plan.version.as_str(),
        plan.name,
        caption.header_suffix()
    );
    let _ = writeln!(out, "-- ============================================================");
}

/// Write the document-level honest header. `caption` carries the dialect claim (for
/// the lowered legs) or the verbatim-raw-`.sql` disclaimer (the raw `.sql` leg, MED-1).
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

/// Flush rendered lines into the output, one blank line between statements.
fn write_rendered(out: &mut String, rendered: &[Rendered]) {
    for r in rendered {
        out.push('\n');
        let _ = writeln!(out, "{}", r.text);
    }
}
