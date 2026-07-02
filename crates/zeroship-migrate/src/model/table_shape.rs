//! Resolve profile-managed table shape into explicit `createTable` IR.

use crate::model::ir::{
    ColType, IndexElement, IrColumn, IrDefault, IrIndex, MigrationIr, Op, SynthDefaultFn,
};
use crate::model::profile::{PolicyProfile, TablePrimaryKeyPolicy};

/// Error raised while applying a [`PolicyProfile`]'s table system shape.
#[derive(Debug, thiserror::Error)]
pub enum TableShapeError {
    /// A profile system column collided with an author-declared column.
    #[error(
        "createTable {table:?} declares column {column:?}, which collides with an injected system column"
    )]
    SystemColumnCollision {
        /// Table being resolved.
        table: String,
        /// Colliding column name.
        column: String,
    },
    /// The profile contains a system column type this engine cannot express in IR.
    #[error("system column {column:?} uses unsupported type {data_type:?}")]
    UnsupportedSystemColumnType {
        /// Column name.
        column: String,
        /// Profile data-type spelling.
        data_type: String,
    },
    /// A confined/profile-owned primary key would silently discard an author PK.
    #[error("createTable {table:?} declares an author primaryKey under a profile-owned table shape")]
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
    #[error("createTable {table:?} declares id as a system-prefix field with unsupported modifiers")]
    InvalidIdPrefixDeclaration {
        /// Table being resolved.
        table: String,
    },
}

/// Apply the active profile's table-shape policy to every `createTable` op.
///
/// The returned IR is the self-contained artifact shape: profile system columns
/// are prepended, profile system indexes are appended, and the resolved
/// `primaryKey` is present before canonical bytes/checksum are computed.
pub fn resolve_create_table_policy(
    ir: &MigrationIr,
    profile: &PolicyProfile,
) -> Result<MigrationIr, TableShapeError> {
    let mut out = ir.clone();
    for op in &mut out.ops {
        let Op::CreateTable {
            name,
            columns,
            primary_key,
            indexes,
            ..
        } = op
        else {
            continue;
        };
        resolve_create_table(name, columns, primary_key, indexes, profile)?;
    }
    Ok(out)
}

fn resolve_create_table(
    table: &str,
    columns: &mut Vec<IrColumn>,
    primary_key: &mut Option<Vec<String>>,
    indexes: &mut Vec<IrIndex>,
    profile: &PolicyProfile,
) -> Result<(), TableShapeError> {
    let shape = &profile.system_shape;
    if shape.columns.is_empty() && matches!(shape.primary_key, TablePrimaryKeyPolicy::Author(_)) {
        return Ok(());
    }

    if already_resolved(columns, primary_key, indexes, profile)? {
        return Ok(());
    }

    let mut folded_id_prefix: Option<String> = None;
    let mut folded_id = false;
    let mut resolved_columns = Vec::with_capacity(shape.columns.len() + columns.len());
    for system in &shape.columns {
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
        let mut col = system_column_to_ir(system)?;
        if system.name == "id" {
            if let Some(author_col) = collision.filter(|c| is_id_identity_replacement(c)) {
                col = author_col.clone();
            } else {
                col.id_prefix = folded_id_prefix.clone();
            }
        }
        resolved_columns.push(col);
    }

    if let TablePrimaryKeyPolicy::ExplicitColumns(_) = &shape.primary_key {
        let author_pk_is_folded_id =
            folded_id && primary_key.as_deref().is_some_and(|pk| pk == ["id"]);
        if primary_key.is_some() && !author_pk_is_folded_id {
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

    *primary_key = match &shape.primary_key {
        TablePrimaryKeyPolicy::ExplicitColumns(columns) => Some(columns.clone()),
        TablePrimaryKeyPolicy::Author(_) => primary_key.clone(),
    };

    indexes.extend(shape.indexes.iter().map(|idx| IrIndex {
        name: None,
        columns: idx
            .columns
            .iter()
            .map(|name| IndexElement::Column { name: name.clone() })
            .collect(),
        unique: None,
        using: None,
        r#where: None,
    }));

    Ok(())
}

fn system_column_to_ir(
    column: &crate::model::profile::InjectedSystemColumnPolicy,
) -> Result<IrColumn, TableShapeError> {
    let ty = match column.data_type.as_str() {
        "text" => ColType::Text,
        "timestamp with time zone" => ColType::Timestamp,
        "integer" => ColType::Int,
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
        && matches!(column.ty, ColType::Int | ColType::BigInt)
}

fn validate_folded_id_prefix(table: &str, column: &IrColumn) -> Result<(), TableShapeError> {
    let has_unsupported_default = match column.default.as_ref() {
        None => false,
        Some(IrDefault::Fn {
            r#fn: SynthDefaultFn::GenRandomUuid,
        }) => false,
        Some(_) => true,
    };
    if column.unique.unwrap_or(false)
        || has_unsupported_default
        || column.mask.is_some()
        || column.generated.is_some()
        || column.identity.is_some()
        || column.vector_metric.is_some()
    {
        return Err(TableShapeError::InvalidIdPrefixDeclaration {
            table: table.to_string(),
        });
    }
    if let Some(prefix) = &column.id_prefix {
        zeroship_schema::query::validate_id_prefix(prefix).map_err(|e| {
            TableShapeError::InvalidIdPrefix {
                table: table.to_string(),
                prefix: prefix.clone(),
                message: e.to_string(),
            }
        })?;
    }
    Ok(())
}

fn validate_folded_id_identity(table: &str, column: &IrColumn) -> Result<(), TableShapeError> {
    let has_default = column.default.is_some();
    if column.unique.unwrap_or(false)
        || has_default
        || column.mask.is_some()
        || column.generated.is_some()
        || column.vector_metric.is_some()
        || column.id_prefix.is_some()
    {
        return Err(TableShapeError::InvalidIdPrefixDeclaration {
            table: table.to_string(),
        });
    }
    Ok(())
}

fn already_resolved(
    columns: &[IrColumn],
    primary_key: &Option<Vec<String>>,
    indexes: &[IrIndex],
    profile: &PolicyProfile,
) -> Result<bool, TableShapeError> {
    let shape = &profile.system_shape;
    if shape.columns.is_empty() {
        return Ok(false);
    }
    if columns.len() < shape.columns.len() || indexes.len() < shape.indexes.len() {
        return Ok(false);
    }
    for (actual, expected) in columns.iter().zip(&shape.columns) {
        let expected = system_column_to_ir(expected)?;
        if expected.name == "id" && is_id_identity_replacement(actual) {
            continue;
        }
        if !system_columns_match(actual, &expected) {
            return Ok(false);
        }
    }
    match &shape.primary_key {
        TablePrimaryKeyPolicy::ExplicitColumns(pk) if primary_key.as_ref() != Some(pk) => {
            return Ok(false);
        }
        _ => {}
    }
    let start = indexes.len() - shape.indexes.len();
    for (actual, expected) in indexes[start..].iter().zip(&shape.indexes) {
        let actual_cols = actual
            .columns
            .iter()
            .map(|c| match c {
                IndexElement::Column { name } => Some(name.as_str()),
                IndexElement::Expr { .. } => None,
            })
            .collect::<Option<Vec<_>>>();
        let Some(actual_cols) = actual_cols else {
            return Ok(false);
        };
        let expected_cols = expected.columns.iter().map(String::as_str).collect::<Vec<_>>();
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
    actual.name == expected.name
        && actual.ty == expected.ty
        && actual.nullable == expected.nullable
        && actual.default == expected.default
        && actual.unique == expected.unique
        && actual.vector_metric == expected.vector_metric
        && actual.mask == expected.mask
        && actual.generated == expected.generated
        && actual.identity == expected.identity
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ir::{CanonicalOpList, CURRENT_IR_VERSION, MigrationIr};
    use crate::model::profile::PolicyProfile;
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
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        }
    }

    #[test]
    fn confined_prepends_system_shape_and_pk() {
        let resolved =
            resolve_create_table_policy(&ir(vec![text_col("title")], None), &PolicyProfile::confined())
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
                        IndexElement::Column { name } => name.as_str(),
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
        let input = ir(vec![text_col("id"), text_col("team")], Some(vec!["id".into()]));
        let resolved = resolve_create_table_policy(&input, &PolicyProfile::platform())
            .expect("platform is a no-op");
        assert_eq!(resolved, input);
    }

    #[test]
    fn system_column_collision_is_rejected_except_id_prefix() {
        let err =
            resolve_create_table_policy(&ir(vec![text_col("created_at")], None), &PolicyProfile::confined())
                .expect_err("created_at collision");
        assert!(matches!(
            err,
            TableShapeError::SystemColumnCollision { column, .. } if column == "created_at"
        ));

        let mut id = text_col("id");
        id.ty = ColType::Uuid;
        id.nullable = Some(false);
        id.default = Some(IrDefault::Fn {
            r#fn: SynthDefaultFn::GenRandomUuid,
        });
        id.id_prefix = Some("post".into());
        let resolved =
            resolve_create_table_policy(&ir(vec![id], Some(vec!["id".into()])), &PolicyProfile::confined())
                .expect("id prefix folds");
        let Op::CreateTable { columns, .. } = &resolved.ops[0] else {
            panic!("create op")
        };
        assert_eq!(columns.iter().filter(|c| c.name == "id").count(), 1);
        assert_eq!(columns[0].id_prefix.as_deref(), Some("post"));
    }

    #[test]
    fn active_profile_changes_resolved_ops_and_checksum() {
        let input = ir(vec![text_col("title")], None);
        let confined =
            resolve_create_table_policy(&input, &PolicyProfile::confined()).expect("confined");
        let platform =
            resolve_create_table_policy(&input, &PolicyProfile::platform()).expect("platform");
        assert_ne!(confined.ops, platform.ops);

        let checksum = |ir: &MigrationIr| {
            Checksum::of_ir(
                &CanonicalOpList(&ir.ops),
                &MigrationFlags::default(),
                &ir.owner_app,
                &[],
                &[],
                &ir.preconditions,
            )
        };
        assert_ne!(checksum(&confined), checksum(&platform));
    }
}
