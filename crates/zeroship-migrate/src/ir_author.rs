//! `IrAuthor` — the DDL **Lower** phase (§6, §6.4, §6.5).
//!
//! `IrAuthor::lower` compiles a validated, ownership-checked [`MigrationIr`]
//! (DDL ops) into the same [`Migration`] shape the declarative differ produces.
//! It is the IR-path peer of `DeclarativeAuthor::diff`.
//!
//! # Single source of truth (§6.5 MANDATE)
//!
//! `IrAuthor` does **NOT** hand-construct snapshots, and does NOT re-spell the
//! default / system-field / encryption-/comment-sentinel logic. It routes every
//! op's fields through the SHARED, dialect-parameterized snapshot-builder
//! [`crate::declarative::build_table_snapshot`] — the SAME builder the differ's
//! `desired_snapshot_for_dialect` calls — and then renders the resulting
//! [`TableSnapshot`] / [`ColumnSnapshot`] / [`IndexSnapshot`] through the SAME
//! render methods the differ uses (`DeclarativeAuthor::lower_*`, which delegate to
//! `render_create_table` / the `DdlEmitter`). So the emitted SQL is byte-identical
//! to the declarative path BY CONSTRUCTION. The §6.4 cross-path byte-identity
//! golden (in `tests/ir_author_render_parity.rs`) guards against accidental
//! regression — not against two independent implementations.
//!
//! # The only IR-path-specific code: the type/shape MAPPING
//!
//! The one thing `IrAuthor` owns is mapping the IR op vocabulary
//! ([`Op`]/[`IrColumn`]/[`ColType`]/[`IrDefault`]) onto the descriptor shape the
//! shared builder consumes ([`FieldDescriptor`]). This is a pure structural
//! translation — it carries NO default/sentinel rendering (that stays in the
//! shared builder), so the §6.5 single-source guarantee holds.

use std::collections::BTreeSet;

use crate::declarative::{
    build_table_snapshot, CollectionDescriptor, DeclarativeAuthor, DeclarativeError,
    FieldDescriptor,
};
use crate::drift::{ColumnSnapshot, IndexSnapshot};
use crate::guard::{guard_for, GuardConfig, GuardError};
use crate::ir::{ColType, IrColumn, IrDefault, IndexMethod, MigrationIr, Op};
use crate::migration::Migration;
use zeroship_schema::query::SqlDialect;

/// The IR-path DDL author (§6). Wraps a [`DeclarativeAuthor`] so it reuses the
/// declarative render seam verbatim; the IR-specific work is the op→descriptor
/// mapping that feeds the shared snapshot-builder.
#[derive(Debug)]
pub struct IrAuthor {
    project_schema: String,
    decl: DeclarativeAuthor,
    dialect: SqlDialect,
}

/// A failure lowering an IR op to SQL.
#[derive(Debug, thiserror::Error)]
pub enum IrLowerError {
    /// The op's fields could not be modelled as a snapshot (e.g. an unknown type
    /// token, an unsafe ref target). Carries the shared builder's error.
    #[error(transparent)]
    Snapshot(#[from] DeclarativeError),
    /// An op `IrAuthor::lower` does not yet compile (DML / online-intent ops are a
    /// later wave; this Lower phase covers the DDL ops). Carries the op tag.
    #[error("IrAuthor::lower does not yet compile op {0:?} (DDL ops only in this wave)")]
    UnsupportedOp(&'static str),
}

/// One rendered SQL FRAGMENT of a lowered op, carrying its attribution (§6.1.1):
/// the originating op INDEX (its position in `MigrationIr::ops`) and the op's kind.
/// A single op can render multiple fragments (`createTable` emits the table + an
/// inline `COMMENT ON COLUMN` side output + per-table indexes), and each is
/// guarded INDIVIDUALLY so a denial is attributable to the exact op — not buried
/// in a concatenated `up` blob.
#[derive(Debug, Clone)]
pub struct GuardedFragment {
    /// The originating op's 0-based index in `MigrationIr::ops`.
    pub op_index: usize,
    /// The op's kind tag (e.g. `"createTable"`) — the human-facing attribution.
    /// (The `.ts` source-map location threads through the §5.1 provenance blob in
    /// a later wave; the op-index + kind is the attribution available at lower.)
    pub op_kind: &'static str,
    /// The single rendered SQL statement (NO trailing `;`), guarded as-is.
    pub sql: String,
}

/// A guard DENIAL attributed to the exact op that produced the denied fragment
/// (§6.1.1). The human message leads with the op-index + kind so an author/AI
/// sees *which* op the guard refused, not a bare "statement denied".
#[derive(Debug, thiserror::Error)]
#[error(
    "op #{op_index} ({op_kind}): rendered statement denied by guard: {source}"
)]
pub struct FragmentGuardDenied {
    /// The op whose rendered fragment the guard denied.
    pub op_index: usize,
    /// The op's kind tag.
    pub op_kind: &'static str,
    /// The underlying guard error.
    #[source]
    pub source: GuardError,
}

/// A failure of the guard-per-fragment lower ([`IrAuthor::lower_guarded`]):
/// lowering failed, OR a rendered fragment was denied by the guard (attributed to
/// its op, §6.1.1), OR the fragment-reassembly byte-identity invariant broke.
#[derive(Debug, thiserror::Error)]
pub enum IrGuardedLowerError {
    /// Lowering a validated op to SQL failed.
    #[error(transparent)]
    Lower(#[from] IrLowerError),
    /// A rendered fragment was denied by the guard, attributed to its op.
    #[error(transparent)]
    Denied(#[from] FragmentGuardDenied),
    /// The reassembly invariant `applied_up == join(guarded_fragments, ";\n")`
    /// broke for a lowered migration — an engine bug (fragment splitting that does
    /// not round-trip). Fail closed rather than apply a `up` that diverges from
    /// what was guarded.
    #[error(
        "fragment-reassembly invariant broke for migration {name:?}: the join of the \
         individually-guarded fragments is not byte-identical to the lowered `up` \
         (guard/render seam bug)"
    )]
    ReassemblyMismatch {
        /// The migration whose reassembly diverged.
        name: String,
    },
}

/// A failure in the loader's IR branch ([`IrAuthor::load_and_lower`]): either the
/// fail-closed LOAD GATE refused the artifact, or LOWERING a validated op failed.
#[derive(Debug, thiserror::Error)]
pub enum LoadAndLowerError {
    /// The `.ir.json` LOAD GATE refused the artifact (deserialize / ir_version /
    /// structural validate / ownership / checksum-hint).
    #[error(transparent)]
    Load(#[from] crate::ir_load::IrLoadError),
    /// Lowering a validated, owned op to SQL failed.
    #[error(transparent)]
    Lower(#[from] IrLowerError),
}

impl IrAuthor {
    /// Construct an IR author bound to a project schema + deploying app, for a
    /// target dialect. The deploying app is the `owner_app` stamped on every
    /// emitted migration (ownership is enforced UPSTREAM by the IR-load gate).
    #[must_use]
    pub fn new(
        project_schema: impl Into<String>,
        owner_app: impl Into<String>,
        dialect: SqlDialect,
    ) -> Self {
        let project_schema = project_schema.into();
        Self {
            decl: DeclarativeAuthor::new_for_dialect(
                project_schema.clone(),
                owner_app,
                dialect,
            ),
            project_schema,
            dialect,
        }
    }

    /// The loader's IR branch (§7.2): run the fail-closed `.ir.json` LOAD GATE
    /// (deserialize → `ir_version` → `validate_ir` → server-stamped ownership →
    /// advisory checksum-hint compare) and then LOWER the validated, owned IR to
    /// migrations. This is the single creator-facing entry the `.ir.json` deploy
    /// path calls — the peer of the platform `.sql` Flyway loader
    /// ([`crate::loader::load_dir`]), which never routes IR.
    ///
    /// `registry` is the project's table→owner map (drives the §8.6 ownership
    /// check); `live_tables` the tables already present (drives FK inline-vs-defer).
    ///
    /// # Errors
    /// - [`LoadAndLowerError::Load`] — the load gate refused the artifact
    ///   (malformed, future ir_version, structural reject incl. the fail-closed
    ///   bare-name DropIndex, ownership violation, or checksum-hint mismatch).
    /// - [`LoadAndLowerError::Lower`] — lowering a validated op failed.
    pub fn load_and_lower(
        &self,
        bytes: &str,
        deploying_app: &str,
        registry: &std::collections::BTreeMap<String, String>,
        live_tables: &BTreeSet<String>,
    ) -> Result<Vec<Migration>, LoadAndLowerError> {
        let target = match self.dialect {
            SqlDialect::Postgres => crate::validate::Dialect::Postgres,
            SqlDialect::Sqlite => crate::validate::Dialect::Sqlite,
        };
        let ir = crate::ir_load::load_ir_document(bytes, deploying_app, target, registry)
            .map_err(LoadAndLowerError::Load)?;
        self.lower(&ir, live_tables).map_err(LoadAndLowerError::Lower)
    }

    /// Lower a validated [`MigrationIr`]'s DDL ops to [`Migration`]s.
    ///
    /// `live_tables` is the set of tables already present in the project (so an FK
    /// to a live target inlines, and a non-live target defers on PG / errors on
    /// SQLite — mirroring `diff`). Tables created EARLIER in the same IR are added
    /// to the working live set as lowering proceeds, so an intra-migration FK
    /// inlines correctly.
    ///
    /// # Errors
    /// - [`IrLowerError::Snapshot`] — the shared builder rejected an op's fields.
    /// - [`IrLowerError::UnsupportedOp`] — a non-DDL op (DML / online intent).
    pub fn lower(
        &self,
        ir: &MigrationIr,
        live_tables: &BTreeSet<String>,
    ) -> Result<Vec<Migration>, IrLowerError> {
        let mut out: Vec<Migration> = Vec::new();
        let mut live: BTreeSet<String> = live_tables.clone();
        for op in &ir.ops {
            out.extend(self.lower_one_op(op, &mut live)?);
        }
        Ok(out)
    }

    /// Lower a SINGLE op to its migration(s), advancing the working `live` set
    /// when the op creates a table (so a later intra-IR FK inlines). Factored out
    /// of [`lower`] so the guard-per-fragment path ([`lower_guarded`]) can attribute
    /// each op's rendered fragments to its op index (§6.1.1).
    ///
    /// # Errors
    /// - [`IrLowerError::Snapshot`] — the shared builder rejected the op's fields.
    /// - [`IrLowerError::UnsupportedOp`] — a non-DDL op (DML / online intent).
    fn lower_one_op(
        &self,
        op: &Op,
        live: &mut BTreeSet<String>,
    ) -> Result<Vec<Migration>, IrLowerError> {
        let migs = match op {
            Op::CreateTable { name, columns, .. } => {
                let desc = self.create_table_descriptor(name, columns);
                let snap = build_table_snapshot(&self.project_schema, &desc, self.dialect)?;
                // The SQLite CREATE routes through the shared `zeroship_schema`
                // emitter, which consumes the SDK schema `Value` — built here from
                // the SAME descriptor bridge (`descriptor_to_sdk_schema`) the
                // differ's `desired_snapshot_for_dialect` uses, so the §6.4
                // byte-identity holds on the SQLite leg (the PG leg ignores it).
                let sqlite_schema = crate::declarative::descriptor_to_sdk_schema(&desc);
                let migs = self.decl.lower_create_table(name, &snap, &sqlite_schema, live)?;
                // The just-created table is now live for any later intra-IR FK.
                live.insert(name.clone());
                migs
            }
            Op::AddColumn { table, column, ty, nullable, default } => {
                let col =
                    self.add_column_snapshot(table, column, ty, *nullable, default.as_ref())?;
                vec![self.decl.lower_add_column(table, &col)]
            }
            Op::CreateIndex { table, columns, name, unique, using, .. } => {
                let idx = create_index_snapshot(table, columns, name.as_deref(), *unique, *using);
                vec![self.decl.lower_create_index(table, &idx)]
            }
            Op::DropTable { table, .. } => vec![self.decl.lower_drop_table(table)],
            Op::DropColumn { table, column, .. } => {
                vec![self.decl.lower_drop_column(table, column)]
            }
            Op::DropIndex { name, .. } => {
                // A bare-name DropIndex is rejected fail-closed UPSTREAM by the
                // validator (§8.6); a table-hinted one reaches here. The IR
                // DropIndex carries no unique hint, so the drop takes the plain
                // (non-destructive) gating — matching the differ, which classes
                // a unique-index drop destructive only when the DESIRED snapshot
                // proves the dropped index was unique. The render is the same
                // `DROP INDEX` regardless of the gating flag.
                let idx = IndexSnapshot::btree(name.clone(), false, Vec::new());
                vec![self.decl.lower_drop_index(&idx)]
            }
            Op::AlterColumnType { .. } => {
                return Err(IrLowerError::UnsupportedOp("alterColumnType"))
            }
            Op::AlterColumnNullability { .. } => {
                return Err(IrLowerError::UnsupportedOp("alterColumnNullability"))
            }
            Op::RenameColumn { .. } => return Err(IrLowerError::UnsupportedOp("renameColumn")),
            Op::AddConstraint { .. } => return Err(IrLowerError::UnsupportedOp("addConstraint")),
            Op::DropConstraint { .. } => {
                return Err(IrLowerError::UnsupportedOp("dropConstraint"))
            }
            Op::Insert { .. } => return Err(IrLowerError::UnsupportedOp("insert")),
            Op::Update { .. } => return Err(IrLowerError::UnsupportedOp("update")),
            Op::Delete { .. } => return Err(IrLowerError::UnsupportedOp("delete")),
            Op::Backfill { .. } => return Err(IrLowerError::UnsupportedOp("backfill")),
        };
        Ok(migs)
    }

    /// **Guard-per-fragment + reassembly (§6.1.1).** Lower the IR's DDL ops and,
    /// for EACH op, guard every rendered SQL FRAGMENT individually — carrying the
    /// op index + kind — BEFORE the step's `up` is assembled. Only after all of an
    /// op's fragments pass the guard is the `up` reassembled by joining exactly
    /// those guarded fragments with the canonical `;\n` separator, and the
    /// byte-identity invariant `applied_up == join(guarded_fragments)` is asserted.
    ///
    /// A DENIED fragment aborts the WHOLE lower with the op-index attribution
    /// ([`FragmentGuardDenied`]) and applies NOTHING — there is no partial plan.
    ///
    /// Returns the per-op guarded fragments (for status/DX attribution) alongside
    /// the lowered migrations whose `up` is provably the reassembly of those exact
    /// fragments. The SQLite leg's guard ([`crate::guard::SqliteDescriptorGuard`])
    /// trusts descriptor-/IR-generated DDL (no string deny-list), so it never
    /// denies — but the fragment split + reassembly invariant still runs, so the
    /// `up`↔fragment correspondence holds on both dialects.
    ///
    /// # Errors
    /// - [`IrGuardedLowerError::Lower`] — an op failed to lower.
    /// - [`IrGuardedLowerError::Denied`] — a rendered fragment was guard-denied.
    /// - [`IrGuardedLowerError::ReassemblyMismatch`] — the fragment split did not
    ///   round-trip (engine bug; fail closed).
    pub fn lower_guarded(
        &self,
        ir: &MigrationIr,
        guard_cfg: &GuardConfig,
        live_tables: &BTreeSet<String>,
    ) -> Result<(Vec<Migration>, Vec<GuardedFragment>), IrGuardedLowerError> {
        let guard = guard_for(guard_cfg);
        let mut migrations: Vec<Migration> = Vec::new();
        let mut fragments: Vec<GuardedFragment> = Vec::new();
        let mut live: BTreeSet<String> = live_tables.clone();

        for (op_index, op) in ir.ops.iter().enumerate() {
            let op_kind = op_kind_tag(op);
            // Lower this op (advancing `live` for intra-IR FK inlining). A lower
            // failure aborts before any guarding — nothing applied.
            let op_migs = self.lower_one_op(op, &mut live)?;

            for mig in op_migs {
                // Split the rendered `up` into its individual statement FRAGMENTS on
                // the canonical `;\n` separator the renderers emit between a
                // statement and its `COMMENT ON COLUMN` side output / follow-on
                // index. Guard EACH fragment individually so a denial is attributed
                // to THIS op (§6.1.1) — not buried in a concatenated blob.
                let frags = split_up_fragments(&mig.up);
                let mut guarded_for_mig: Vec<String> = Vec::with_capacity(frags.len());
                for frag in frags {
                    guard.check(frag).map_err(|source| FragmentGuardDenied {
                        op_index,
                        op_kind,
                        source,
                    })?;
                    fragments.push(GuardedFragment {
                        op_index,
                        op_kind,
                        sql: frag.to_string(),
                    });
                    guarded_for_mig.push(frag.to_string());
                }
                // Byte-identity invariant: the step's `up` MUST be exactly the join
                // of the fragments we just guarded — nothing inserted, rewritten, or
                // re-quoted between guarding and concatenation (§6.1.1).
                let reassembled = guarded_for_mig.join(";\n");
                if reassembled != mig.up {
                    return Err(IrGuardedLowerError::ReassemblyMismatch {
                        name: mig.name.clone(),
                    });
                }
                migrations.push(mig);
            }
        }
        Ok((migrations, fragments))
    }

    /// Map an IR `createTable` op to the [`CollectionDescriptor`] the shared
    /// snapshot-builder consumes. Pure structural translation — no default /
    /// sentinel rendering (that lives in the shared builder, §6.5).
    fn create_table_descriptor(&self, name: &str, columns: &[IrColumn]) -> CollectionDescriptor {
        CollectionDescriptor {
            name: name.to_string(),
            owner_app: self.decl.owner_app().to_string(),
            fields: columns.iter().map(ir_column_to_field).collect(),
            indexes: Vec::new(),
        }
    }

    /// Build the [`ColumnSnapshot`] for an `addColumn` op by routing its single
    /// field through the SHARED builder (a one-field descriptor) and pulling the
    /// matching column out — so the default / encryption / comment sentinel is
    /// built by the shared kernel, never re-spelled here (§6.5).
    fn add_column_snapshot(
        &self,
        table: &str,
        column: &str,
        ty: &ColType,
        nullable: Option<bool>,
        default: Option<&IrDefault>,
    ) -> Result<ColumnSnapshot, IrLowerError> {
        let field = ir_column_to_field(&IrColumn {
            name: column.to_string(),
            ty: ty.clone(),
            nullable,
            default: default.cloned(),
            unique: None,
        });
        let desc = CollectionDescriptor {
            name: table.to_string(),
            owner_app: self.decl.owner_app().to_string(),
            fields: vec![field],
            indexes: Vec::new(),
        };
        let snap = build_table_snapshot(&self.project_schema, &desc, self.dialect)?;
        snap.columns
            .into_iter()
            .find(|c| c.name == column)
            .ok_or(IrLowerError::UnsupportedOp("addColumn (column folded away)"))
    }

}

/// Split a lowered migration's `up` into its individual statement FRAGMENTS on
/// the canonical `;\n` separator the renderers emit between statements (§6.1.1).
/// `join(";\n")` over the result reproduces the input byte-for-byte (the
/// reassembly invariant `lower_guarded` asserts), so a fragment is exactly one
/// guardable statement with NO trailing `;`.
///
/// The renderers ALWAYS separate statements with the literal `;\n` and never emit
/// a `;\n` inside a statement (the only multi-statement ups are
/// `<stmt>;\n COMMENT ON COLUMN …` and `<create>;\n<index>` follow-ons — all
/// emitter-controlled, never carrying user free-text with an embedded `;\n`). A
/// single-statement `up` yields one fragment.
fn split_up_fragments(up: &str) -> Vec<&str> {
    up.split(";\n").collect()
}

/// The op kind tag for §6.1.1 attribution — the human-facing name the guard
/// denial / status surface leads with.
const fn op_kind_tag(op: &Op) -> &'static str {
    match op {
        Op::CreateTable { .. } => "createTable",
        Op::AddColumn { .. } => "addColumn",
        Op::CreateIndex { .. } => "createIndex",
        Op::DropTable { .. } => "dropTable",
        Op::DropColumn { .. } => "dropColumn",
        Op::DropIndex { .. } => "dropIndex",
        Op::AlterColumnType { .. } => "alterColumnType",
        Op::AlterColumnNullability { .. } => "alterColumnNullability",
        Op::RenameColumn { .. } => "renameColumn",
        Op::AddConstraint { .. } => "addConstraint",
        Op::DropConstraint { .. } => "dropConstraint",
        Op::Insert { .. } => "insert",
        Op::Update { .. } => "update",
        Op::Delete { .. } => "delete",
        Op::Backfill { .. } => "backfill",
    }
}

/// Build the [`IndexSnapshot`] for a `createIndex` op. A plain B-tree index is
/// the common case; a non-`btree` `using` carries the access method. Pure
/// translation (no state), so a free function.
fn create_index_snapshot(
    table: &str,
    columns: &[String],
    name: Option<&str>,
    unique: Option<bool>,
    using: Option<IndexMethod>,
) -> IndexSnapshot {
    let idx_name = name.map_or_else(
        || crate::author::cap_ident_name(&format!("{table}_{}_idx", columns.join("_"))),
        ToString::to_string,
    );
    let unique = unique.unwrap_or(false);
    let mut idx = IndexSnapshot::btree(idx_name, unique, columns.to_vec());
    if let Some(m) = using {
        idx.access_method = index_method_access(m).to_string();
    }
    idx
}

/// Map an [`IrColumn`] to the [`FieldDescriptor`] the shared snapshot-builder
/// consumes. Pure structural translation of the type + nullability + default +
/// unique; the snapshot's default/sentinel rendering is the shared builder's job.
fn ir_column_to_field(c: &IrColumn) -> FieldDescriptor {
    // `nullable` defaults to TRUE (the `t.*` lexicon — §3.2); `required` is the
    // inverse the descriptor models. An explicit `nullable: false` ⇒ required.
    let required = !c.nullable.unwrap_or(true);
    let (ty, references) = col_type_to_token(&c.ty);
    // An ENCRYPTED column carries the inner token as `ty` PLUS the `encrypted`
    // facet — the shared builder reads the facet to pick BYTEA + the `zsenc`
    // sentinel (built by the shared kernel, never re-spelled here, §6.5). The
    // empty `{}` selects the kernel's defaults (`randomised:default:<inner>`),
    // matching the differ's `t.encrypted(...)` shape.
    let encrypted = matches!(c.ty, ColType::Encrypted { .. })
        .then(|| serde_json::json!({}));
    FieldDescriptor {
        name: c.name.clone(),
        ty,
        required,
        unique: c.unique.unwrap_or(false),
        references,
        default: c.default.as_ref().and_then(ir_default_to_value),
        encrypted,
        ..Default::default()
    }
}

/// Map a closed [`ColType`] to the descriptor's `(type_token, references?)`. The
/// tokens are exactly the SDK `FieldDef` type spellings the shared kernel maps
/// (`def_to_column_type_for_dialect`).
fn col_type_to_token(ty: &ColType) -> (String, Option<String>) {
    match ty {
        ColType::String => ("string".into(), None),
        ColType::Text => ("string".into(), None),
        ColType::Int | ColType::BigInt => ("int".into(), None),
        ColType::Float => ("number".into(), None),
        ColType::Bool => ("boolean".into(), None),
        ColType::Json => ("json".into(), None),
        ColType::Timestamp => ("date".into(), None),
        ColType::Uuid => ("string".into(), None),
        ColType::Bytea => ("bytes".into(), None),
        ColType::Ref { references } => ("ref".into(), Some(references.clone())),
        ColType::Vector { .. } => ("vector".into(), None),
        ColType::GeoPoint => ("geoPoint".into(), None),
        ColType::Decimal { .. } => ("number".into(), None),
        // An encrypted column wraps an inner type; the descriptor carries it as the
        // inner token with the `encrypted` facet set (the shared builder reads the
        // facet to pick BYTEA + the sentinel). The inner token drives the masked
        // sibling's plaintext shape.
        ColType::Encrypted { of } => {
            let (inner, _) = col_type_to_token(of);
            (inner, None)
        }
    }
}

/// Map an [`IrDefault`] to the descriptor's `default` JSON value. A literal maps
/// to its scalar; a synth `now`/`genRandomUuid` is an apply-time default that the
/// `t.*` lexicon expresses as a `{ fn }` object — but the declarative differ only
/// emits IMMUTABLE literal defaults (never a volatile synth), so a synth default
/// is left to the shared builder's policy (carried as `None` here, matching the
/// differ which never sees a synth default on an autogenerated column).
fn ir_default_to_value(d: &IrDefault) -> Option<serde_json::Value> {
    use crate::ir::IrScalar;
    use serde_json::Value;
    match d {
        IrDefault::Literal { value } => Some(match value {
            IrScalar::Null => Value::Null,
            IrScalar::Bool(b) => Value::Bool(*b),
            IrScalar::Int(i) => Value::from(*i),
            // A decimal is carried as its canonical string; the descriptor's
            // `default` is rendered as a literal by the shared builder.
            IrScalar::Decimal(s) => Value::String(s.clone()),
            IrScalar::Str(s) => Value::String(s.clone()),
            // A bytes default is not an autogenerated-column default the differ
            // emits; carry it as its canonical base64 string for completeness.
            IrScalar::Bytes(b) => {
                use base64::Engine as _;
                Value::String(base64::engine::general_purpose::STANDARD.encode(b))
            }
        }),
        IrDefault::Fn { .. } => None,
    }
}

/// The access-method string for a closed [`IndexMethod`] — matches the spellings
/// the snapshot's `access_method` carries (and `render_create_index` emits).
fn index_method_access(m: IndexMethod) -> &'static str {
    match m {
        IndexMethod::Btree => "btree",
        IndexMethod::Gin => "gin",
        IndexMethod::Gist => "gist",
        IndexMethod::Ivfflat => "ivfflat",
        IndexMethod::Hnsw => "hnsw",
        IndexMethod::Fts5 => "fts5",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn registry(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(t, o)| (t.to_string(), o.to_string())).collect()
    }

    use crate::ir::{IrColumn as TIrColumn, IrFlagsOverride};

    /// Build a one-op `createTable` IR for the guard-per-fragment tests.
    fn create_table_ir(table: &str, cols: Vec<TIrColumn>) -> MigrationIr {
        MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::CreateTable {
                name: table.into(),
                columns: cols,
                constraints: vec![],
                indexes: vec![],
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        }
    }

    // §6.1.1 — the byte-identity invariant: for a MULTI-statement op (a
    // createTable with an encrypted column → `CREATE TABLE …;\nCOMMENT ON COLUMN
    // …`), the lowered `up` is byte-identical to the join of the individually
    // guarded fragments, and >1 fragment is actually guarded.
    #[test]
    fn guard_per_fragment_reassembly_is_byte_identical_pg() {
        let ir = create_table_ir(
            "vault",
            vec![TIrColumn {
                name: "secret".into(),
                ty: ColType::Encrypted { of: Box::new(ColType::String) },
                nullable: None,
                default: None,
                unique: None,
            }],
        );
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let guard_cfg = GuardConfig::confined("app".to_string());
        let (migs, frags) = author
            .lower_guarded(&ir, &guard_cfg, &BTreeSet::new())
            .expect("guarded lower of a clean createTable passes");

        // The createTable emits a multi-statement `up` (CREATE + COMMENT sentinel),
        // so MORE THAN ONE fragment is guarded for op #0.
        let op0_frags: Vec<_> = frags.iter().filter(|f| f.op_index == 0).collect();
        assert!(
            op0_frags.len() >= 2,
            "an encrypted-column createTable renders >1 fragment (CREATE + COMMENT); got {}",
            op0_frags.len()
        );
        assert!(op0_frags.iter().all(|f| f.op_kind == "createTable"));
        assert!(
            op0_frags.iter().any(|f| f.sql.contains("COMMENT ON COLUMN")),
            "the COMMENT sentinel is a SEPARATELY-guarded fragment"
        );

        // Reassembly: each migration's `up` == join of THAT migration's guarded
        // fragments with `;\n` (the invariant `lower_guarded` enforces). Verify it
        // independently here over the createTable migration.
        let create_mig = migs
            .iter()
            .find(|m| m.up.contains("CREATE TABLE"))
            .expect("a CREATE migration");
        let reassembled = split_up_fragments(&create_mig.up).join(";\n");
        assert_eq!(reassembled, create_mig.up, "reassembly must be byte-identical");
    }

    // §6.1.1 — a DENIED fragment aborts the WHOLE lower with the op-index
    // attribution, and NOTHING is applied. We force a denial by guarding the
    // rendered `"app".…` DDL under a guard CONFINED to a DIFFERENT schema, so the
    // qualified reference is a cross-schema construct the guard refuses — the same
    // refusal a hostile cross-tenant fragment would trigger.
    #[test]
    fn guard_per_fragment_denied_aborts_with_op_index_pg() {
        let ir = create_table_ir(
            "widgets",
            vec![TIrColumn {
                name: "title".into(),
                ty: ColType::String,
                nullable: None,
                default: None,
                unique: None,
            }],
        );
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        // Guard confined to "other" — the rendered `CREATE TABLE "app".…` is then a
        // cross-schema reference the Confined guard denies.
        let guard_cfg = GuardConfig::confined("other".to_string());
        let err = author
            .lower_guarded(&ir, &guard_cfg, &BTreeSet::new())
            .expect_err("a fragment outside the confined schema must be denied");
        match err {
            IrGuardedLowerError::Denied(d) => {
                assert_eq!(d.op_index, 0, "the denial attributes to op #0");
                assert_eq!(d.op_kind, "createTable");
            }
            other => panic!("expected a per-fragment Denied, got: {other}"),
        }
    }

    // §6.1.1 — the SQLite leg: the descriptor guard trusts IR-generated DDL (no
    // string deny-list), so it never denies, but the fragment split + reassembly
    // invariant still runs and holds on SQLite.
    #[test]
    fn guard_per_fragment_reassembly_holds_sqlite() {
        let ir = create_table_ir(
            "widgets",
            vec![TIrColumn {
                name: "title".into(),
                ty: ColType::String,
                nullable: Some(false),
                default: None,
                unique: None,
            }],
        );
        let author = IrAuthor::new("app", "app_a", SqlDialect::Sqlite);
        let guard_cfg = GuardConfig::confined_sqlite("app".to_string());
        let (migs, frags) = author
            .lower_guarded(&ir, &guard_cfg, &BTreeSet::new())
            .expect("SQLite guarded lower passes (descriptor guard trusts IR DDL)");
        assert!(!frags.is_empty(), "fragments are still attributed on SQLite");
        for m in &migs {
            let reassembled = split_up_fragments(&m.up).join(";\n");
            assert_eq!(reassembled, m.up, "SQLite reassembly must be byte-identical");
        }
    }

    // The loader's IR branch end-to-end (§7.2): a well-formed `.ir.json`
    // createTable by its declarer loads (fail-closed gate passes) AND lowers to a
    // CREATE TABLE migration.
    #[test]
    fn load_and_lower_create_table_end_to_end() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"createTable","name":"fresh","columns":[{"name":"title","type":"text"}]}
        ]}"#;
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let migs = author
            .load_and_lower(bytes, "app_a", &registry(&[]), &BTreeSet::new())
            .expect("a fresh createTable by its declarer loads + lowers");
        assert!(
            migs.iter().any(|m| m.up.contains("CREATE TABLE \"app\".\"fresh\"")),
            "lowering must emit the CREATE TABLE"
        );
    }

    // The fail-closed bare-name DropIndex (§8.6) is refused by the LOAD GATE the
    // loader's IR branch runs — proving the fix is wired into the real entry, not
    // only the validator unit test.
    #[test]
    fn load_and_lower_refuses_bare_name_drop_index() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"dropIndex","name":"victim_idx"}
        ]}"#;
        let author = IrAuthor::new("app", "app_intruder", SqlDialect::Postgres);
        let err = author
            .load_and_lower(
                bytes,
                "app_intruder",
                &registry(&[("victim", "app_victim")]),
                &BTreeSet::new(),
            )
            .unwrap_err();
        match err {
            LoadAndLowerError::Load(crate::ir_load::IrLoadError::Validate(ae)) => {
                assert_eq!(ae.code, crate::validate::CODE_UNSUPPORTED);
                assert_eq!(ae.kind, Some(crate::validate::UnsupportedKind::Op));
            }
            other => panic!("expected a fail-closed Load(Validate) reject, got: {other}"),
        }
    }

    // An op on ANOTHER app's table is refused by the load gate (ownership) before
    // any lowering happens.
    #[test]
    fn load_and_lower_refuses_cross_tenant_op() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"dropColumn","table":"users","column":"x"}
        ]}"#;
        let author = IrAuthor::new("app", "app_intruder", SqlDialect::Postgres);
        let err = author
            .load_and_lower(
                bytes,
                "app_intruder",
                &registry(&[("users", "app_owner")]),
                &BTreeSet::new(),
            )
            .unwrap_err();
        assert!(
            matches!(
                err,
                LoadAndLowerError::Load(crate::ir_load::IrLoadError::NotTableOwner { .. })
            ),
            "got: {err}"
        );
    }
}
