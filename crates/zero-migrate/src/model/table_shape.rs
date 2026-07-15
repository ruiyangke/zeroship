//! Resolve policy-managed table shape into explicit `createTable` IR.
//!
//! # Injection-as-rule (§II.4)
//!
//! System columns, indexes, and the pinned primary key are no longer read from a
//! monolithic `PolicyProfile.system_shape`. They are driven by the composed,
//! unforgeable [`EffectivePolicy`]: for each `createTable` op we build the
//! [`ObjectName`] the op names and ask `effective.injects_for(&object)` for the
//! covering [`InjectSpec`]s (in the sealed cross-layer inject total order). Each
//! spec contributes its columns (prepended, in order), indexes (appended), and — if
//! it pins one — the table's primary key. The policy CONTENT (which columns, which
//! type token) lives in the policy crate; this module only MAPS the opaque type
//! tokens to [`ColType`] and lays the resolved shape into the IR.
//!
//! The semantics are byte-for-byte those of the retired `system_shape` path: system
//! columns prepend, [`SystemColumnCollision`](TableShapeError::SystemColumnCollision)
//! on an author collision (except the `id`-folding cases), an
//! [`AuthorPrimaryKeyForbidden`](TableShapeError::AuthorPrimaryKeyForbidden) when a
//! covering spec pins the PK and forbids author PKs, injected indexes appended, and
//! an idempotent re-run over an already-resolved table.

use zero_migrate_policy::{AuthorPkPolicy, EffectivePolicy, InjectColumn, InjectIndex, ObjectName};

use crate::model::expr::{Expr, SynthFn};
use crate::model::ir::{ColType, IndexElement, IrColumn, IrDefault, IrIndex, MigrationIr, Op};
use zero_migrate_ir::policy_registry;

/// Error raised while applying the effective policy's table injection.
#[derive(Debug, thiserror::Error)]
pub enum TableShapeError {
    /// A policy-injected system column collided with an author-declared column.
    #[error(
        "createTable {table:?} declares column {column:?}, which collides with an injected system column"
    )]
    SystemColumnCollision {
        /// Table being resolved.
        table: String,
        /// Colliding column name.
        column: String,
    },
    /// The policy injects a system column type this engine cannot express in IR.
    #[error("system column {column:?} uses unsupported type {data_type:?}")]
    UnsupportedSystemColumnType {
        /// Column name.
        column: String,
        /// Inject type token.
        data_type: String,
    },
    /// A policy-pinned primary key would silently discard an author PK.
    #[error(
        "createTable {table:?} declares an author primaryKey under a policy-pinned table shape"
    )]
    AuthorPrimaryKeyForbidden {
        /// Table being resolved.
        table: String,
    },
    /// The `id: t.id({ prefix })` fold found a malformed prefix.
    #[error("createTable {table:?} declares invalid id prefix {prefix:?}: {message}")]
    InvalidIdPrefix {
        /// Table being resolved.
        table: String,
        /// Invalid prefix.
        prefix: String,
        /// Validator message.
        message: String,
    },
    /// The `id` prefix declaration carried a facet the fold would lose.
    #[error(
        "createTable {table:?} declares id as a system-prefix field with unsupported modifiers"
    )]
    InvalidIdPrefixDeclaration {
        /// Table being resolved.
        table: String,
    },
}

/// The resolved injection shape covering ONE object: the union of every covering
/// [`InjectSpec`]'s columns/indexes plus the first pinned primary key. This is the
/// per-object content the resolver lays into the IR — the flattening of
/// `injects_for(object)` into a single ordered shape.
///
/// Column order is the sealed inject total order (outermost inject first, each
/// spec's columns in document order); indexes likewise. The primary key is the
/// FIRST covering spec that pins one (the outermost ceiling wins — a draft cannot
/// override a ceiling PK, which `admit`'s collision blame already
/// guarantees is non-conflicting). `author_primary_key` is `Forbid` if ANY covering
/// spec forbids (obligations union up).
struct ResolvedInject {
    columns: Vec<InjectColumn>,
    indexes: Vec<InjectIndex>,
    primary_key: Option<Vec<String>>,
    author_primary_key: AuthorPkPolicy,
}

impl ResolvedInject {
    /// Flatten the covering inject specs at `object` into a single ordered shape.
    fn for_object(effective: &EffectivePolicy, object: &ObjectName) -> Self {
        let mut columns: Vec<InjectColumn> = Vec::new();
        let mut indexes: Vec<InjectIndex> = Vec::new();
        let mut primary_key: Option<Vec<String>> = None;
        let mut author_primary_key = AuthorPkPolicy::Allow;
        for spec in effective.injects_for(object) {
            columns.extend(spec.columns.iter().cloned());
            indexes.extend(spec.indexes.iter().cloned());
            if primary_key.is_none() {
                primary_key.clone_from(&spec.primary_key);
            }
            if matches!(spec.author_primary_key, AuthorPkPolicy::Forbid) {
                author_primary_key = AuthorPkPolicy::Forbid;
            }
        }
        Self { columns, indexes, primary_key, author_primary_key }
    }

    /// This object carries no injection — the resolver is a no-op for it.
    fn is_empty(&self) -> bool {
        self.columns.is_empty() && self.primary_key.is_none()
    }
}

/// The [`ObjectName`] a `createTable` op addresses: `schema.table` when the op
/// carries a schema qualifier, else the bare table (the confined project-schema
/// case, where the ⊤-scope confined inject covers it regardless). Names are used
/// verbatim; the policy's scope matcher folds them (II.2.7).
fn object_for_create(name: &str, schema: Option<&str>) -> ObjectName {
    match schema {
        Some(s) => ObjectName::table(s.as_bytes().to_vec(), name.as_bytes().to_vec()),
        None => ObjectName::table(b"public".to_vec(), name.as_bytes().to_vec()),
    }
}

/// Apply the effective policy's table injection to every `createTable` op.
///
/// The returned IR is the self-contained artifact shape: policy-injected system
/// columns are prepended, injected indexes are appended, and the pinned
/// `primaryKey` is present before canonical bytes/checksum are computed. The
/// checksum (folded downstream over this RESOLVED IR) therefore depends on the
/// injected SHAPE, not on how the policy was authored: two policies that inject the
/// same columns/indexes/PK resolve to byte-identical IR ⇒ the same checksum (G6).
///
/// # Errors
/// A [`TableShapeError`] on an author/system column collision, an author PK under a
/// pinned-PK-forbid inject, an unmappable inject type token, or a malformed `id`
/// prefix declaration.
pub fn resolve_create_table_policy(
    ir: &MigrationIr,
    effective: &EffectivePolicy,
) -> Result<MigrationIr, TableShapeError> {
    let mut out = ir.clone();
    for op in &mut out.ops {
        let Op::CreateTable {
            name,
            columns,
            primary_key,
            indexes,
            schema,
            ..
        } = op
        else {
            continue;
        };
        let object = object_for_create(name, schema.as_deref());
        let resolved = ResolvedInject::for_object(effective, &object);
        resolve_create_table(name, columns, primary_key, indexes, &resolved)?;
    }
    Ok(out)
}

fn resolve_create_table(
    table: &str,
    columns: &mut Vec<IrColumn>,
    primary_key: &mut Option<Vec<String>>,
    indexes: &mut Vec<IrIndex>,
    inject: &ResolvedInject,
) -> Result<(), TableShapeError> {
    // No columns to inject AND no pinned PK ⇒ the policy manages nothing about this
    // table's shape (the platform/author-owned case). Leave it verbatim.
    if inject.is_empty() {
        return Ok(());
    }

    if resolved_create_table_matches_inject(columns, primary_key, indexes, inject)? {
        return Ok(());
    }

    let mut folded_id_prefix: Option<String> = None;
    let mut folded_id = false;
    let mut resolved_columns = Vec::with_capacity(inject.columns.len() + columns.len());
    for system in &inject.columns {
        let collision = columns.iter().find(|c| c.name == system.name);
        if let Some(author_col) = collision {
            if is_id_prefix_declaration(author_col) {
                validate_folded_id_prefix(table, author_col)?;
                folded_id_prefix = author_col.id_prefix.clone();
                folded_id = true;
            } else if is_id_identity_replacement(author_col) {
                validate_folded_id_identity(table, author_col)?;
                folded_id = true;
            } else {
                return Err(TableShapeError::SystemColumnCollision {
                    table: table.to_string(),
                    column: system.name.clone(),
                });
            }
        }
        let mut col = inject_column_to_ir(system)?;
        if system.name == "id" {
            if let Some(author_col) = collision.filter(|c| is_id_identity_replacement(c)) {
                col = author_col.clone();
            } else {
                col.id_prefix = folded_id_prefix.clone();
            }
        }
        resolved_columns.push(col);
    }

    if inject.primary_key.is_some() {
        let author_pk_is_folded_id =
            folded_id && primary_key.as_deref().is_some_and(|pk| pk == ["id"]);
        if primary_key.is_some()
            && !author_pk_is_folded_id
            && matches!(inject.author_primary_key, AuthorPkPolicy::Forbid)
        {
            return Err(TableShapeError::AuthorPrimaryKeyForbidden {
                table: table.to_string(),
            });
        }
    }

    resolved_columns.extend(
        columns
            .iter()
            .filter(|c| !(folded_id && c.name == "id"))
            .cloned(),
    );
    *columns = resolved_columns;

    if let Some(pk) = &inject.primary_key {
        *primary_key = Some(pk.clone());
    }

    indexes.extend(inject.indexes.iter().map(|idx| IrIndex {
        name: None,
        columns: idx
            .columns
            .iter()
            .map(|name| IndexElement::Column {
                name: name.clone(),
                order: None,
                opclass: None,
                collation: None,
            })
            .collect(),
        unique: None,
        using: None,
        r#where: None,
        include: Vec::new(),
        with: None,
        only: None,
        nulls_not_distinct: None,
    }));

    Ok(())
}

/// Map an [`InjectColumn`]'s opaque type token to a native [`IrColumn`]. The token
/// spellings the engine understands mirror the retired `system_shape` mapping
/// (`text`, `timestamptz`/`timestamp with time zone`, `integer`/`int`).
fn inject_column_to_ir(column: &InjectColumn) -> Result<IrColumn, TableShapeError> {
    let ty = match column.ty.as_str() {
        "text" => ColType::Text,
        "timestamptz" | "timestamp with time zone" => ColType::Timestamp,
        "integer" | "int" => ColType::Int,
        other => {
            return Err(TableShapeError::UnsupportedSystemColumnType {
                column: column.name.clone(),
                data_type: other.to_string(),
            })
        }
    };
    Ok(IrColumn {
        name: column.name.clone(),
        ty,
        nullable: Some(column.nullable),
        default: None,
        unique: None,
        id_prefix: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
    })
}

fn is_id_prefix_declaration(column: &IrColumn) -> bool {
    column.name == "id" && matches!(column.ty, ColType::Uuid)
}

fn is_id_identity_replacement(column: &IrColumn) -> bool {
    column.name == "id"
        && column.identity.is_some()
        && matches!(
            column.ty,
            ColType::SmallInt | ColType::Int | ColType::BigInt
        )
}

fn validate_folded_id_prefix(table: &str, column: &IrColumn) -> Result<(), TableShapeError> {
    let has_unsupported_default = match column.default.as_ref() {
        None => false,
        Some(default) if is_gen_random_uuid_default(default) => false,
        Some(_) => true,
    };
    if column.unique.unwrap_or(false)
        || has_unsupported_default
        || column.mask.is_some()
        || column.generated.is_some()
        || column.identity.is_some()
        || column.case_sensitive.is_some()
        || column.vector_metric.is_some()
    {
        return Err(TableShapeError::InvalidIdPrefixDeclaration {
            table: table.to_string(),
        });
    }
    if let Some(prefix) = &column.id_prefix {
        crate::schema::query::validate_id_prefix(prefix).map_err(|e| {
            TableShapeError::InvalidIdPrefix {
                table: table.to_string(),
                prefix: prefix.clone(),
                message: e.to_string(),
            }
        })?;
    }
    Ok(())
}

#[cfg(test)]
fn gen_random_uuid_default() -> IrDefault {
    IrDefault::Expr {
        expr: Expr::FnSynth {
            r#fn: SynthFn::GenRandomUuid,
            args: Vec::new(),
        },
    }
}

fn is_gen_random_uuid_default(default: &IrDefault) -> bool {
    matches!(
        default,
        IrDefault::Expr {
            expr: Expr::FnSynth {
                r#fn: SynthFn::GenRandomUuid,
                args,
            },
        } if args.is_empty()
    )
}

fn validate_folded_id_identity(table: &str, column: &IrColumn) -> Result<(), TableShapeError> {
    let has_default = column.default.is_some();
    if column.unique.unwrap_or(false)
        || has_default
        || column.mask.is_some()
        || column.generated.is_some()
        || column.case_sensitive.is_some()
        || column.vector_metric.is_some()
        || column.id_prefix.is_some()
    {
        return Err(TableShapeError::InvalidIdPrefixDeclaration {
            table: table.to_string(),
        });
    }
    Ok(())
}

/// Is this resolved `createTable` ALREADY the injected shape (an idempotent re-run)?
///
/// The old `resolved_create_table_matches_profile` re-derived the expected columns
/// from the profile; this peer re-derives them from the covering
/// [`InjectSpec`]s (`inject`). Beyond the leading-prefix name/shape match it adds
/// the II.2.6b conformance check: a resolved column occupying an injected slot must
/// match the [`InjectColumn`]'s type + nullability + default (a rename-into or a
/// hand-forged column that merely borrows an injected NAME but diverges in shape is
/// NOT the injected shape, so it is re-injected / re-checked, never silently
/// accepted).
fn resolved_create_table_matches_inject(
    columns: &[IrColumn],
    primary_key: &Option<Vec<String>>,
    indexes: &[IrIndex],
    inject: &ResolvedInject,
) -> Result<bool, TableShapeError> {
    if inject.columns.is_empty() {
        return Ok(false);
    }
    if columns.len() < inject.columns.len() || indexes.len() < inject.indexes.len() {
        return Ok(false);
    }
    for (actual, expected) in columns.iter().zip(&inject.columns) {
        // The `id` identity-replacement fold leaves an author identity column in the
        // `id` slot; it is a conforming resolution of the injected `id`.
        if expected.name == "id" && is_id_identity_replacement(actual) {
            continue;
        }
        let expected_ir = inject_column_to_ir(expected)?;
        if !system_columns_match(actual, &expected_ir) {
            return Ok(false);
        }
    }
    if let Some(pk) = &inject.primary_key {
        if primary_key.as_ref() != Some(pk) {
            return Ok(false);
        }
    }
    let start = indexes.len() - inject.indexes.len();
    for (actual, expected) in indexes[start..].iter().zip(&inject.indexes) {
        let actual_cols = actual
            .columns
            .iter()
            .map(|c| match c {
                IndexElement::Column { name, .. } => Some(name.as_str()),
                IndexElement::Expr { .. } => None,
            })
            .collect::<Option<Vec<_>>>();
        let Some(actual_cols) = actual_cols else {
            return Ok(false);
        };
        let expected_cols = expected
            .columns
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        if actual_cols != expected_cols
            || actual.unique.unwrap_or(false)
            || actual.using.is_some()
            || actual.r#where.is_some()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn system_columns_match(actual: &IrColumn, expected: &IrColumn) -> bool {
    // II.2.6b conformance: an occupant of an injected column slot must match the
    // inject's name AND its type/nullability/default (the shape the InjectSpec
    // carries — `inject_column_to_ir` maps the opaque token; injected columns carry
    // no default today, so `default` matches on `None`).
    actual.name == expected.name
        && actual.ty == expected.ty
        && actual.nullable == expected.nullable
        && actual.default == expected.default
        && actual.unique == expected.unique
        && actual.case_sensitive == expected.case_sensitive
        && actual.vector_metric == expected.vector_metric
        && actual.mask == expected.mask
        && actual.generated == expected.generated
        && actual.identity == expected.identity
}

// ══════════════════════════════════════════════════════════════════════════════
// Test-support: the GENERIC engine test ceiling
// ══════════════════════════════════════════════════════════════════════════════

/// The engine's GENERIC test ceiling, as an in-repo `RootCeiling` TOML string. It
/// reproduces TODAY's confined system shape as a SINGLE ⊤-scope `inject` rule: the
/// seven system columns (`id`/`created_at`/`updated_at`/`created_by`/`updated_by`/
/// `version`/`deleted_at`), the three system indexes (`deleted_at`/`updated_at`/
/// `created_by`), `primary_key = ["id"]`, and `author_primary_key = "forbid"`.
///
/// This is deliberately the engine's own test scaffolding — NOT a shipped preset.
/// Zeroship's REAL confined ceiling moves to the monorepo in Phase 3; the engine
/// constructs no default ceiling of its own.
#[cfg(any(test, feature = "test-support"))]
pub const ZEROSHIP_CONFINED_CEILING_TOML: &str = r#"policy_version = 1

[[grant]]
key = "core.cross_schema"
value = true
scope = { include = ["app"] }

[[grant]]
key = "core.create_table"
value = true
scope = { include = ["app"] }

[[grant]]
key = "core.rename_into"
value = true
scope = { include = ["app"] }

[[grant]]
key = "sec.destructive_ops"
value = "allow"
scope = "all"

[[inject]]
scope = "all"
mandatory = true
primary_key = ["id"]
author_primary_key = "forbid"
columns = [
  { name = "id",         type = "text",        nullable = false },
  { name = "created_at", type = "timestamptz", nullable = false },
  { name = "updated_at", type = "timestamptz", nullable = false },
  { name = "created_by", type = "text",        nullable = true  },
  { name = "updated_by", type = "text",        nullable = true  },
  { name = "version",    type = "integer",     nullable = false },
  { name = "deleted_at", type = "timestamptz", nullable = true  },
]
indexes = [
  { name = "ix_deleted_at", columns = ["deleted_at"] },
  { name = "ix_updated_at", columns = ["updated_at"] },
  { name = "ix_created_by", columns = ["created_by"] },
]
"#;

/// Build an [`EffectivePolicy`] from a `RootCeiling` document (TOML). The ceiling
/// is parsed against the engine's builtin registry, then composes against a
/// grant-only draft extracted from the same ceiling. Inject/require/validate
/// rules survive from the root ceiling; grants become effective through the draft
/// side of `admit` after proving they do not exceed the root ceiling.
/// Inject-only ceilings still compose because the extracted draft is empty.
///
/// This is the engine-side constructor the production authoring verb
/// (`lower_envelope_to_migrations`) and the test ceiling both go through — the
/// engine never fabricates an `EffectivePolicy` by hand.
///
/// # Errors
/// A human-readable message on: a malformed ceiling document, a malformed empty
/// draft (unreachable), or a composition failure.
pub fn effective_policy_from_ceiling_toml(ceiling_toml: &str) -> Result<EffectivePolicy, String> {
    let registry = policy_registry::builtin_registry();
    let ceiling = zero_migrate_policy::RootCeiling::parse_toml(ceiling_toml, &registry)
        .map_err(|e| format!("policy ceiling failed to load: {e:?}"))?;
    let draft_toml = grant_only_draft_toml(ceiling_toml)?;
    let draft = zero_migrate_policy::PolicyDoc::parse_toml(
        &draft_toml,
        &registry,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .map_err(|e| format!("empty policy draft failed to load: {e:?}"))?;
    zero_migrate_policy::admit(&ceiling, &draft, &registry)
        .map_err(|e| format!("policy composition failed: {e:?}"))
}

fn grant_only_draft_toml(ceiling_toml: &str) -> Result<String, String> {
    let parsed: toml::Value = toml::from_str(ceiling_toml)
        .map_err(|e| format!("policy ceiling failed to parse as TOML: {e}"))?;
    let Some(table) = parsed.as_table() else {
        return Err("policy ceiling root must be a TOML table".to_string());
    };

    let mut draft = toml::map::Map::new();
    let Some(version) = table.get("policy_version").cloned() else {
        return Err("policy ceiling is missing policy_version".to_string());
    };
    draft.insert("policy_version".to_string(), version);
    if let Some(default_scope) = table.get("default_scope").cloned() {
        draft.insert("default_scope".to_string(), default_scope);
    }
    if let Some(grants) = table.get("grant").cloned() {
        draft.insert("grant".to_string(), grants);
    }
    toml::to_string(&toml::Value::Table(draft))
        .map_err(|e| format!("grant-only policy draft failed to serialize: {e}"))
}

/// The GENERIC engine test ceiling as a composed [`EffectivePolicy`]: the shared
/// helper every in-crate and integration test routes its
/// `resolve_create_table_policy` setup through. Reproduces the confined system
/// shape via [`ZEROSHIP_CONFINED_CEILING_TOML`].
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn zeroship_confined_ceiling() -> EffectivePolicy {
    effective_policy_from_ceiling_toml(ZEROSHIP_CONFINED_CEILING_TOML)
        .expect("embedded generic test ceiling composes")
}

/// A NO-INJECT effective policy: injects nothing, so `resolve_create_table_policy`
/// is a no-op and the author-owned table shape passes through verbatim. This is the
/// test peer of the retired `PolicyProfile::platform()` (author-owned) setup — the
/// engine-external tests route their platform/author-owned injection setup through
/// this instead of naming the policy crate.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn zeroship_no_inject_ceiling() -> EffectivePolicy {
    confined_no_inject_policy("app").expect("embedded no-inject confined policy composes")
}

/// A no-inject confined policy for the named project schema. This is the
/// author-owned path: table-shape resolution is a no-op, while the guard still
/// receives the scoped namespace grants it needs for create/rename attribution.
pub fn confined_no_inject_policy(project_schema: &str) -> Result<EffectivePolicy, String> {
    let schemas = vec![project_schema.to_string()];
    let scope = schema_scope_value(&schemas);
    let grant_rules = vec![
        // Cross-schema confinement: a confined migration may reference only its own
        // project schema. This is the guard's `grants(core.cross_schema, s)` source.
        grant_rule(
            policy_registry::KEY_CORE_CROSS_SCHEMA,
            toml::Value::Boolean(true),
            scope.clone(),
        ),
        grant_rule(
            policy_registry::KEY_CORE_CREATE_TABLE,
            toml::Value::Boolean(true),
            scope.clone(),
        ),
        grant_rule(
            policy_registry::KEY_CORE_RENAME_INTO,
            toml::Value::Boolean(true),
            scope,
        ),
        grant_rule(
            policy_registry::KEY_SEC_DESTRUCTIVE_OPS,
            toml::Value::String("allow".to_string()),
            toml::Value::String("all".to_string()),
        ),
    ];

    effective_policy_from_grant_rules(grant_rules)
}

/// A no-inject operator policy for platform-owned flows. It grants the builtin
/// vendor capability set, scoped creation/rename grants for `schemas` (or `all`
/// when the list is empty), and the extension name allowlist.
pub fn operator_no_inject_policy(
    schemas: &[String],
    extensions: &[String],
) -> Result<EffectivePolicy, String> {
    operator_policy_inner(schemas, extensions, false)
}

/// The **trusted** (dbmate-like) no-inject policy: the operator capability set plus
/// the `core.skip_static_guard` belt-skip and ⊤ cross-schema (no confinement). The
/// belt-skip is the policy grant the guard's [`skips_denylist_belt`] reads.
///
/// [`skips_denylist_belt`]: crate::guard::GuardConfig::skips_denylist_belt
pub fn trusted_no_inject_policy() -> Result<EffectivePolicy, String> {
    operator_policy_inner(&[], &[], true)
}

fn operator_policy_inner(
    schemas: &[String],
    extensions: &[String],
    skip_static_guard: bool,
) -> Result<EffectivePolicy, String> {
    let mut grant_rules = Vec::new();
    let all = toml::Value::String("all".to_string());
    // Whole-DB vendor capabilities (Global ⊤).
    for key in [
        policy_registry::KEY_PG_ROLE,
        policy_registry::KEY_PG_GRANT,
        policy_registry::KEY_PG_EXTENSION,
        policy_registry::KEY_PG_SCHEMA,
        policy_registry::KEY_CORE_CREATE_SCHEMA,
        policy_registry::KEY_PG_POLICY,
        policy_registry::KEY_PG_RLS,
        policy_registry::KEY_PG_PARTITION,
        policy_registry::KEY_PG_FUNCTION,
        policy_registry::KEY_CORE_RAW_SQL,
        policy_registry::KEY_CORE_RAW_VIEW_BODY,
        policy_registry::KEY_PG_MATERIALIZED_VIEW,
    ] {
        grant_rules.push(grant_rule(key, toml::Value::Boolean(true), all.clone()));
    }
    // Cross-schema + creation/rename, scoped to the owned schema allowlist (⊤ when
    // empty — the trusted posture, which skips the belt anyway).
    let creation_scope = schema_scope_value(schemas);
    for key in [
        policy_registry::KEY_CORE_CROSS_SCHEMA,
        policy_registry::KEY_CORE_CREATE_TABLE,
        policy_registry::KEY_CORE_RENAME_INTO,
    ] {
        grant_rules.push(grant_rule(key, toml::Value::Boolean(true), creation_scope.clone()));
    }
    // Platform posture relaxes the raw-island role/search_path needles; trusted does
    // NOT (its raw-island backstop still denies them). Trusted instead skips the
    // whole static belt via `core.skip_static_guard`.
    if skip_static_guard {
        grant_rules.push(grant_rule(
            policy_registry::KEY_CORE_SKIP_STATIC_GUARD,
            toml::Value::Boolean(true),
            all.clone(),
        ));
    } else {
        grant_rules.push(grant_rule(
            policy_registry::KEY_CORE_RAW_ISLAND_ROLE,
            toml::Value::Boolean(true),
            all.clone(),
        ));
    }
    if !extensions.is_empty() {
        grant_rules.push(grant_rule(
            policy_registry::KEY_PG_EXTENSIONS,
            toml::Value::Array(
                extensions
                    .iter()
                    .map(|s| toml::Value::String(s.clone()))
                    .collect(),
            ),
            all.clone(),
        ));
    }
    grant_rules.push(grant_rule(
        policy_registry::KEY_SEC_DESTRUCTIVE_OPS,
        toml::Value::String("allow".to_string()),
        all,
    ));

    effective_policy_from_grant_rules(grant_rules)
}

fn schema_scope_value(schemas: &[String]) -> toml::Value {
    if schemas.is_empty() {
        toml::Value::String("all".to_string())
    } else {
        let mut scope = toml::map::Map::new();
        scope.insert(
            "include".to_string(),
            toml::Value::Array(
                schemas
                    .iter()
                    .map(|s| toml::Value::String(s.clone()))
                    .collect(),
            ),
        );
        toml::Value::Table(scope)
    }
}

fn grant_rule(key: &str, value: toml::Value, scope: toml::Value) -> toml::Value {
    let mut grant = toml::map::Map::new();
    grant.insert("key".to_string(), toml::Value::String(key.to_string()));
    grant.insert("value".to_string(), value);
    grant.insert("scope".to_string(), scope);
    toml::Value::Table(grant)
}

fn effective_policy_from_grant_rules(
    grant_rules: Vec<toml::Value>,
) -> Result<EffectivePolicy, String> {
    let mut doc = toml::map::Map::new();
    doc.insert(
        "policy_version".to_string(),
        toml::Value::Integer(i64::from(zero_migrate_policy::SUPPORTED_POLICY_VERSION)),
    );
    doc.insert("grant".to_string(), toml::Value::Array(grant_rules));
    let toml = toml::to_string(&toml::Value::Table(doc))
        .map_err(|e| format!("no-inject confined policy failed to serialize: {e}"))?;
    effective_policy_from_ceiling_toml(&toml)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ir::{CanonicalOpList, MigrationIr, CURRENT_IR_VERSION};
    use crate::{Checksum, MigrationFlags};

    fn ir(columns: Vec<IrColumn>, primary_key: Option<Vec<String>>) -> MigrationIr {
        MigrationIr {
            ir_version: CURRENT_IR_VERSION,
            name: "m".into(),
            owner_app: "app".into(),
            ops: vec![Op::CreateTable {
                name: "widgets".into(),
                columns,
                primary_key,
                constraints: vec![],
                indexes: vec![],
                partition_by: None,
                runtime_options: None,
                schema: None,
                existence_guard: None,
            }],
            checksum: None,
            flags: Default::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
        }
    }

    fn text_col(name: &str) -> IrColumn {
        IrColumn {
            name: name.into(),
            ty: ColType::Text,
            nullable: None,
            default: None,
            unique: None,
            id_prefix: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        }
    }

    /// An alternate ceiling with the SAME injected shape as the confined ceiling but
    /// authored differently (indexes named differently — index names are not part of
    /// the injected IR shape; the resolver appends unnamed IR indexes over the
    /// injected columns). Used to prove checksum-invariance under equivalent-shape
    /// policies (G6).
    fn equivalent_shape_ceiling() -> EffectivePolicy {
        let toml = r#"policy_version = 1

[[inject]]
scope = "all"
mandatory = true
primary_key = ["id"]
author_primary_key = "forbid"
columns = [
  { name = "id",         type = "text",        nullable = false },
  { name = "created_at", type = "timestamptz", nullable = false },
  { name = "updated_at", type = "timestamptz", nullable = false },
  { name = "created_by", type = "text",        nullable = true  },
  { name = "updated_by", type = "text",        nullable = true  },
  { name = "version",    type = "integer",     nullable = false },
  { name = "deleted_at", type = "timestamptz", nullable = true  },
]
indexes = [
  { name = "renamed_deleted_at_idx", columns = ["deleted_at"] },
  { name = "renamed_updated_at_idx", columns = ["updated_at"] },
  { name = "renamed_created_by_idx", columns = ["created_by"] },
]
"#;
        effective_policy_from_ceiling_toml(toml).expect("equivalent-shape ceiling composes")
    }

    #[test]
    fn confined_prepends_system_shape_and_pk() {
        let resolved =
            resolve_create_table_policy(&ir(vec![text_col("title")], None), &zeroship_confined_ceiling())
                .expect("resolve");
        let Op::CreateTable {
            columns,
            primary_key,
            indexes,
            ..
        } = &resolved.ops[0]
        else {
            panic!("create op")
        };
        let names = columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>();
        assert_eq!(
            &names[..7],
            [
                "id",
                "created_at",
                "updated_at",
                "created_by",
                "updated_by",
                "version",
                "deleted_at"
            ]
        );
        assert_eq!(names[7], "title");
        assert_eq!(primary_key.as_deref(), Some(&["id".to_string()][..]));
        let index_cols = indexes
            .iter()
            .map(|idx| {
                idx.columns
                    .iter()
                    .map(|c| match c {
                        IndexElement::Column { name, .. } => name.as_str(),
                        IndexElement::Expr { .. } => "<expr>",
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            index_cols,
            vec![vec!["deleted_at"], vec!["updated_at"], vec!["created_by"]]
        );
    }

    #[test]
    fn platform_preserves_author_shape() {
        // A ceiling that injects nothing (deny_all — no inject rules) is a no-op: the
        // author-owned table shape passes through verbatim.
        let input = ir(
            vec![text_col("id"), text_col("team")],
            Some(vec!["id".into()]),
        );
        let registry = zero_migrate_policy::PolicyRegistry::empty();
        let no_inject = EffectivePolicy::deny_all(&registry);
        let resolved =
            resolve_create_table_policy(&input, &no_inject).expect("no-inject ceiling is a no-op");
        assert_eq!(resolved, input);
    }

    #[test]
    fn system_column_collision_is_rejected_except_id_prefix() {
        let err = resolve_create_table_policy(
            &ir(vec![text_col("created_at")], None),
            &zeroship_confined_ceiling(),
        )
        .expect_err("created_at collision");
        assert!(matches!(
            err,
            TableShapeError::SystemColumnCollision { column, .. } if column == "created_at"
        ));

        let mut id = text_col("id");
        id.ty = ColType::Uuid;
        id.nullable = Some(false);
        id.default = Some(gen_random_uuid_default());
        id.id_prefix = Some("post".into());
        let resolved = resolve_create_table_policy(
            &ir(vec![id], Some(vec!["id".into()])),
            &zeroship_confined_ceiling(),
        )
        .expect("id prefix folds");
        let Op::CreateTable { columns, .. } = &resolved.ops[0] else {
            panic!("create op")
        };
        assert_eq!(columns.iter().filter(|c| c.name == "id").count(), 1);
        assert_eq!(columns[0].id_prefix.as_deref(), Some("post"));
    }

    #[test]
    fn author_primary_key_under_pinned_pk_is_forbidden() {
        // A non-`id` author PK under the confined (PK-pinning, author-PK-forbid)
        // ceiling is rejected.
        let mut extra = text_col("code");
        extra.nullable = Some(false);
        let err = resolve_create_table_policy(
            &ir(vec![extra], Some(vec!["code".into()])),
            &zeroship_confined_ceiling(),
        )
        .expect_err("author PK forbidden under pinned PK");
        assert!(matches!(err, TableShapeError::AuthorPrimaryKeyForbidden { .. }));
    }

    fn checksum_of(ir: &MigrationIr) -> Checksum {
        Checksum::of_ir(
            &CanonicalOpList(&ir.ops),
            &MigrationFlags::default(),
            &ir.owner_app,
            &[],
            &[],
            &ir.preconditions,
        )
    }

    /// G6 (checksum honesty): the checksum is over the RESOLVED IR (injected columns
    /// are part of the applied DDL), and it depends on the RESOLVED SHAPE, not on how
    /// the policy was authored. Two ceilings that inject the SAME columns/indexes/PK
    /// resolve to byte-identical IR ⇒ the SAME checksum. And the checksum IS sensitive
    /// to the actual injected columns (a no-inject ceiling differs).
    #[test]
    fn checksum_is_invariant_under_equivalent_shape_and_sensitive_to_injection() {
        let input = ir(vec![text_col("title")], None);

        let confined =
            resolve_create_table_policy(&input, &zeroship_confined_ceiling()).expect("confined");
        let equivalent =
            resolve_create_table_policy(&input, &equivalent_shape_ceiling()).expect("equivalent");
        // Equivalent injected SHAPE ⇒ byte-identical resolved IR ⇒ identical checksum.
        assert_eq!(confined.ops, equivalent.ops);
        assert_eq!(checksum_of(&confined), checksum_of(&equivalent));

        // Sensitivity: a ceiling that injects nothing resolves to a DIFFERENT shape
        // (no system columns) ⇒ a different checksum.
        let registry = zero_migrate_policy::PolicyRegistry::empty();
        let no_inject =
            resolve_create_table_policy(&input, &EffectivePolicy::deny_all(&registry))
                .expect("no-inject");
        assert_ne!(confined.ops, no_inject.ops);
        assert_ne!(checksum_of(&confined), checksum_of(&no_inject));
    }

    /// The resolver is idempotent: re-running it over an already-resolved table is a
    /// no-op (the II.2.6b conformance check accepts the injected shape).
    #[test]
    fn resolve_is_idempotent() {
        let input = ir(vec![text_col("title")], None);
        let once = resolve_create_table_policy(&input, &zeroship_confined_ceiling()).expect("once");
        let twice =
            resolve_create_table_policy(&once, &zeroship_confined_ceiling()).expect("twice");
        assert_eq!(once, twice);
    }
}
