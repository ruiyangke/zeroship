//! Resolve policy-managed table shape into explicit `createTable` IR.
//!
//! # Injection-as-rule (§II.4)
//!
//! System columns, indexes, and the pinned primary key are no longer read from a
//! monolithic `PolicyProfile.system_shape`. They are driven by the composed,
//! unforgeable [`EffectivePolicy`]: for each `createTable` op we build the
//! [`ObjectName`] the op names and ask `effective.injects_for(&object)` for the
//! covering [`zero_migrate_policy::InjectSpec`]s (in the sealed cross-layer inject total order). Each
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

use zero_migrate_policy::{
    normalize_pg_identifier, AuthorPkPolicy, EffectivePolicy, InjectColumn, InjectIndex, ObjectName,
};

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
    /// The policy injects a default token this engine cannot map into the closed IR.
    #[error("system column {column:?} uses unsupported default {default:?}")]
    UnsupportedSystemColumnDefault {
        /// Column name.
        column: String,
        /// Opaque default token from the inject rule.
        default: String,
    },
    /// A policy injection identifier is not a portable single SQL identifier.
    #[error("injected {kind} identifier {name:?} is invalid")]
    InvalidInjectIdentifier {
        /// Identifier role in the injection rule.
        kind: &'static str,
        /// Authored identifier token.
        name: String,
    },
    /// A policy-pinned primary key would silently discard an author PK.
    #[error(
        "createTable {table:?} declares an author primaryKey under a policy-pinned table shape"
    )]
    AuthorPrimaryKeyForbidden {
        /// Table being resolved.
        table: String,
    },
    /// The legacy internal platform-ID fold found a malformed prefix.
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
/// [`zero_migrate_policy::InjectSpec`]'s columns/indexes plus the first pinned primary key. This is the
/// per-object content the resolver lays into the IR — the flattening of
/// `injects_for(object)` into a single ordered shape.
///
/// Column order is the sealed inject total order (outermost inject first, each
/// spec's columns in document order); indexes likewise. The primary key is the
/// FIRST covering spec that pins one (the outermost charter wins — a draft cannot
/// override a charter PK, which `admit`'s collision blame already
/// guarantees is non-conflicting). `author_primary_key` is `Forbid` if ANY covering
/// spec forbids (obligations union up).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedInject {
    columns: Vec<IrColumn>,
    indexes: Vec<IrIndex>,
    primary_key: Option<Vec<String>>,
    author_primary_key: AuthorPkPolicy,
}

impl ResolvedInject {
    /// Flatten the covering inject specs at `object` into a single ordered shape.
    pub(crate) fn for_object(
        effective: &EffectivePolicy,
        object: &ObjectName,
    ) -> Result<Self, TableShapeError> {
        let mut columns: Vec<IrColumn> = Vec::new();
        let mut indexes: Vec<IrIndex> = Vec::new();
        let mut primary_key: Option<Vec<String>> = None;
        let mut author_primary_key = AuthorPkPolicy::Allow;
        for spec in effective.injects_for(object) {
            columns.extend(
                spec.columns
                    .iter()
                    .map(inject_column_to_ir)
                    .collect::<Result<Vec<_>, _>>()?,
            );
            indexes.extend(
                spec.indexes
                    .iter()
                    .map(inject_index_to_ir)
                    .collect::<Result<Vec<_>, _>>()?,
            );
            if primary_key.is_none() {
                primary_key = spec
                    .primary_key
                    .as_ref()
                    .map(|columns| {
                        columns
                            .iter()
                            .map(|name| canonical_inject_identifier(name, "primary-key column"))
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .transpose()?;
            }
            if matches!(spec.author_primary_key, AuthorPkPolicy::Forbid) {
                author_primary_key = AuthorPkPolicy::Forbid;
            }
        }
        Ok(Self {
            columns,
            indexes,
            primary_key,
            author_primary_key,
        })
    }

    /// Resolve the injection covering one concrete `schema.table` object.
    ///
    /// # Errors
    /// Returns [`TableShapeError`] when an injected type or default token cannot
    /// be represented by the engine's closed IR.
    pub fn for_table(
        effective: &EffectivePolicy,
        schema: &str,
        table: &str,
    ) -> Result<Self, TableShapeError> {
        Self::for_object(
            effective,
            &ObjectName::table(schema.as_bytes().to_vec(), table.as_bytes().to_vec()),
        )
    }

    /// Canonical injected columns, in sealed policy order.
    pub fn columns(&self) -> &[IrColumn] {
        &self.columns
    }

    /// Canonical injected indexes, in sealed policy order.
    pub fn indexes(&self) -> &[IrIndex] {
        &self.indexes
    }

    /// Policy-pinned primary-key columns, when present.
    pub fn primary_key(&self) -> Option<&[String]> {
        self.primary_key.as_deref()
    }

    /// Whether the active injection owns a column with this name.
    #[must_use]
    pub fn contains_column(&self, name: &str) -> bool {
        self.columns.iter().any(|column| column.name == name)
    }

    /// Whether this injection owns the canonical single-column `id` primary key.
    ///
    /// Legacy ID-prefix and integer-identity folds are valid only for this exact
    /// policy-declared shape. Merely injecting an ordinary column named `id` does
    /// not activate platform-primary-key semantics.
    #[must_use]
    pub fn owns_id_primary_key(&self) -> bool {
        matches!(self.primary_key.as_deref(), Some([column]) if column == "id")
            && self.contains_column("id")
    }

    fn owns_id_primary_key_column(&self, name: &str) -> bool {
        name == "id" && self.owns_id_primary_key()
    }

    /// This object carries no injection — the resolver is a no-op for it.
    fn is_empty(&self) -> bool {
        self.columns.is_empty() && self.indexes.is_empty() && self.primary_key.is_none()
    }
}

/// The [`ObjectName`] a `createTable` op addresses: `schema.table` when the op
/// carries a schema qualifier, otherwise `default_schema.table`. The caller must
/// supply the same deployment schema used by snapshot construction; there is no
/// ambient `public` fallback that could select a different scoped inject rule.
fn object_for_create(name: &str, schema: Option<&str>, default_schema: &str) -> ObjectName {
    let schema = schema.unwrap_or(default_schema);
    ObjectName::table(schema.as_bytes().to_vec(), name.as_bytes().to_vec())
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
    default_schema: &str,
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
        let object = object_for_create(name, schema.as_deref(), default_schema);
        let resolved = ResolvedInject::for_object(effective, &object)?;
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
        let is_id_primary_key = inject.owns_id_primary_key_column(&system.name);
        if let Some(author_col) = collision {
            if is_id_primary_key && is_id_prefix_declaration(author_col) {
                validate_folded_id_prefix(table, author_col)?;
                folded_id_prefix = author_col.id_prefix.clone();
                folded_id = true;
            } else if is_id_primary_key && is_id_identity_replacement(author_col) {
                validate_folded_id_identity(table, author_col)?;
                folded_id = true;
            } else {
                return Err(TableShapeError::SystemColumnCollision {
                    table: table.to_string(),
                    column: system.name.clone(),
                });
            }
        }
        let mut col = system.clone();
        if is_id_primary_key {
            if let Some(author_col) = collision.filter(|c| is_id_identity_replacement(c)) {
                col = author_col.clone();
            } else {
                col.id_prefix = folded_id_prefix.clone();
                // A primary-key column may also be a typed reference (for
                // one-to-one inheritance). The prefix fold replaces the author's
                // UUID carrier with the injected text ID carrier, so copy the
                // reference facet explicitly instead of silently discarding it.
                col.references = collision.and_then(|author_col| author_col.references.clone());
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

    indexes.extend(inject.indexes.iter().cloned());

    Ok(())
}

/// Map an [`InjectColumn`]'s opaque type token to a native [`IrColumn`]. The token
/// spellings the engine understands mirror the retired `system_shape` mapping
/// (`text`, `timestamptz`/`timestamp with time zone`, `integer`/`int`).
fn inject_column_to_ir(column: &InjectColumn) -> Result<IrColumn, TableShapeError> {
    let name = canonical_inject_identifier(&column.name, "column")?;
    let ty = match column.ty.as_str() {
        // Policy-injected system string columns (the public `id`, actor stamps like
        // created_by/updated_by) are BOUNDED `VARCHAR(255)`: they hold ids, are often
        // keyed (the `id` primary key, audit indexes), and must be index-able on
        // MySQL, where an unbounded `TEXT` cannot be a key. Typing them here (not
        // just at render) keeps every path — validate, both injection resolvers, the
        // collection/query renderer — consistent.
        "text" => ColType::String { length: 255 },
        "timestamptz" | "timestamp with time zone" => ColType::Timestamp,
        "integer" | "int" => ColType::Int,
        other => {
            return Err(TableShapeError::UnsupportedSystemColumnType {
                column: name,
                data_type: other.to_string(),
            })
        }
    };
    let default = inject_default_to_ir(column, &ty)?;
    Ok(IrColumn {
        name,
        ty,
        nullable: Some(column.nullable),
        default,
        unique: None,
        value_format: None,
        references: None,
        id_prefix: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
    })
}

fn inject_default_to_ir(
    column: &InjectColumn,
    ty: &ColType,
) -> Result<Option<IrDefault>, TableShapeError> {
    let Some(default) = column.default.as_deref() else {
        return Ok(None);
    };
    let normalized = default.trim();
    let mapped = match ty {
        ColType::Timestamp
            if matches!(
                normalized.to_ascii_lowercase().as_str(),
                "now" | "now()" | "current_timestamp"
            ) =>
        {
            Some(IrDefault::Expr {
                expr: Expr::FnSynth {
                    r#fn: SynthFn::Now,
                    args: Vec::new(),
                },
            })
        }
        ColType::Int => normalized
            .parse::<i64>()
            .ok()
            .map(|value| IrDefault::Literal {
                value: crate::model::ir::IrScalar::Int(value),
            }),
        ColType::Text | ColType::String { .. }
            if normalized.len() >= 2
                && normalized.starts_with('\'')
                && normalized.ends_with('\'') =>
        {
            let value = normalized[1..normalized.len() - 1].replace("''", "'");
            Some(IrDefault::Literal {
                value: crate::model::ir::IrScalar::Str(value),
            })
        }
        _ => None,
    };
    mapped
        .map(Some)
        .ok_or_else(|| TableShapeError::UnsupportedSystemColumnDefault {
            column: column.name.clone(),
            default: default.to_string(),
        })
}

fn inject_index_to_ir(index: &InjectIndex) -> Result<IrIndex, TableShapeError> {
    Ok(IrIndex {
        // Inject index names have historically been logical policy labels. The
        // physical name remains the engine's deterministic table/column name.
        name: None,
        columns: index
            .columns
            .iter()
            .map(|name| {
                Ok(IndexElement::Column {
                    name: canonical_inject_identifier(name, "index column")?,
                    order: None,
                    opclass: None,
                    collation: None,
                })
            })
            .collect::<Result<Vec<_>, TableShapeError>>()?,
        unique: None,
        using: None,
        r#where: None,
        include: Vec::new(),
        with: None,
        only: None,
        nulls_not_distinct: None,
    })
}

fn canonical_inject_identifier(raw: &str, kind: &'static str) -> Result<String, TableShapeError> {
    let quoted = raw.starts_with('"');
    if !quoted {
        let mut chars = raw.chars();
        if !chars
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
            || !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            return Err(TableShapeError::InvalidInjectIdentifier {
                kind,
                name: raw.to_string(),
            });
        }
    }
    let normalized = normalize_pg_identifier(raw)
        .filter(|object| object.table.is_none())
        .and_then(|object| String::from_utf8(object.schema).ok())
        .filter(|name| name.len() <= 63)
        .ok_or_else(|| TableShapeError::InvalidInjectIdentifier {
            kind,
            name: raw.to_string(),
        })?;
    Ok(normalized)
}

fn is_id_prefix_declaration(column: &IrColumn) -> bool {
    column.name == "id" && matches!(column.ty, ColType::Uuid) && column.id_prefix.is_some()
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
        Some(default) if is_uuid_v4_default(default) => false,
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
fn uuid_v4_default() -> IrDefault {
    IrDefault::Expr { expr: Expr::UuidV4 }
}

fn is_uuid_v4_default(default: &IrDefault) -> bool {
    matches!(default, IrDefault::Expr { expr: Expr::UuidV4 })
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
/// [`zero_migrate_policy::InjectSpec`]s (`inject`). Beyond the leading-prefix name/shape match it adds
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
    if columns.len() < inject.columns.len() || indexes.len() < inject.indexes.len() {
        return Ok(false);
    }
    for (actual, expected) in columns.iter().zip(&inject.columns) {
        // The `id` identity-replacement fold leaves an author identity column in the
        // `id` slot; it is a conforming resolution of the injected `id`.
        if inject.owns_id_primary_key_column(&expected.name) && is_id_identity_replacement(actual) {
            continue;
        }
        let expected_ir = expected;
        // The platform prefix fold deliberately retains two authored facets on
        // the otherwise injected `id` slot: `id_prefix` and a possible typed
        // reference. Compare the injected base shape while allowing exactly that
        // folded carrier. Other injected slots (and an unprefixed injected `id`)
        // must match `references: None` below.
        if inject.owns_id_primary_key_column(&expected.name) && actual.id_prefix.is_some() {
            let mut folded_base = actual.clone();
            folded_base.id_prefix = None;
            folded_base.references = None;
            if system_columns_match(&folded_base, expected_ir) {
                continue;
            }
        }
        if !system_columns_match(actual, expected_ir) {
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
        if actual != expected {
            return Ok(false);
        }
    }
    Ok(true)
}

fn system_columns_match(actual: &IrColumn, expected: &IrColumn) -> bool {
    // II.2.6b conformance: an occupant of an injected column slot must match the
    // inject's name AND its type/nullability/default (the shape the InjectSpec
    // carries — `inject_column_to_ir` maps the opaque token; injected columns carry
    // the canonical default mapped from the rule, when present).
    actual.name == expected.name
        && actual.ty == expected.ty
        && actual.nullable == expected.nullable
        && actual.default == expected.default
        && actual.unique == expected.unique
        && actual.references == expected.references
        && actual.case_sensitive == expected.case_sensitive
        && actual.vector_metric == expected.vector_metric
        && actual.mask == expected.mask
        && actual.generated == expected.generated
        && actual.identity == expected.identity
}

/// Build an [`EffectivePolicy`] from a `RootCharter` document (TOML). The charter
/// is parsed against the engine's builtin registry, then composes against a
/// grant-only draft extracted from the same charter. Inject/require/validate
/// rules survive from the root charter; grants become effective through the draft
/// side of `admit` after proving they do not exceed the root charter.
/// Inject-only charters still compose because the extracted draft is empty.
///
/// This is the engine-side constructor the production authoring verb
/// (`lower_envelope_to_migrations`) and tests both go through. The engine never
/// fabricates an `EffectivePolicy` by hand.
///
/// # Errors
/// A human-readable message on: a malformed charter document, a malformed empty
/// draft (unreachable), or a composition failure.
pub fn effective_policy_from_charter_toml(charter_toml: &str) -> Result<EffectivePolicy, String> {
    let registry = policy_registry::builtin_registry();
    let charter = zero_migrate_policy::RootCharter::parse_toml(charter_toml, &registry)
        .map_err(|e| format!("policy charter failed to load: {e:?}"))?;
    let draft_toml = grant_only_draft_toml(charter_toml)?;
    let draft = zero_migrate_policy::PolicyDoc::parse_toml(
        &draft_toml,
        &registry,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .map_err(|e| format!("empty policy draft failed to load: {e:?}"))?;
    zero_migrate_policy::admit(&charter, &draft, &registry)
        .map_err(|e| format!("policy composition failed: {e:?}"))
}

/// Compose an ORDERED list of charter documents into one sealed [`EffectivePolicy`].
/// `layers[0]` is the ROOT charter (the bound; the only layer where a `mandatory`
/// inject is legal). Each subsequent document is admitted as an untrusted narrowing
/// layer over the accumulated policy: its grants must be less than or equal to the
/// bound (rejected, not clipped, on escalation) and its inject/require/validate rules
/// union up. Grants a layer is silent on inherit from below.
///
/// # Errors
/// Returns an error when no root charter is supplied, a document fails to load in
/// its required context, or a non-root layer exceeds the accumulated charter.
pub fn effective_policy_from_charter_layers(layers: &[&str]) -> Result<EffectivePolicy, String> {
    let Some(root) = layers.first() else {
        return Err("at least one policy charter is required".to_string());
    };
    if layers.len() == 1 {
        return effective_policy_from_charter_toml(root);
    }

    let registry = policy_registry::builtin_registry();
    let mut acc = effective_policy_from_charter_toml(root)?;
    for (index, source) in layers.iter().enumerate().skip(1) {
        let draft = zero_migrate_policy::PolicyDoc::parse_toml(
            source,
            &registry,
            zero_migrate_policy::LoadContext::NonRootLayer,
        )
        .map_err(|e| format!("policy layer {} failed to load: {e:?}", index + 1))?;
        acc = zero_migrate_policy::admit(&acc, &draft, &registry)
            .map_err(|e| format!("policy layer {} rejected: {e:?}", index + 1))?;
    }
    Ok(acc)
}

fn grant_only_draft_toml(charter_toml: &str) -> Result<String, String> {
    let parsed: toml::Value = toml::from_str(charter_toml)
        .map_err(|e| format!("policy charter failed to parse as TOML: {e}"))?;
    let Some(table) = parsed.as_table() else {
        return Err("policy charter root must be a TOML table".to_string());
    };

    let mut draft = toml::map::Map::new();
    let Some(version) = table.get("policy_version").cloned() else {
        return Err("policy charter is missing policy_version".to_string());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ir::{CanonicalOpList, ColumnReference, MigrationIr, CURRENT_IR_VERSION};
    use crate::test_fixtures::{confined_charter, no_inject, CONFINED_CHARTER_TOML};
    use crate::{Checksum, MigrationFlags};

    fn resolve_create_table_policy(
        ir: &MigrationIr,
        effective: &EffectivePolicy,
    ) -> Result<MigrationIr, TableShapeError> {
        super::resolve_create_table_policy(ir, effective, "app")
    }

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
            value_format: None,
            references: None,
            id_prefix: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        }
    }

    fn reference(table: &str) -> ColumnReference {
        ColumnReference {
            table: table.to_string(),
            column: "id".to_string(),
            on_delete: None,
            on_update: None,
            name: None,
        }
    }

    /// An alternate charter with the SAME injected shape as the confined charter but
    /// authored differently (indexes named differently — index names are not part of
    /// the injected IR shape; the resolver appends unnamed IR indexes over the
    /// injected columns). Used to prove checksum-invariance under equivalent-shape
    /// policies (G6).
    fn equivalent_shape_charter() -> EffectivePolicy {
        let toml = CONFINED_CHARTER_TOML
            .replace("ix_deleted_at", "renamed_deleted_at_idx")
            .replace("ix_updated_at", "renamed_updated_at_idx")
            .replace("ix_created_by", "renamed_created_by_idx");
        effective_policy_from_charter_toml(&toml).expect("equivalent-shape charter composes")
    }

    #[test]
    fn unquoted_inject_identifiers_are_canonicalized_before_collision_checks() {
        let effective = effective_policy_from_charter_toml(
            r#"policy_version = 1

[[inject]]
scope = "all"
mandatory = true
primary_key = ["UPDATED_AT"]
author_primary_key = "allow"
columns = [
  { name = "UPDATED_AT", type = "timestamptz", nullable = false },
]
indexes = [
  { name = "ix_updated_at", columns = ["UPDATED_AT"] },
]
"#,
        )
        .expect("uppercase-identifier policy composes");

        let inject = ResolvedInject::for_table(&effective, "app", "widgets")
            .expect("uppercase policy identifiers resolve");
        assert_eq!(inject.columns()[0].name, "updated_at");
        assert_eq!(inject.primary_key(), Some(&["updated_at".to_string()][..]));
        assert!(matches!(
            &inject.indexes()[0].columns[0],
            IndexElement::Column { name, .. } if name == "updated_at"
        ));

        let error =
            resolve_create_table_policy(&ir(vec![text_col("updated_at")], None), &effective)
                .expect_err("canonical injected name must collide with the author column");
        assert!(matches!(
            error,
            TableShapeError::SystemColumnCollision { column, .. } if column == "updated_at"
        ));
    }

    #[test]
    fn malformed_inject_identifier_is_rejected_during_resolution() {
        let effective = effective_policy_from_charter_toml(
            r#"policy_version = 1

[[inject]]
scope = "all"
mandatory = true
columns = [
  { name = "bad name", type = "text", nullable = false },
]
"#,
        )
        .expect("policy loader accepts opaque inject identifier tokens");

        let error = ResolvedInject::for_table(&effective, "app", "widgets")
            .expect_err("an inject column must be one portable SQL identifier");
        assert!(matches!(
            error,
            TableShapeError::InvalidInjectIdentifier { kind: "column", name }
                if name == "bad name"
        ));
    }

    #[test]
    fn short_text_defaults_return_typed_errors_without_panicking() {
        for default in ["", "'"] {
            let effective = effective_policy_from_charter_toml(&format!(
                r#"policy_version = 1

[[inject]]
scope = "all"
mandatory = true
columns = [
  {{ name = "status", type = "text", nullable = false, default = "{default}" }},
]
"#
            ))
            .expect("short-default policy composes");

            let error = ResolvedInject::for_table(&effective, "app", "widgets")
                .expect_err("a short non-literal text default is unsupported");
            assert!(matches!(
                error,
                TableShapeError::UnsupportedSystemColumnDefault { column, default: actual }
                    if column == "status" && actual == default
            ));
        }
    }

    #[test]
    fn confined_prepends_exact_resolved_inject_shape_and_pk() {
        let effective = confined_charter();
        let inject = ResolvedInject::for_table(&effective, "public", "widgets")
            .expect("confined injection resolves");
        let resolved = resolve_create_table_policy(&ir(vec![text_col("title")], None), &effective)
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
            &names[..inject.columns().len()],
            inject
                .columns()
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(names[inject.columns().len()], "title");
        assert_eq!(primary_key.as_deref(), inject.primary_key());
        assert_eq!(indexes, inject.indexes());
    }

    #[test]
    fn platform_preserves_author_shape() {
        // The explicit no-inject charter is a no-op: the author-owned table shape
        // passes through verbatim.
        let input = ir(
            vec![text_col("id"), text_col("team")],
            Some(vec!["id".into()]),
        );
        let no_inject = no_inject("app");
        let resolved =
            resolve_create_table_policy(&input, &no_inject).expect("no-inject charter is a no-op");
        assert_eq!(resolved, input);
    }

    #[test]
    fn system_column_collision_is_rejected_except_id_prefix() {
        let err = resolve_create_table_policy(
            &ir(vec![text_col("created_at")], None),
            &confined_charter(),
        )
        .expect_err("created_at collision");
        assert!(matches!(
            err,
            TableShapeError::SystemColumnCollision { column, .. } if column == "created_at"
        ));

        let mut id = text_col("id");
        id.ty = ColType::Uuid;
        id.nullable = Some(false);
        id.default = Some(uuid_v4_default());
        id.id_prefix = Some("post".into());
        let resolved = resolve_create_table_policy(
            &ir(vec![id], Some(vec!["id".into()])),
            &confined_charter(),
        )
        .expect("id prefix folds");
        let Op::CreateTable { columns, .. } = &resolved.ops[0] else {
            panic!("create op")
        };
        assert_eq!(columns.iter().filter(|c| c.name == "id").count(), 1);
        assert_eq!(columns[0].id_prefix.as_deref(), Some("post"));
    }

    #[test]
    fn id_prefix_fold_preserves_typed_reference_and_remains_idempotent() {
        let mut id = text_col("id");
        id.ty = ColType::Uuid;
        id.nullable = Some(false);
        id.default = Some(uuid_v4_default());
        id.id_prefix = Some("post".into());
        id.references = Some(reference("parent_posts"));

        let once = resolve_create_table_policy(
            &ir(vec![id], Some(vec!["id".into()])),
            &confined_charter(),
        )
        .expect("a primary-key reference survives the platform ID prefix fold");
        let Op::CreateTable { columns, .. } = &once.ops[0] else {
            panic!("create op")
        };
        assert_eq!(columns[0].id_prefix.as_deref(), Some("post"));
        assert_eq!(columns[0].references, Some(reference("parent_posts")));

        let twice = resolve_create_table_policy(&once, &confined_charter())
            .expect("a folded ID reference is a conforming resolved shape");
        assert_eq!(once, twice);
    }

    #[test]
    fn forged_reference_on_injected_system_column_is_rejected() {
        let mut resolved =
            resolve_create_table_policy(&ir(vec![text_col("title")], None), &confined_charter())
                .expect("initial policy resolution");
        let Op::CreateTable { columns, .. } = &mut resolved.ops[0] else {
            panic!("create op")
        };
        columns
            .iter_mut()
            .find(|column| column.name == "created_at")
            .expect("injected created_at")
            .references = Some(reference("other_rows"));

        let error = resolve_create_table_policy(&resolved, &confined_charter())
            .expect_err("an injected slot cannot acquire an authored reference facet");
        assert!(matches!(
            error,
            TableShapeError::SystemColumnCollision { .. }
        ));
    }

    #[test]
    fn explicit_uuid_primary_key_is_not_reinterpreted_as_the_platform_id() {
        let mut id = text_col("id");
        id.ty = ColType::Uuid;
        id.nullable = Some(false);
        id.default = Some(uuid_v4_default());

        let err = resolve_create_table_policy(
            &ir(vec![id], Some(vec!["id".into()])),
            &confined_charter(),
        )
        .expect_err("an explicit UUID key must not fold into the injected text key");

        assert!(matches!(
            err,
            TableShapeError::SystemColumnCollision { column, .. } if column == "id"
        ));
    }

    #[test]
    fn bare_author_id_without_a_prefix_or_identity_is_refused() {
        // Only the two folding forms may reuse the injected `id` slot: an `id_prefix`
        // declaration and an identity replacement. A bare author `id` carrying
        // neither is a collision even when it claims no primary key, which is the
        // case the sibling test above does not reach.
        let mut id = text_col("id");
        id.ty = ColType::Uuid;
        id.nullable = Some(false);

        let err = resolve_create_table_policy(&ir(vec![id], None), &confined_charter())
            .expect_err("a bare author `id` cannot occupy the injected platform key");

        assert!(matches!(
            err,
            TableShapeError::SystemColumnCollision { column, .. } if column == "id"
        ));
    }

    #[test]
    fn author_primary_key_under_pinned_pk_is_forbidden() {
        // A non-`id` author PK under the confined (PK-pinning, author-PK-forbid)
        // charter is rejected.
        let mut extra = text_col("code");
        extra.nullable = Some(false);
        let err = resolve_create_table_policy(
            &ir(vec![extra], Some(vec!["code".into()])),
            &confined_charter(),
        )
        .expect_err("author PK forbidden under pinned PK");
        assert!(matches!(
            err,
            TableShapeError::AuthorPrimaryKeyForbidden { .. }
        ));
    }

    #[test]
    fn pinned_pk_charter_silent_on_author_primary_key_never_rewrites_the_author_key() {
        // Under a pinned PK the two `author_primary_key` readings are not symmetric:
        // the pin overwrites the author's key either way, so `allow` only suppresses
        // the rejection. A charter that pins a key and says nothing must therefore not
        // resolve to the permissive reading and discard an author-declared primary key
        // with no diagnostic. Refusing at load and refusing at resolution both satisfy
        // this; silently rewriting does not.
        let silent = CONFINED_CHARTER_TOML.replace("author_primary_key = \"forbid\"\n", "");
        assert!(
            !silent.contains("author_primary_key"),
            "the charter under test must be silent on author_primary_key"
        );

        let mut code = text_col("code");
        code.nullable = Some(false);
        let authored = ir(vec![code], Some(vec!["code".into()]));

        match effective_policy_from_charter_toml(&silent) {
            // Refused at load: the charter never reaches a resolver at all.
            Err(_) => {}
            // Admitted: resolution owes the author a refusal.
            Ok(effective) => {
                let err = resolve_create_table_policy(&authored, &effective).expect_err(
                    "a charter that pins a primary key and omits author_primary_key must not \
                     silently discard the author-declared primary key",
                );
                assert!(matches!(
                    err,
                    TableShapeError::AuthorPrimaryKeyForbidden { .. }
                ));
            }
        }
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
    /// the policy was authored. Two charters that inject the SAME columns/indexes/PK
    /// resolve to byte-identical IR ⇒ the SAME checksum. And the checksum IS sensitive
    /// to the actual injected columns (a no-inject charter differs).
    #[test]
    fn checksum_is_invariant_under_equivalent_shape_and_sensitive_to_injection() {
        let input = ir(vec![text_col("title")], None);

        let confined = resolve_create_table_policy(&input, &confined_charter()).expect("confined");
        let equivalent =
            resolve_create_table_policy(&input, &equivalent_shape_charter()).expect("equivalent");
        // Equivalent injected SHAPE ⇒ byte-identical resolved IR ⇒ identical checksum.
        assert_eq!(confined.ops, equivalent.ops);
        assert_eq!(checksum_of(&confined), checksum_of(&equivalent));

        // Sensitivity: a charter that injects nothing resolves to a DIFFERENT shape
        // (no system columns) ⇒ a different checksum.
        let no_inject = resolve_create_table_policy(&input, &no_inject("app")).expect("no-inject");
        assert_ne!(confined.ops, no_inject.ops);
        assert_ne!(checksum_of(&confined), checksum_of(&no_inject));
    }

    /// The resolver is idempotent: re-running it over an already-resolved table is a
    /// no-op (the II.2.6b conformance check accepts the injected shape).
    #[test]
    fn resolve_is_idempotent() {
        let input = ir(vec![text_col("title")], None);
        let once = resolve_create_table_policy(&input, &confined_charter()).expect("once");
        let twice = resolve_create_table_policy(&once, &confined_charter()).expect("twice");
        assert_eq!(once, twice);
    }
}
