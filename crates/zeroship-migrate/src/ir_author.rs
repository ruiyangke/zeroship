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
            match op {
                Op::CreateTable { name, columns, .. } => {
                    let desc = self.create_table_descriptor(name, columns);
                    let snap = build_table_snapshot(&self.project_schema, &desc, self.dialect)?;
                    out.extend(self.decl.lower_create_table(name, &snap, &live)?);
                    // The just-created table is now live for any later intra-IR FK.
                    live.insert(name.clone());
                }
                Op::AddColumn { table, column, ty, nullable, default } => {
                    let col =
                        self.add_column_snapshot(table, column, ty, *nullable, default.as_ref())?;
                    out.push(self.decl.lower_add_column(table, &col));
                }
                Op::CreateIndex { table, columns, name, unique, using, .. } => {
                    let idx =
                        create_index_snapshot(table, columns, name.as_deref(), *unique, *using);
                    out.push(self.decl.lower_create_index(table, &idx));
                }
                Op::DropTable { table, .. } => {
                    out.push(self.decl.lower_drop_table(table));
                }
                Op::DropColumn { table, column, .. } => {
                    out.push(self.decl.lower_drop_column(table, column));
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
                    out.push(self.decl.lower_drop_index(&idx));
                }
                Op::AlterColumnType { .. } => {
                    return Err(IrLowerError::UnsupportedOp("alterColumnType"))
                }
                Op::AlterColumnNullability { .. } => {
                    return Err(IrLowerError::UnsupportedOp("alterColumnNullability"))
                }
                Op::RenameColumn { .. } => {
                    return Err(IrLowerError::UnsupportedOp("renameColumn"))
                }
                Op::AddConstraint { .. } => {
                    return Err(IrLowerError::UnsupportedOp("addConstraint"))
                }
                Op::DropConstraint { .. } => {
                    return Err(IrLowerError::UnsupportedOp("dropConstraint"))
                }
                Op::Insert { .. } => return Err(IrLowerError::UnsupportedOp("insert")),
                Op::Update { .. } => return Err(IrLowerError::UnsupportedOp("update")),
                Op::Delete { .. } => return Err(IrLowerError::UnsupportedOp("delete")),
                Op::Backfill { .. } => return Err(IrLowerError::UnsupportedOp("backfill")),
            }
        }
        Ok(out)
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
