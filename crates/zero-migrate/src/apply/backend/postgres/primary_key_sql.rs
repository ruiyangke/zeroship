//! PostgreSQL execution for the structured primary-key lifecycle operation.
//!
//! This deliberately is not general drift introspection. It reads only the
//! catalog facts needed to prove one authored add/replace/drop precondition,
//! after taking an `ACCESS EXCLUSIVE` table lock in the same transaction that
//! performs the identity/constraint DDL and writes the journal event.

use std::collections::BTreeMap;
use std::time::Instant;

use crate::apply::executor::ApplyError;
use crate::apply::journal::{self, JournalError, Phase};
use crate::approval::{Approval, ApprovalScope};
use crate::conn::ExecutorConfig;
use crate::driver::SqlSession;
use crate::model::ir::AlterPrimaryKeyAction;
use crate::render::dml::quote_ident_checked;
use crate::render::step::AlterPrimaryKeyStep;

use super::session;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrimaryKey {
    constraint_name: String,
    index_oid: i64,
    columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ColumnFact {
    not_null: bool,
    identity: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UniqueKey {
    index_oid: i64,
    index_name: String,
    is_primary: bool,
    constraint_owned: bool,
    reusable_as_primary: bool,
    columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InboundForeignKey {
    constraint_name: String,
    child_schema: String,
    child_table: String,
    referenced_index_oid: i64,
    referenced_columns: Vec<String>,
}

/// Apply one structured primary-key mutation. The generic plan executor normally
/// performs the net-applied check before reaching this seam; repeating it here is
/// intentional defense in depth for direct backend callers.
pub(super) async fn apply<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    step: &AlterPrimaryKeyStep,
    approval: Approval,
    scope: &ApprovalScope,
    applied_by: &str,
) -> Result<bool, ApplyError> {
    let marker = &step.migration;
    let completed = journal::applied(conn, cfg)
        .await?
        .into_iter()
        .filter(|entry| matches!(entry.phase, Phase::Completed))
        .find(|entry| entry.version == marker.version.as_str());
    if let Some(entry) = completed {
        if entry.checksum != marker.checksum.as_str() {
            return Err(ApplyError::ChecksumDrift {
                version: marker.version.as_str().to_string(),
                recorded: entry.checksum,
                expected: marker.checksum.as_str().to_string(),
            });
        }
        return Ok(false);
    }

    let gated = marker.flags.destructive || marker.flags.requires_approval;
    if gated && approval != Approval::Approved {
        return Err(ApplyError::ApprovalRequired);
    }
    if gated && !scope.admits(marker.version.as_str()) {
        return Err(ApplyError::ApprovalNotScoped {
            version: marker.version.as_str().to_string(),
        });
    }

    // Render every engine/authored identifier before opening a transaction. A
    // malformed identifier therefore cannot strand an open transaction on an
    // early-return path.
    let table_q = format!(
        "{}.{}",
        quote_ident_checked(&step.schema)?,
        quote_ident_checked(&step.table)?
    );
    let session_sql = session::set_local_session_sql(cfg, marker)?;
    let role_sql = session::set_local_role_sql(cfg)?;
    let meta_q = quote_ident_checked(&cfg.pg.meta_schema)?;

    conn.batch("BEGIN").await?;
    let started = Instant::now();
    let result = apply_inside_transaction(
        conn,
        cfg,
        step,
        &table_q,
        &session_sql,
        role_sql.as_deref(),
        &meta_q,
        applied_by,
        started,
    )
    .await;

    match result {
        Ok(()) => {
            conn.batch("COMMIT").await?;
            Ok(true)
        }
        Err(error) => {
            if let Err(rollback_error) = conn.batch("ROLLBACK").await {
                tracing::warn!(
                    error = %rollback_error,
                    version = %marker.version.as_str(),
                    "zero-migrate: ROLLBACK failed after PostgreSQL primary-key operation"
                );
            }
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn apply_inside_transaction<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    step: &AlterPrimaryKeyStep,
    table_q: &str,
    session_sql: &str,
    role_sql: Option<&str>,
    meta_q: &str,
    applied_by: &str,
    started: Instant,
) -> Result<(), ApplyError> {
    conn.batch(session_sql).await?;

    // The lock and every catalog decision share this transaction. No live state
    // can move between the exact expectedColumns check and the final DDL.
    conn.batch(&format!("LOCK TABLE {table_q} IN ACCESS EXCLUSIVE MODE"))
        .await?;

    let primary = read_primary_key(conn, &step.schema, &step.table).await?;
    let columns = read_columns(conn, &step.schema, &step.table).await?;
    let unique_keys = read_unique_keys(conn, &step.schema, &step.table).await?;
    let inbound = read_inbound_foreign_keys(conn, &step.schema, &step.table).await?;

    validate_current_primary_key(&step.action, primary.as_ref(), &step.schema, &step.table)?;
    validate_target_columns(
        &step.action,
        &columns,
        &unique_keys,
        &step.schema,
        &step.table,
    )?;
    validate_identity_transition(&step.action, &columns, &step.schema, &step.table)?;
    validate_inbound_foreign_keys(
        &step.action,
        primary.as_ref(),
        &unique_keys,
        &inbound,
        &step.schema,
        &step.table,
    )?;

    let ddl = render_ddl(&step.action, primary.as_ref(), &unique_keys, table_q)?;

    if let Some(set_role) = role_sql {
        conn.batch(set_role).await?;
    }
    if let Err(error) = conn.batch(&ddl).await {
        return Err(ApplyError::MigrationFailed {
            version: step.migration.version.as_str().to_string(),
            source: error.into(),
        });
    }
    if cfg.pg.migrator_role.is_some() {
        conn.batch("RESET ROLE").await?;
    }

    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let inserted = conn
        .exec(
            &format!(
                "INSERT INTO {meta_q}.schema_migrations
                     (event_kind, version, name, checksum, \"by\", exec_ms, phase, outcome, kind)
                 VALUES ('{applied}', $1, $2, $3, $4, $5, 'completed', 'success', 'apply')",
                applied = journal::EventKind::Applied.as_str()
            ),
            &[
                step.migration.version.as_str().into(),
                (&step.migration.name).into(),
                step.migration.checksum.as_str().into(),
                applied_by.into(),
                exec_ms.into(),
            ],
        )
        .await
        .map_err(|error| ApplyError::Journal(JournalError::Db(error.into())))?;
    if inserted != 1 {
        return Err(ApplyError::Journal(JournalError::Backend(format!(
            "PostgreSQL primary-key journal insert affected {inserted} rows, expected 1"
        ))));
    }
    Ok(())
}

async fn read_primary_key<D: SqlSession>(
    conn: &D,
    schema: &str,
    table: &str,
) -> Result<Option<PrimaryKey>, ApplyError> {
    let rows = conn
        .query(
            "SELECT con.conname::text AS constraint_name,
                    con.conindid::bigint AS index_oid,
                    COALESCE(array_agg(att.attname::text ORDER BY key.ordinality),
                             ARRAY[]::text[]) AS columns
             FROM pg_catalog.pg_constraint con
             JOIN pg_catalog.pg_class tbl ON tbl.oid = con.conrelid
             JOIN pg_catalog.pg_namespace ns ON ns.oid = tbl.relnamespace
             CROSS JOIN LATERAL unnest(con.conkey)
               WITH ORDINALITY AS key(attnum, ordinality)
             JOIN pg_catalog.pg_attribute att
               ON att.attrelid = tbl.oid AND att.attnum = key.attnum
             WHERE ns.nspname = $1 AND tbl.relname = $2 AND con.contype = 'p'
             GROUP BY con.oid, con.conname, con.conindid",
            &[schema.into(), table.into()],
        )
        .await?;
    if rows.len() > 1 {
        return Err(ApplyError::Backend(format!(
            "PostgreSQL catalog reported multiple primary keys for {schema}.{table}"
        )));
    }
    rows.first()
        .map(|row| {
            Ok(PrimaryKey {
                constraint_name: row.try_get("constraint_name")?,
                index_oid: row.try_get("index_oid")?,
                columns: row.try_get("columns")?,
            })
        })
        .transpose()
}

async fn read_columns<D: SqlSession>(
    conn: &D,
    schema: &str,
    table: &str,
) -> Result<BTreeMap<String, ColumnFact>, ApplyError> {
    let rows = conn
        .query(
            "SELECT att.attname::text AS column_name,
                    att.attnotnull AS not_null,
                    (att.attidentity <> '') AS is_identity
             FROM pg_catalog.pg_attribute att
             JOIN pg_catalog.pg_class tbl ON tbl.oid = att.attrelid
             JOIN pg_catalog.pg_namespace ns ON ns.oid = tbl.relnamespace
             WHERE ns.nspname = $1 AND tbl.relname = $2
               AND att.attnum > 0 AND NOT att.attisdropped
             ORDER BY att.attnum",
            &[schema.into(), table.into()],
        )
        .await?;
    let mut facts = BTreeMap::new();
    for row in rows {
        facts.insert(
            row.try_get("column_name")?,
            ColumnFact {
                not_null: row.try_get("not_null")?,
                identity: row.try_get("is_identity")?,
            },
        );
    }
    Ok(facts)
}

async fn read_unique_keys<D: SqlSession>(
    conn: &D,
    schema: &str,
    table: &str,
) -> Result<Vec<UniqueKey>, ApplyError> {
    let rows = conn
        .query(
            "SELECT idx.indexrelid::bigint AS index_oid,
                    index_rel.relname::text AS index_name,
                    idx.indisprimary AS is_primary,
                    EXISTS (
                      SELECT 1 FROM pg_catalog.pg_constraint owner
                      WHERE owner.conindid = idx.indexrelid
                        AND owner.conrelid = idx.indrelid
                        AND owner.contype IN ('p', 'u', 'x')
                    ) AS constraint_owned,
                    (
                      am.amname = 'btree'
                      AND index_rel.relkind = 'i'
                      AND idx.indnkeyatts = idx.indnatts
                      AND NOT EXISTS (
                        SELECT 1
                        FROM generate_series(0, idx.indnkeyatts::integer - 1) AS pos(n)
                        LEFT JOIN pg_catalog.pg_attribute key_att
                          ON key_att.attrelid = idx.indrelid
                         AND key_att.attnum = idx.indkey[pos.n]
                        LEFT JOIN pg_catalog.pg_opclass opc
                          ON opc.oid = idx.indclass[pos.n]
                        WHERE key_att.attname IS NULL
                           OR idx.indoption[pos.n] <> 0
                           OR NOT opc.opcdefault
                           OR idx.indcollation[pos.n] <> key_att.attcollation
                      )
                    ) AS reusable_as_primary,
                    COALESCE((
                      SELECT array_agg(key_att.attname::text ORDER BY key.ordinality)
                      FROM unnest(idx.indkey) WITH ORDINALITY AS key(attnum, ordinality)
                      JOIN pg_catalog.pg_attribute key_att
                        ON key_att.attrelid = idx.indrelid
                       AND key_att.attnum = key.attnum
                      WHERE key.ordinality <= idx.indnkeyatts
                    ), ARRAY[]::text[]) AS columns
             FROM pg_catalog.pg_index idx
             JOIN pg_catalog.pg_class tbl ON tbl.oid = idx.indrelid
             JOIN pg_catalog.pg_namespace ns ON ns.oid = tbl.relnamespace
             JOIN pg_catalog.pg_class index_rel ON index_rel.oid = idx.indexrelid
             JOIN pg_catalog.pg_am am ON am.oid = index_rel.relam
             WHERE ns.nspname = $1 AND tbl.relname = $2
               AND idx.indisunique AND idx.indisvalid AND idx.indisready AND idx.indislive
               AND idx.indpred IS NULL AND idx.indexprs IS NULL
               AND NOT EXISTS (
                 SELECT 1
                 FROM unnest(idx.indkey) WITH ORDINALITY AS key(attnum, ordinality)
                 WHERE key.ordinality <= idx.indnkeyatts AND key.attnum = 0
               )
             ORDER BY index_rel.relname",
            &[schema.into(), table.into()],
        )
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok(UniqueKey {
                index_oid: row.try_get("index_oid")?,
                index_name: row.try_get("index_name")?,
                is_primary: row.try_get("is_primary")?,
                constraint_owned: row.try_get("constraint_owned")?,
                reusable_as_primary: row.try_get("reusable_as_primary")?,
                columns: row.try_get("columns")?,
            })
        })
        .collect()
}

async fn read_inbound_foreign_keys<D: SqlSession>(
    conn: &D,
    schema: &str,
    table: &str,
) -> Result<Vec<InboundForeignKey>, ApplyError> {
    let rows = conn
        .query(
            "SELECT fk.conname::text AS constraint_name,
                    child_ns.nspname::text AS child_schema,
                    child.relname::text AS child_table,
                    fk.conindid::bigint AS referenced_index_oid,
                    COALESCE(array_agg(parent_att.attname::text ORDER BY key.ordinality),
                             ARRAY[]::text[]) AS referenced_columns
             FROM pg_catalog.pg_constraint fk
             JOIN pg_catalog.pg_class parent ON parent.oid = fk.confrelid
             JOIN pg_catalog.pg_namespace parent_ns ON parent_ns.oid = parent.relnamespace
             JOIN pg_catalog.pg_class child ON child.oid = fk.conrelid
             JOIN pg_catalog.pg_namespace child_ns ON child_ns.oid = child.relnamespace
             CROSS JOIN LATERAL unnest(fk.confkey)
               WITH ORDINALITY AS key(attnum, ordinality)
             JOIN pg_catalog.pg_attribute parent_att
               ON parent_att.attrelid = parent.oid AND parent_att.attnum = key.attnum
             WHERE parent_ns.nspname = $1 AND parent.relname = $2 AND fk.contype = 'f'
             GROUP BY fk.oid, fk.conname, child_ns.nspname, child.relname, fk.conindid
             ORDER BY child_ns.nspname, child.relname, fk.conname",
            &[schema.into(), table.into()],
        )
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok(InboundForeignKey {
                constraint_name: row.try_get("constraint_name")?,
                child_schema: row.try_get("child_schema")?,
                child_table: row.try_get("child_table")?,
                referenced_index_oid: row.try_get("referenced_index_oid")?,
                referenced_columns: row.try_get("referenced_columns")?,
            })
        })
        .collect()
}

fn validate_current_primary_key(
    action: &AlterPrimaryKeyAction,
    primary: Option<&PrimaryKey>,
    schema: &str,
    table: &str,
) -> Result<(), ApplyError> {
    match action {
        AlterPrimaryKeyAction::Add { .. } => {
            if let Some(actual) = primary {
                return Err(precondition_error(
                    schema,
                    table,
                    format!(
                        "add requires no current primary key; live key is ({})",
                        actual.columns.join(", ")
                    ),
                ));
            }
        }
        AlterPrimaryKeyAction::Replace {
            expected_columns, ..
        }
        | AlterPrimaryKeyAction::Drop {
            expected_columns, ..
        } => {
            let Some(actual) = primary else {
                return Err(precondition_error(
                    schema,
                    table,
                    format!(
                        "expected primary key ({}) but the table has no primary key",
                        expected_columns.join(", ")
                    ),
                ));
            };
            if actual.columns != *expected_columns {
                return Err(precondition_error(
                    schema,
                    table,
                    format!(
                        "expected primary key ({}) but live key is ({}) (order is significant)",
                        expected_columns.join(", "),
                        actual.columns.join(", ")
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn validate_target_columns(
    action: &AlterPrimaryKeyAction,
    columns: &BTreeMap<String, ColumnFact>,
    unique_keys: &[UniqueKey],
    schema: &str,
    table: &str,
) -> Result<(), ApplyError> {
    let Some(target) = action.target_columns() else {
        return Ok(());
    };
    for name in target {
        let Some(fact) = columns.get(name) else {
            return Err(precondition_error(
                schema,
                table,
                format!("target primary-key column '{name}' does not exist"),
            ));
        };
        if !fact.not_null {
            return Err(precondition_error(
                schema,
                table,
                format!(
                    "target primary-key column '{name}' is nullable; set NOT NULL before altering the primary key"
                ),
            ));
        }
    }
    if !unique_keys
        .iter()
        .any(|key| !key.is_primary && key.columns == target)
    {
        return Err(precondition_error(
            schema,
            table,
            format!(
                "no exact pre-existing unique candidate exists for target key ({})",
                target.join(", ")
            ),
        ));
    }
    Ok(())
}

fn validate_identity_transition(
    action: &AlterPrimaryKeyAction,
    columns: &BTreeMap<String, ColumnFact>,
    schema: &str,
    table: &str,
) -> Result<(), ApplyError> {
    let target = action.target_columns();
    let declared = action.drop_identity_from();

    for name in declared {
        let Some(fact) = columns.get(name) else {
            return Err(precondition_error(
                schema,
                table,
                format!("dropIdentityFrom column '{name}' does not exist"),
            ));
        };
        if !fact.identity {
            return Err(precondition_error(
                schema,
                table,
                format!("dropIdentityFrom column '{name}' is not a PostgreSQL identity column"),
            ));
        }
    }

    for name in action.expected_columns().into_iter().flatten() {
        let Some(fact) = columns.get(name) else {
            continue; // Exact current-key/column validation reports this first.
        };
        if !fact.identity {
            continue;
        }
        // PostgreSQL permits a target-gated identity component inside a
        // composite primary key. Unlike MySQL/SQLite, it need not be the sole
        // component; it loses this lifecycle contract only when the new key no
        // longer contains it (or the key is dropped).
        let remains_valid = matches!(target, Some(target) if target.contains(name));
        if !remains_valid && !declared.contains(name) {
            return Err(precondition_error(
                schema,
                table,
                format!(
                    "identity column '{name}' would no longer be a primary-key component; list it in dropIdentityFrom"
                ),
            ));
        }
    }
    Ok(())
}

fn validate_inbound_foreign_keys(
    action: &AlterPrimaryKeyAction,
    primary: Option<&PrimaryKey>,
    unique_keys: &[UniqueKey],
    inbound: &[InboundForeignKey],
    schema: &str,
    table: &str,
) -> Result<(), ApplyError> {
    if matches!(action, AlterPrimaryKeyAction::Add { .. }) {
        return Ok(());
    }
    let Some(primary) = primary else {
        return Ok(()); // Exact-current-key validation reports this first.
    };

    for fk in inbound {
        let alternate = unique_keys.iter().find(|key| {
            key.index_oid != primary.index_oid
                && !key.is_primary
                && key.columns == fk.referenced_columns
        });
        if alternate.is_none() {
            return Err(precondition_error(
                schema,
                table,
                format!(
                    "inbound foreign key {}.{}.{} references ({}) and has no exact alternate unique key",
                    fk.child_schema,
                    fk.child_table,
                    fk.constraint_name,
                    fk.referenced_columns.join(", ")
                ),
            ));
        }

        // PostgreSQL binds an FK to one concrete referenced index (`conindid`).
        // Merely creating another equivalent index later does not repoint that
        // dependency, and DROP CONSTRAINT would fail. This operation intentionally
        // does not migrate FKs, so fail before DDL even when an alternate exists.
        if fk.referenced_index_oid == primary.index_oid {
            return Err(precondition_error(
                schema,
                table,
                format!(
                    "inbound foreign key {}.{}.{} is still physically bound to the current primary-key index; recreate it against the exact alternate unique key before altering the primary key",
                    fk.child_schema, fk.child_table, fk.constraint_name
                ),
            ));
        }

        if !unique_keys.iter().any(|key| {
            key.index_oid == fk.referenced_index_oid && key.columns == fk.referenced_columns
        }) {
            return Err(precondition_error(
                schema,
                table,
                format!(
                    "inbound foreign key {}.{}.{} is not backed by a live exact alternate unique key",
                    fk.child_schema, fk.child_table, fk.constraint_name
                ),
            ));
        }
    }
    Ok(())
}

fn render_ddl(
    action: &AlterPrimaryKeyAction,
    primary: Option<&PrimaryKey>,
    unique_keys: &[UniqueKey],
    table_q: &str,
) -> Result<String, ApplyError> {
    let mut statements = Vec::new();
    for column in action.drop_identity_from() {
        statements.push(format!(
            "ALTER TABLE {table_q} ALTER COLUMN {} DROP IDENTITY",
            quote_ident_checked(column)?
        ));
    }

    match action {
        AlterPrimaryKeyAction::Add { columns } => {
            statements.push(render_add_primary_key(table_q, None, columns, unique_keys)?);
        }
        AlterPrimaryKeyAction::Replace { columns, .. } => {
            let primary = primary.expect("validated replace has a live primary key");
            statements.push(format!(
                "ALTER TABLE {table_q} DROP CONSTRAINT {}",
                quote_ident_checked(&primary.constraint_name)?
            ));
            statements.push(render_add_primary_key(
                table_q,
                Some(&primary.constraint_name),
                columns,
                unique_keys,
            )?);
        }
        AlterPrimaryKeyAction::Drop { .. } => {
            let primary = primary.expect("validated drop has a live primary key");
            statements.push(format!(
                "ALTER TABLE {table_q} DROP CONSTRAINT {}",
                quote_ident_checked(&primary.constraint_name)?
            ));
        }
    }
    Ok(format!("{};", statements.join(";\n")))
}

fn render_add_primary_key(
    table_q: &str,
    constraint_name: Option<&str>,
    target: &[String],
    unique_keys: &[UniqueKey],
) -> Result<String, ApplyError> {
    // Attaching a compatible standalone index avoids a second index build. A
    // UNIQUE constraint's owned index cannot be re-owned without dropping that
    // constraint (out of scope), so that candidate takes the ordinary ADD path.
    let reusable = unique_keys.iter().find(|key| {
        !key.is_primary && !key.constraint_owned && key.reusable_as_primary && key.columns == target
    });
    let constraint = constraint_name
        .map(quote_ident_checked)
        .transpose()?
        .map(|name| format!("CONSTRAINT {name} "))
        .unwrap_or_default();
    if let Some(index) = reusable {
        return Ok(format!(
            "ALTER TABLE {table_q} ADD {constraint}PRIMARY KEY USING INDEX {}",
            quote_ident_checked(&index.index_name)?
        ));
    }
    let columns = target
        .iter()
        .map(|column| quote_ident_checked(column))
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    Ok(format!(
        "ALTER TABLE {table_q} ADD {constraint}PRIMARY KEY ({columns})"
    ))
}

fn precondition_error(schema: &str, table: &str, detail: impl std::fmt::Display) -> ApplyError {
    ApplyError::Backend(format!(
        "PostgreSQL primary-key precondition failed for {schema}.{table}: {detail}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    use crate::driver::{Bind, DbError, Row, Value};
    use crate::model::migration::{
        Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId,
    };

    fn pk(columns: &[&str]) -> PrimaryKey {
        PrimaryKey {
            constraint_name: "items_pkey".into(),
            index_oid: 10,
            columns: columns.iter().map(|value| (*value).into()).collect(),
        }
    }

    fn unique(index_oid: i64, columns: &[&str]) -> UniqueKey {
        UniqueKey {
            index_oid,
            index_name: format!("unique_{index_oid}"),
            is_primary: false,
            constraint_owned: false,
            reusable_as_primary: true,
            columns: columns.iter().map(|value| (*value).into()).collect(),
        }
    }

    struct RecordingSession {
        log: RefCell<Vec<String>>,
        live_primary_columns: Vec<String>,
    }

    impl RecordingSession {
        fn new(live_primary_columns: &[&str]) -> Self {
            Self {
                log: RefCell::new(Vec::new()),
                live_primary_columns: live_primary_columns
                    .iter()
                    .map(|column| (*column).to_string())
                    .collect(),
            }
        }

        fn rows_for(&self, sql: &str) -> Vec<Row> {
            if sql.contains("union_all") && sql.contains("schema_migrations_inflight") {
                Vec::new()
            } else if sql.contains("con.contype = 'p'") {
                vec![Row::new(
                    vec![
                        "constraint_name".into(),
                        "index_oid".into(),
                        "columns".into(),
                    ],
                    vec![
                        Value::Text("items_pkey".into()),
                        Value::Int(10),
                        Value::TextArray(
                            self.live_primary_columns
                                .iter()
                                .cloned()
                                .map(Some)
                                .collect(),
                        ),
                    ],
                )]
            } else if sql.contains("att.attidentity") {
                vec![
                    Row::new(
                        vec![
                            "column_name".into(),
                            "not_null".into(),
                            "is_identity".into(),
                        ],
                        vec![
                            Value::Text("id".into()),
                            Value::Bool(true),
                            Value::Bool(true),
                        ],
                    ),
                    Row::new(
                        vec![
                            "column_name".into(),
                            "not_null".into(),
                            "is_identity".into(),
                        ],
                        vec![
                            Value::Text("tenant_id".into()),
                            Value::Bool(true),
                            Value::Bool(false),
                        ],
                    ),
                    Row::new(
                        vec![
                            "column_name".into(),
                            "not_null".into(),
                            "is_identity".into(),
                        ],
                        vec![
                            Value::Text("item_id".into()),
                            Value::Bool(true),
                            Value::Bool(false),
                        ],
                    ),
                ]
            } else if sql.contains("FROM pg_catalog.pg_index idx") {
                vec![
                    unique_row(10, "items_pkey", true, true, &["id"]),
                    unique_row(
                        20,
                        "items_candidate",
                        false,
                        false,
                        &["tenant_id", "item_id"],
                    ),
                ]
            } else if sql.contains("fk.contype = 'f'") {
                Vec::new()
            } else {
                panic!("unexpected recording-session query: {sql}");
            }
        }
    }

    fn unique_row(
        oid: i64,
        name: &str,
        primary: bool,
        constraint_owned: bool,
        columns: &[&str],
    ) -> Row {
        Row::new(
            vec![
                "index_oid".into(),
                "index_name".into(),
                "is_primary".into(),
                "constraint_owned".into(),
                "reusable_as_primary".into(),
                "columns".into(),
            ],
            vec![
                Value::Int(oid),
                Value::Text(name.into()),
                Value::Bool(primary),
                Value::Bool(constraint_owned),
                Value::Bool(true),
                Value::TextArray(
                    columns
                        .iter()
                        .map(|column| Some((*column).into()))
                        .collect(),
                ),
            ],
        )
    }

    impl SqlSession for RecordingSession {
        async fn batch(&self, sql: &str) -> Result<(), DbError> {
            self.log.borrow_mut().push(format!("batch: {sql}"));
            Ok(())
        }

        async fn exec(&self, sql: &str, _binds: &[Bind]) -> Result<u64, DbError> {
            self.log.borrow_mut().push(format!("exec: {sql}"));
            Ok(1)
        }

        async fn exec_text(&self, sql: &str, _params: &[Option<String>]) -> Result<u64, DbError> {
            self.log.borrow_mut().push(format!("exec_text: {sql}"));
            Ok(1)
        }

        async fn query(&self, sql: &str, _binds: &[Bind]) -> Result<Vec<Row>, DbError> {
            self.log.borrow_mut().push(format!("query: {sql}"));
            Ok(self.rows_for(sql))
        }

        async fn query_one(&self, sql: &str, _binds: &[Bind]) -> Result<Row, DbError> {
            self.log.borrow_mut().push(format!("query_one: {sql}"));
            self.rows_for(sql)
                .into_iter()
                .next()
                .ok_or_else(|| DbError::message("query_one: no row"))
        }
    }

    fn replacement_step(expected_columns: &[&str]) -> AlterPrimaryKeyStep {
        let flags = MigrationFlags {
            destructive: true,
            requires_approval: true,
            ..MigrationFlags::default()
        };
        let up = "-- structured primary-key operation";
        let checksum = Checksum::of(&ChecksumInput {
            up,
            down: None,
            flags: &flags,
            owner_app: "app",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        AlterPrimaryKeyStep {
            migration: Migration {
                version: MigrationId::generate(),
                name: "replace items primary key".into(),
                checksum,
                up: up.into(),
                down: None,
                flags,
                owner_app: "app".into(),
                depends_on: Vec::new(),
                supersedes: Vec::new(),
                preconditions: Vec::new(),
                existence_guard: None,
            },
            schema: "app".into(),
            table: "items".into(),
            action: AlterPrimaryKeyAction::Replace {
                expected_columns: expected_columns
                    .iter()
                    .map(|column| (*column).to_string())
                    .collect(),
                columns: vec!["tenant_id".into(), "item_id".into()],
                drop_identity_from: Some(vec!["id".into()]),
            },
        }
    }

    #[test]
    fn expected_columns_are_an_exact_ordered_precondition() {
        let action = AlterPrimaryKeyAction::Replace {
            expected_columns: vec!["tenant_id".into(), "id".into()],
            columns: vec!["id".into()],
            drop_identity_from: None,
        };
        assert!(validate_current_primary_key(
            &action,
            Some(&pk(&["tenant_id", "id"])),
            "app",
            "items"
        )
        .is_ok());
        let error =
            validate_current_primary_key(&action, Some(&pk(&["id", "tenant_id"])), "app", "items")
                .unwrap_err();
        assert!(error.to_string().contains("order is significant"));
    }

    #[test]
    fn target_requires_existing_not_null_columns_and_an_exact_unique_candidate() {
        let action = AlterPrimaryKeyAction::Replace {
            expected_columns: vec!["id".into()],
            columns: vec!["tenant_id".into(), "item_id".into()],
            drop_identity_from: None,
        };
        let mut columns = BTreeMap::from([
            (
                "tenant_id".into(),
                ColumnFact {
                    not_null: true,
                    identity: false,
                },
            ),
            (
                "item_id".into(),
                ColumnFact {
                    not_null: false,
                    identity: false,
                },
            ),
        ]);

        let nullable = validate_target_columns(
            &action,
            &columns,
            &[unique(20, &["tenant_id", "item_id"])],
            "app",
            "items",
        )
        .unwrap_err();
        assert!(nullable.to_string().contains("is nullable"));

        columns.get_mut("item_id").unwrap().not_null = true;
        let missing = validate_target_columns(&action, &columns, &[], "app", "items").unwrap_err();
        assert!(missing
            .to_string()
            .contains("no exact pre-existing unique candidate"));
        assert!(validate_target_columns(
            &action,
            &columns,
            &[unique(20, &["tenant_id", "item_id"])],
            "app",
            "items",
        )
        .is_ok());
    }

    #[test]
    fn identity_may_remain_in_a_composite_but_must_be_dropped_when_it_leaves_the_key() {
        let mut columns = BTreeMap::new();
        columns.insert(
            "id".into(),
            ColumnFact {
                not_null: true,
                identity: true,
            },
        );
        columns.insert(
            "tenant_id".into(),
            ColumnFact {
                not_null: true,
                identity: false,
            },
        );
        let refused = AlterPrimaryKeyAction::Replace {
            expected_columns: vec!["id".into()],
            columns: vec!["tenant_id".into()],
            drop_identity_from: None,
        };
        assert!(
            validate_identity_transition(&refused, &columns, "app", "items")
                .unwrap_err()
                .to_string()
                .contains("dropIdentityFrom")
        );

        let allowed = AlterPrimaryKeyAction::Replace {
            expected_columns: vec!["id".into()],
            columns: vec!["tenant_id".into()],
            drop_identity_from: Some(vec!["id".into()]),
        };
        assert!(validate_identity_transition(&allowed, &columns, "app", "items").is_ok());

        let composite = AlterPrimaryKeyAction::Replace {
            expected_columns: vec!["id".into()],
            columns: vec!["tenant_id".into(), "id".into()],
            drop_identity_from: None,
        };
        assert!(validate_identity_transition(&composite, &columns, "app", "items").is_ok());

        columns.insert(
            "unrelated_identity".into(),
            ColumnFact {
                not_null: true,
                identity: true,
            },
        );
        assert!(
            validate_identity_transition(&allowed, &columns, "app", "items").is_ok(),
            "an unrelated non-primary identity column is outside this operation's contract"
        );
    }

    #[test]
    fn inbound_fk_requires_alternate_and_must_not_remain_bound_to_old_pk() {
        let action = AlterPrimaryKeyAction::Drop {
            expected_columns: vec!["id".into()],
            drop_identity_from: None,
        };
        let inbound = vec![InboundForeignKey {
            constraint_name: "child_parent_fkey".into(),
            child_schema: "app".into(),
            child_table: "child".into(),
            referenced_index_oid: 10,
            referenced_columns: vec!["id".into()],
        }];
        let alternate = vec![unique(20, &["id"])];
        let error = validate_inbound_foreign_keys(
            &action,
            Some(&pk(&["id"])),
            &alternate,
            &inbound,
            "app",
            "items",
        )
        .unwrap_err();
        assert!(error.to_string().contains("physically bound"));

        let repointed = vec![InboundForeignKey {
            referenced_index_oid: 20,
            ..inbound[0].clone()
        }];
        assert!(validate_inbound_foreign_keys(
            &action,
            Some(&pk(&["id"])),
            &alternate,
            &repointed,
            "app",
            "items",
        )
        .is_ok());
    }

    #[test]
    fn ddl_drops_identity_and_reuses_standalone_candidate_in_one_transactional_batch() {
        let action = AlterPrimaryKeyAction::Replace {
            expected_columns: vec!["id".into()],
            columns: vec!["tenant_id".into(), "item_id".into()],
            drop_identity_from: Some(vec!["id".into()]),
        };
        let ddl = render_ddl(
            &action,
            Some(&pk(&["id"])),
            &[unique(20, &["tenant_id", "item_id"])],
            "\"app\".\"items\"",
        )
        .unwrap();
        assert!(ddl.contains("ALTER COLUMN \"id\" DROP IDENTITY"));
        assert!(ddl.contains("DROP CONSTRAINT \"items_pkey\""));
        assert!(ddl.contains("ADD CONSTRAINT \"items_pkey\" PRIMARY KEY USING INDEX \"unique_20\""));
    }

    #[compio::test]
    async fn apply_keeps_lock_validation_ddl_and_journal_in_one_transaction() {
        let session = RecordingSession::new(&["id"]);
        let step = replacement_step(&["id"]);
        let ran = apply(
            &session,
            &ExecutorConfig::new("project", "app"),
            &step,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
        )
        .await
        .unwrap();
        assert!(ran);

        let log = session.log.borrow();
        let begin = log
            .iter()
            .position(|entry| entry == "batch: BEGIN")
            .unwrap();
        let lock = log
            .iter()
            .position(|entry| entry.contains("LOCK TABLE \"app\".\"items\""))
            .unwrap();
        let ddl = log
            .iter()
            .position(|entry| {
                entry.contains("ALTER COLUMN \"id\" DROP IDENTITY")
                    && entry.contains("PRIMARY KEY USING INDEX \"items_candidate\"")
            })
            .unwrap();
        let journal = log
            .iter()
            .position(|entry| entry.contains("INSERT INTO \"app_migrations\".schema_migrations"))
            .unwrap();
        let commit = log
            .iter()
            .position(|entry| entry == "batch: COMMIT")
            .unwrap();
        assert!(begin < lock && lock < ddl && ddl < journal && journal < commit);
    }

    #[compio::test]
    async fn expected_columns_mismatch_rolls_back_before_any_ddl_or_journal() {
        let session = RecordingSession::new(&["id"]);
        let step = replacement_step(&["tenant_id", "id"]);
        let error = apply(
            &session,
            &ExecutorConfig::new("project", "app"),
            &step,
            Approval::Approved,
            &ApprovalScope::All,
            "tester",
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("order is significant"));

        let log = session.log.borrow();
        assert!(log.iter().any(|entry| entry == "batch: ROLLBACK"));
        assert!(!log.iter().any(|entry| entry.contains("DROP IDENTITY")));
        assert!(!log.iter().any(|entry| entry.contains("INSERT INTO")));
        assert!(!log.iter().any(|entry| entry == "batch: COMMIT"));
    }
}
