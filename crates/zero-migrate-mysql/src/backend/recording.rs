//! The canned [`SqlSession`] the MySQL backend's tests drive, and the catalog rows
//! they feed it.
//!
//! # Why this is a `feature`d module and not `#[cfg(test)]`
//!
//! Two suites need this one double, and they cannot be in the same crate.
//!
//! Most of what it proves is vendor-internal - which SQL the backend emits, in which
//! order, with which binds - and that stays here, as unit tests beside the code.
//! Fourteen of them additionally drive the ENGINE (`apply_with_lock_backend`,
//! `MigrationEngine`, `diff_snapshots`, `fold_ops`) over the same recorder, and those
//! cannot live in a vendor crate at all: `zero-migrate` depends on this crate, so the
//! edge back is a cycle Cargo refuses. They are integration tests OF THE ENGINE
//! driving a MySQL backend, and they live in `zero-migrate/tests/mysql_engine/`.
//!
//! A second copy of the recorder over there would be the real hazard: its canned
//! `information_schema` rows are the shared premise of both suites, and two copies
//! drift silently - one suite would go on asserting against a catalog shape the other
//! had already corrected. So there is ONE recorder, and the engine's test tree reaches
//! it through the `testing` feature, which `zero-migrate` turns on in its
//! `[dev-dependencies]` only. Resolver 3 keeps dev-dependency features out of the
//! normal build, so nothing here is compiled into a shipping `zero-migrate-mysql`.
//!
//! It returns canned rows for the reads the MySQL apply path issues: `GET_LOCK(...)`
//! -> a single `got=1` row (lock acquired); the `information_schema.triggers`
//! existence probe -> empty (so `ensure_journal` creates every trigger); the journal
//! net-state reads -> empty. It is the MySQL analogue of the PG backend's in-crate
//! `RecordingSession` genericity proof.

use std::cell::RefCell;

use zero_migrate_backend::driver::{Bind, DbError, Row, SqlSession, Value};
use zero_migrate_backend::requirements::{DatabaseFeature, DatabaseRequirements};
use zero_migrate_backend::step::BindValue;
use zero_migrate_ir::migration::{Checksum, Migration, MigrationFlags, MigrationId};
use zero_migrate_ir::probe::GuardProbe;

use super::MysqlInflightDdlMarker;

/// The MySQL connection id the canned holder probe reports. Distinct from the
/// `performance_schema` thread id the lock rows carry, because the reply must
/// name the id `KILL` accepts.
pub const HOLDER_CONNECTION_ID: i64 = 113_110;

/// A non-compio, host-shaped [`SqlSession`] that records the SQL + binds of
/// every verb and returns canned rows for the reads the MySQL apply path issues:
/// `GET_LOCK(...)` -> a single `got=1` row (lock acquired); the
/// `information_schema.triggers` existence probe -> empty (so `ensure_journal`
/// creates every trigger); the journal net-state reads -> empty. This is the
/// MySQL analogue of the PG backend's in-crate `RecordingSession` genericity
/// proof.
#[derive(Debug)]
pub struct RecordingSession {
    pub log: RefCell<Vec<String>>,
    pub binds: RefCell<Vec<Vec<Bind>>>,
    pub applied: RefCell<Option<(String, String)>>,
    pub inflight_marker: RefCell<Option<MysqlInflightDdlMarker>>,
    pub table_engine: RefCell<String>,
    pub server_version: String,
    pub default_storage_engine: String,
    pub innodb_support: Option<String>,
    pub global_binlog_format: String,
    pub session_binlog_format: String,
    pub trigger_name: RefCell<Option<String>>,
    pub unique_index_rows: RefCell<Vec<Row>>,
    pub edge_index_rows: RefCell<Vec<Row>>,
    pub binary_journal_collations: bool,
    pub catalog_tables: RefCell<Vec<Row>>,
    pub catalog_columns: RefCell<Vec<Row>>,
    pub catalog_checks: RefCell<Vec<Row>>,
    pub catalog_indexes: RefCell<Vec<Row>>,
    pub catalog_foreign_keys: RefCell<Vec<Row>>,
    pub progress: RefCell<Vec<Row>>,
    pub progress_table_exists: bool,
    pub progress_checksum_exists: bool,
    pub session_in_transaction: i64,
    pub zero_affected_contains: RefCell<Option<String>>,
    pub fail_once_contains: RefCell<Option<String>>,
    /// When false the NON-WAITING `GET_LOCK(?, 0)` answers 0, standing in for a
    /// peer's deploy holding the project lock for the length of its run. The
    /// blocking `GET_LOCK(?, ?)` still answers 1, because that is the one the
    /// journal bootstrap takes and it is a different lock name.
    pub grants_project_lock: bool,
}

impl Default for RecordingSession {
    /// The canned baseline: MySQL 8.0.13 on InnoDB, an empty catalog, an empty
    /// journal, and a project lock that grants.
    ///
    /// Delegating rather than `#[derive]`d, because none of those are the field
    /// types' own defaults - an all-`Default` recorder would report an empty server
    /// version and refuse every capability gate. It exists at all because `new` went
    /// `pub` when the recorder became shared, and a `pub fn new` with no arguments is
    /// a `Default` by any caller's reading.
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingSession {
    pub fn new() -> Self {
        Self {
            log: RefCell::new(Vec::new()),
            binds: RefCell::new(Vec::new()),
            applied: RefCell::new(None),
            inflight_marker: RefCell::new(None),
            table_engine: RefCell::new("InnoDB".to_string()),
            server_version: "8.0.13".to_string(),
            default_storage_engine: "InnoDB".to_string(),
            innodb_support: Some("DEFAULT".to_string()),
            global_binlog_format: "ROW".to_string(),
            session_binlog_format: "ROW".to_string(),
            trigger_name: RefCell::new(None),
            unique_index_rows: RefCell::new(Vec::new()),
            edge_index_rows: RefCell::new(Vec::new()),
            binary_journal_collations: true,
            catalog_tables: RefCell::new(Vec::new()),
            catalog_columns: RefCell::new(Vec::new()),
            catalog_checks: RefCell::new(Vec::new()),
            catalog_indexes: RefCell::new(Vec::new()),
            catalog_foreign_keys: RefCell::new(Vec::new()),
            progress: RefCell::new(Vec::new()),
            progress_table_exists: false,
            progress_checksum_exists: false,
            session_in_transaction: 0,
            zero_affected_contains: RefCell::new(None),
            fail_once_contains: RefCell::new(None),
            grants_project_lock: true,
        }
    }

    /// A session whose non-waiting project-lock acquisition always finds the
    /// lock taken.
    pub fn with_contended_project_lock() -> Self {
        let mut session = Self::new();
        session.grants_project_lock = false;
        session
    }

    pub fn with_table_engine(engine: &str) -> Self {
        let session = Self::new();
        *session.table_engine.borrow_mut() = engine.to_string();
        session
    }

    pub fn with_uuid_capabilities(
        version: &str,
        default_engine: &str,
        innodb_support: Option<&str>,
        global_binlog_format: &str,
        session_binlog_format: &str,
    ) -> Self {
        let mut session = Self::new();
        session.server_version = version.to_string();
        session.default_storage_engine = default_engine.to_string();
        session.innodb_support = innodb_support.map(str::to_string);
        session.global_binlog_format = global_binlog_format.to_string();
        session.session_binlog_format = session_binlog_format.to_string();
        session
    }

    pub fn with_trigger(name: &str) -> Self {
        let session = Self::new();
        *session.trigger_name.borrow_mut() = Some(name.to_string());
        session
    }

    pub fn with_unique_indexes(rows: Vec<Row>) -> Self {
        let session = Self::new();
        *session.unique_index_rows.borrow_mut() = rows;
        session
    }

    pub fn with_edge_index(rows: Vec<Row>) -> Self {
        let session = Self::new();
        *session.edge_index_rows.borrow_mut() = rows;
        session
    }

    pub fn with_legacy_journal_collations() -> Self {
        let mut session = Self::new();
        session.binary_journal_collations = false;
        session
    }

    pub fn with_catalog(
        tables: Vec<Row>,
        columns: Vec<Row>,
        indexes: Vec<Row>,
        foreign_keys: Vec<Row>,
    ) -> Self {
        let session = Self::new();
        *session.catalog_tables.borrow_mut() = tables;
        *session.catalog_columns.borrow_mut() = columns;
        *session.catalog_indexes.borrow_mut() = indexes;
        *session.catalog_foreign_keys.borrow_mut() = foreign_keys;
        session
    }

    pub fn with_catalog_checks(
        tables: Vec<Row>,
        columns: Vec<Row>,
        indexes: Vec<Row>,
        foreign_keys: Vec<Row>,
        checks: Vec<Row>,
    ) -> Self {
        let mut session = Self::with_catalog(tables, columns, indexes, foreign_keys);
        session.server_version = "8.0.16".to_string();
        *session.catalog_checks.borrow_mut() = checks;
        session
    }

    pub fn with_progress(rows: Vec<Row>, checksum_exists: bool) -> Self {
        let mut session = Self::new();
        *session.progress.borrow_mut() = rows;
        session.progress_table_exists = true;
        session.progress_checksum_exists = checksum_exists;
        session
    }

    pub fn with_applied(version: &str, checksum: &Checksum) -> Self {
        let session = Self::new();
        *session.applied.borrow_mut() = Some((version.to_string(), checksum.as_str().to_string()));
        session
    }

    pub fn with_inflight(migration: &Migration, applied_by: &str) -> Self {
        let session = Self::new();
        *session.inflight_marker.borrow_mut() = Some(MysqlInflightDdlMarker {
            version: migration.version.as_str().to_string(),
            name: migration.name.clone(),
            checksum: migration.checksum.as_str().to_string(),
            applied_by: applied_by.to_string(),
            started_at: "2026-07-15 12:00:00.000000".to_string(),
        });
        session
    }

    pub fn with_failure(fragment: &str) -> Self {
        let session = Self::new();
        *session.fail_once_contains.borrow_mut() = Some(fragment.to_string());
        session
    }

    pub fn with_in_transaction(in_transaction: i64) -> Self {
        let mut session = Self::new();
        session.session_in_transaction = in_transaction;
        session
    }

    pub fn with_zero_affected(fragment: &str) -> Self {
        let session = Self::new();
        *session.zero_affected_contains.borrow_mut() = Some(fragment.to_string());
        session
    }

    pub fn fail_if_requested(&self, sql: &str) -> Result<(), DbError> {
        let should_fail = self
            .fail_once_contains
            .borrow()
            .as_deref()
            .is_some_and(|fragment| sql.contains(fragment));
        if should_fail {
            self.fail_once_contains.borrow_mut().take();
            return Err(DbError::message("injected RecordingSession failure"));
        }
        Ok(())
    }

    /// Route a read to its canned rows by SQL shape. `GET_LOCK` returns a
    /// single `got=1` row; everything else (trigger-existence probe, journal
    /// net-state reads) returns empty - enough to drive the whole apply/journal
    /// sweep end-to-end without a live server.
    pub fn rows_for(&self, sql: &str) -> Vec<Row> {
        if sql.contains("GET_LOCK(?, 0)") {
            vec![Row::new(
                vec!["got".to_string()],
                vec![Value::Int(i64::from(self.grants_project_lock))],
            )]
        } else if sql.contains("GET_LOCK") {
            vec![Row::new(vec!["got".to_string()], vec![Value::Int(1)])]
        } else if sql.contains("performance_schema.metadata_locks") {
            vec![Row::new(
                vec![
                    "pid".into(),
                    "account".into(),
                    "state".into(),
                    "stmt".into(),
                ],
                vec![
                    Value::Int(HOLDER_CONNECTION_ID),
                    Value::Text("deployer@10.0.0.7".to_string()),
                    Value::Text("Query: altering table".to_string()),
                    Value::Text("ALTER TABLE widgets ADD COLUMN name VARCHAR(64)".to_string()),
                ],
            )]
        } else if sql.contains("VERSION() AS server_version") {
            vec![Row::new(
                vec![
                    "server_version".into(),
                    "default_storage_engine".into(),
                    "innodb_support".into(),
                    "global_binlog_format".into(),
                    "session_binlog_format".into(),
                ],
                vec![
                    Value::Text(self.server_version.clone()),
                    Value::Text(self.default_storage_engine.clone()),
                    self.innodb_support
                        .as_ref()
                        .map_or(Value::Null, |value| Value::Text(value.clone())),
                    Value::Text(self.global_binlog_format.clone()),
                    Value::Text(self.session_binlog_format.clone()),
                ],
            )]
        } else if sql.contains("@@SESSION.sql_mode") {
            vec![Row::new(
                vec![
                    "sql_mode".into(),
                    "time_zone".into(),
                    "max_execution_time".into(),
                    "innodb_lock_wait_timeout".into(),
                    "information_schema_stats_expiry".into(),
                    "autocommit".into(),
                    "foreign_key_checks".into(),
                    "unique_checks".into(),
                    "transaction_tracking_enabled".into(),
                    "in_transaction".into(),
                ],
                vec![
                    Value::Text("STRICT_TRANS_TABLES".into()),
                    Value::Text("SYSTEM".into()),
                    Value::Int(0),
                    Value::Int(50),
                    Value::Int(86_400),
                    Value::Int(0),
                    Value::Int(0),
                    Value::Int(0),
                    Value::Int(1),
                    Value::Int(self.session_in_transaction),
                ],
            )]
        } else if sql.contains("performance_schema.events_transactions_current") {
            vec![Row::new(
                vec![
                    "transaction_tracking_enabled".into(),
                    "in_transaction".into(),
                ],
                vec![Value::Int(1), Value::Int(self.session_in_transaction)],
            )]
        } else if sql.contains("schema_migrations_inflight") && sql.contains("FOR UPDATE") {
            self.inflight_marker
                .borrow()
                .as_ref()
                .map_or_else(Vec::new, |marker| {
                    vec![Row::new(
                        vec![
                            "version".into(),
                            "name".into(),
                            "checksum".into(),
                            "applied_by".into(),
                            "started_at".into(),
                        ],
                        vec![
                            Value::Text(marker.version.clone()),
                            Value::Text(marker.name.clone()),
                            Value::Text(marker.checksum.clone()),
                            Value::Text(marker.applied_by.clone()),
                            Value::Text(marker.started_at.clone()),
                        ],
                    )]
                })
        } else if sql.contains("WITH ranked AS") {
            self.applied
                .borrow()
                .as_ref()
                .map_or_else(Vec::new, |(version, checksum)| {
                    vec![Row::new(
                        vec![
                            "version".into(),
                            "checksum".into(),
                            "mig_kind".into(),
                            "event_seq".into(),
                            "phase".into(),
                            // The applied read selects the stored reverse now,
                            // so a canned row without it cannot be parsed.
                            "down".into(),
                        ],
                        vec![
                            Value::Text(version.clone()),
                            Value::Text(checksum.clone()),
                            Value::Text("apply".into()),
                            Value::Int(1),
                            Value::Text("completed".into()),
                            Value::Null,
                        ],
                    )]
                })
        } else if sql.contains("AS table_exists") && sql.contains("schema_backfills") {
            vec![Row::new(
                vec!["table_exists".into(), "checksum_exists".into()],
                vec![
                    Value::Int(i64::from(self.progress_table_exists)),
                    Value::Int(i64::from(self.progress_checksum_exists)),
                ],
            )]
        } else if sql.contains("schema_backfills") && sql.contains("AS checksum") {
            self.progress.borrow().clone()
        } else if sql.contains("information_schema.TRIGGERS") {
            self.trigger_name
                .borrow()
                .as_ref()
                .map_or_else(Vec::new, |name| {
                    vec![Row::new(
                        vec!["trigger_name".into()],
                        vec![Value::Text(name.clone())],
                    )]
                })
        } else if sql.contains("TABLE_TYPE = 'BASE TABLE'") {
            self.catalog_tables.borrow().clone()
        } else if sql.contains("COLUMN_TYPE AS column_type")
            && sql.contains("ORDINAL_POSITION AS ordinal_position")
        {
            self.catalog_columns.borrow().clone()
        } else if sql.contains("information_schema.CHECK_CONSTRAINTS")
            && sql.contains("tc.ENFORCED AS enforced")
        {
            self.catalog_checks.borrow().clone()
        } else if sql.contains("EXPRESSION AS expression")
            && sql.contains("ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX")
        {
            self.catalog_indexes.borrow().clone()
        } else if sql.contains("information_schema.REFERENTIAL_CONSTRAINTS")
            && sql.contains("POSITION_IN_UNIQUE_CONSTRAINT")
        {
            self.catalog_foreign_keys.borrow().clone()
        } else if sql.contains("COLLATION_NAME AS collation_name")
            && sql.contains("schema_migrations_inflight")
        {
            let collation = if self.binary_journal_collations {
                "utf8mb4_bin"
            } else {
                "utf8mb4_0900_ai_ci"
            };
            [
                ("schema_migrations", "version"),
                ("schema_migrations", "checksum"),
                ("schema_migrations_supersedes", "squash_version"),
                ("schema_migrations_supersedes", "superseded_version"),
                ("schema_migrations_inflight", "version"),
                ("schema_migrations_inflight", "checksum"),
                ("schema_migrations_rollback_inflight", "version"),
                ("schema_migrations_rollback_inflight", "checksum"),
                ("schema_migrations_recovery", "version"),
                ("schema_migrations_recovery", "checksum"),
            ]
            .into_iter()
            .map(|(table, column)| {
                Row::new(
                    vec![
                        "table_name".into(),
                        "column_name".into(),
                        "character_set_name".into(),
                        "collation_name".into(),
                    ],
                    vec![
                        Value::Text(table.into()),
                        Value::Text(column.into()),
                        Value::Text("utf8mb4".into()),
                        Value::Text(collation.into()),
                    ],
                )
            })
            .collect()
        } else if sql.contains("information_schema.STATISTICS")
            && sql.contains("schema_migrations_supersedes_edge_uq")
        {
            self.edge_index_rows.borrow().clone()
        } else if sql.contains("information_schema.STATISTICS") {
            self.unique_index_rows.borrow().clone()
        } else if sql.contains("information_schema.TABLES") {
            vec![Row::new(
                vec!["table_engine".into()],
                vec![Value::Text(self.table_engine.borrow().clone())],
            )]
        } else {
            Vec::new()
        }
    }
}

impl SqlSession for RecordingSession {
    async fn batch(&self, sql: &str) -> Result<(), DbError> {
        self.log.borrow_mut().push(format!("batch: {sql}"));
        self.fail_if_requested(sql)?;
        Ok(())
    }
    async fn exec(&self, sql: &str, params: &[Bind]) -> Result<u64, DbError> {
        self.log.borrow_mut().push(format!("exec: {sql}"));
        self.binds.borrow_mut().push(params.to_vec());
        self.fail_if_requested(sql)?;
        Ok(u64::from(
            !self
                .zero_affected_contains
                .borrow()
                .as_deref()
                .is_some_and(|fragment| sql.contains(fragment)),
        ))
    }
    async fn query(&self, sql: &str, params: &[Bind]) -> Result<Vec<Row>, DbError> {
        self.log.borrow_mut().push(format!("query: {sql}"));
        self.binds.borrow_mut().push(params.to_vec());
        self.fail_if_requested(sql)?;
        Ok(self.rows_for(sql))
    }
    async fn query_one(&self, sql: &str, params: &[Bind]) -> Result<Row, DbError> {
        self.log.borrow_mut().push(format!("query_one: {sql}"));
        self.binds.borrow_mut().push(params.to_vec());
        self.fail_if_requested(sql)?;
        self.rows_for(sql)
            .into_iter()
            .next()
            .ok_or_else(|| DbError::message("query_one: no canned row"))
    }
}

pub fn trivial_migration() -> Migration {
    let flags = MigrationFlags::default();
    let version = MigrationId::generate();
    let checksum = Checksum::of(&zero_migrate_ir::migration::ChecksumInput {
        up: "CREATE TABLE t (id INT)",
        down: Some("DROP TABLE t"),
        flags: &flags,
        owner_app: "app_test",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    Migration {
        version,
        name: "create_t".into(),
        up: "CREATE TABLE t (id INT)".into(),
        down: Some("DROP TABLE t".into()),
        checksum,
        flags,
        owner_app: "app_test".into(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
        effect: None,
    }
}

pub fn guarded_migration(up: &str, probe: GuardProbe) -> Migration {
    let mut migration = trivial_migration();
    migration.name = "guarded object".into();
    migration.up = up.into();
    migration.down = None;
    migration.checksum = Checksum::of(&zero_migrate_ir::migration::ChecksumInput {
        up: &migration.up,
        down: migration.down.as_deref(),
        flags: &migration.flags,
        owner_app: &migration.owner_app,
        depends_on: &migration.depends_on,
        supersedes: &migration.supersedes,
        preconditions: &migration.preconditions,
    });
    migration.existence_guard = Some(probe);
    migration
}

pub fn catalog_table(table: &str) -> Row {
    Row::new(vec!["table_name".into()], vec![Value::Text(table.into())])
}

pub fn step_checksum(label: &str) -> Checksum {
    Checksum::of(&zero_migrate_ir::migration::ChecksumInput {
        up: label,
        down: None,
        flags: &MigrationFlags::default(),
        owner_app: "app_test",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    })
}

pub fn unique_index_part(index: &str, column: Option<&str>, sub_part: Option<i64>) -> Row {
    Row::new(
        vec!["index_name".into(), "column_name".into(), "sub_part".into()],
        vec![
            Value::Text(index.into()),
            column.map_or(Value::Null, |value| Value::Text(value.into())),
            sub_part.map_or(Value::Null, Value::Int),
        ],
    )
}

pub fn edge_index_part(
    non_unique: i64,
    sequence: i64,
    column: Option<&str>,
    sub_part: Option<i64>,
) -> Row {
    Row::new(
        vec![
            "non_unique".into(),
            "seq_in_index".into(),
            "column_name".into(),
            "sub_part".into(),
        ],
        vec![
            Value::Int(non_unique),
            Value::Int(sequence),
            column.map_or(Value::Null, |value| Value::Text(value.into())),
            sub_part.map_or(Value::Null, Value::Int),
        ],
    )
}

pub fn catalog_column(
    table: &str,
    column: &str,
    column_type: &str,
    character_set: Option<&str>,
    collation: Option<&str>,
    nullable: bool,
    ordinal: i64,
) -> Row {
    catalog_column_with_generation(
        table,
        column,
        column_type,
        character_set,
        collation,
        nullable,
        ordinal,
        None,
        "",
    )
}

#[allow(clippy::too_many_arguments)]
pub fn catalog_column_with_generation(
    table: &str,
    column: &str,
    column_type: &str,
    character_set: Option<&str>,
    collation: Option<&str>,
    nullable: bool,
    ordinal: i64,
    default: Option<&str>,
    extra: &str,
) -> Row {
    Row::new(
        vec![
            "table_name".into(),
            "column_name".into(),
            "column_type".into(),
            "character_set_name".into(),
            "collation_name".into(),
            "is_nullable".into(),
            "column_default".into(),
            "extra".into(),
            "ordinal_position".into(),
        ],
        vec![
            Value::Text(table.into()),
            Value::Text(column.into()),
            Value::Text(column_type.into()),
            character_set.map_or(Value::Null, |value| Value::Text(value.into())),
            collation.map_or(Value::Null, |value| Value::Text(value.into())),
            Value::Text(if nullable { "YES" } else { "NO" }.into()),
            default.map_or(Value::Null, |value| Value::Text(value.into())),
            Value::Text(extra.into()),
            Value::Int(ordinal),
        ],
    )
}

pub fn catalog_check(table: &str, constraint: &str, enforced: bool, check_clause: &str) -> Row {
    Row::new(
        vec![
            "table_name".into(),
            "constraint_name".into(),
            "enforced".into(),
            "check_clause".into(),
        ],
        vec![
            Value::Text(table.into()),
            Value::Text(constraint.into()),
            Value::Text(if enforced { "YES" } else { "NO" }.into()),
            Value::Text(check_clause.into()),
        ],
    )
}

pub const MYSQL_CATALOG_UUID_V4_DEFAULT: &str = "lower(concat(hex(random_bytes(4)),_latin1'-',hex(random_bytes(2)),_latin1'-',hex(((ord(random_bytes(1)) & 15) | 64)),hex(random_bytes(1)),_latin1'-',hex(((ord(random_bytes(1)) & 63) | 128)),hex(random_bytes(1)),_latin1'-',hex(random_bytes(6))))";

pub fn id_catalog_columns(
    generated_uuid_default: Option<&str>,
    supplied_uuid_default: Option<&str>,
    type_id_default: Option<&str>,
    ulid_default: Option<&str>,
) -> Vec<Row> {
    id_catalog_columns_with_generated_uuid_extra(
        generated_uuid_default,
        supplied_uuid_default,
        type_id_default,
        ulid_default,
        "DEFAULT_GENERATED",
    )
}

pub fn id_catalog_columns_with_generated_uuid_extra(
    generated_uuid_default: Option<&str>,
    supplied_uuid_default: Option<&str>,
    type_id_default: Option<&str>,
    ulid_default: Option<&str>,
    generated_uuid_extra: &str,
) -> Vec<Row> {
    vec![
        catalog_column_with_generation(
            "ids",
            "auto_id",
            "bigint",
            None,
            None,
            false,
            1,
            None,
            "auto_increment",
        ),
        catalog_column_with_generation(
            "ids",
            "generated_uuid",
            "varchar(36)",
            Some("ascii"),
            Some("ascii_bin"),
            false,
            2,
            generated_uuid_default,
            generated_uuid_extra,
        ),
        catalog_column_with_generation(
            "ids",
            "supplied_uuid",
            "varchar(36)",
            Some("ascii"),
            Some("ascii_bin"),
            false,
            3,
            supplied_uuid_default,
            supplied_uuid_default.map_or("", |_| "DEFAULT_GENERATED"),
        ),
        catalog_column_with_generation(
            "ids",
            "type_id",
            "varchar(191)",
            Some("ascii"),
            Some("ascii_bin"),
            false,
            4,
            type_id_default,
            type_id_default.map_or("", |_| "DEFAULT_GENERATED"),
        ),
        catalog_column_with_generation(
            "ids",
            "ulid",
            "varchar(191)",
            Some("ascii"),
            Some("ascii_bin"),
            false,
            5,
            ulid_default,
            ulid_default.map_or("", |_| "DEFAULT_GENERATED"),
        ),
        // An ordinary text default that happens to call uuid() must remain
        // outside the narrow ID-default comparison surface without an
        // engine-owned UUID format CHECK.
        //
        // `text`, NOT `varchar(191)`, because that is what the engine deploys for
        // a plain `ColType::Text` column - measured against a live MySQL server by
        // `tests/drift_column_physical_type.rs`, whose `body` column authors `text`
        // and introspects back as `Lob { tier: "text" }`. The `varchar(191)`
        // spelling belongs to the two columns ABOVE, which carry a `value_format`
        // and so need an indexable width. This row read `varchar(191)` until the
        // drift report learned to print the physical contract: the comparator had
        // always answered "different" here, and the report dropped the answer
        // because `mysql_canonical_type` folds `varchar(191)` to the same literal
        // `text` the fold emits.
        catalog_column_with_generation(
            "ids",
            "ordinary",
            "text",
            Some("utf8mb4"),
            Some("utf8mb4_bin"),
            false,
            6,
            Some("uuid()"),
            "DEFAULT_GENERATED",
        ),
    ]
}

pub fn catalog_index_part(
    table: &str,
    index: &str,
    non_unique: i64,
    sequence: i64,
    column: Option<&str>,
    prefix: Option<i64>,
    collation: Option<&str>,
    expression: Option<&str>,
) -> Row {
    Row::new(
        vec![
            "table_name".into(),
            "index_name".into(),
            "non_unique".into(),
            "seq_in_index".into(),
            "column_name".into(),
            "sub_part".into(),
            "index_collation".into(),
            "index_type".into(),
            "expression".into(),
        ],
        vec![
            Value::Text(table.into()),
            Value::Text(index.into()),
            Value::Int(non_unique),
            Value::Int(sequence),
            column.map_or(Value::Null, |value| Value::Text(value.into())),
            prefix.map_or(Value::Null, Value::Int),
            collation.map_or(Value::Null, |value| Value::Text(value.into())),
            Value::Text("BTREE".into()),
            expression.map_or(Value::Null, |value| Value::Text(value.into())),
        ],
    )
}

#[allow(clippy::too_many_arguments)]
pub fn catalog_foreign_key_part(
    table: &str,
    constraint: &str,
    ordinal: i64,
    unique_position: i64,
    column: &str,
    referenced_schema: &str,
    referenced_table: &str,
    referenced_column: &str,
    update_rule: &str,
    delete_rule: &str,
) -> Row {
    Row::new(
        vec![
            "table_name".into(),
            "constraint_name".into(),
            "ordinal_position".into(),
            "position_in_unique_constraint".into(),
            "column_name".into(),
            "referenced_table_schema".into(),
            "referenced_table_name".into(),
            "referenced_column_name".into(),
            "update_rule".into(),
            "delete_rule".into(),
        ],
        vec![
            Value::Text(table.into()),
            Value::Text(constraint.into()),
            Value::Int(ordinal),
            Value::Int(unique_position),
            Value::Text(column.into()),
            Value::Text(referenced_schema.into()),
            Value::Text(referenced_table.into()),
            Value::Text(referenced_column.into()),
            Value::Text(update_rule.into()),
            Value::Text(delete_rule.into()),
        ],
    )
}

pub fn plan_dml_step(
    label: &str,
    destructive: bool,
) -> (zero_migrate_backend::step::PlanStep, MigrationId) {
    let version = MigrationId::generate();
    let template = if destructive {
        "DELETE FROM `proj_x`.`users` WHERE `id` = ?"
    } else {
        "INSERT INTO `proj_x`.`users` (`id`) VALUES (?)"
    };
    (
        zero_migrate_backend::step::PlanStep::Dml {
            version: version.clone(),
            checksum: step_checksum(label),
            name: label.into(),
            template: template.into(),
            binds: vec![BindValue::Int(7)],
            target_schema: "proj_x".into(),
            target_table: "users".into(),
            conflict_target: None,
            mutates_data: true,
            transactional: true,
            destructive,
            requires_approval: destructive,
            owner_app: "app_test".into(),
        },
        version,
    )
}

pub fn requirements(feature: DatabaseFeature) -> DatabaseRequirements {
    let mut requirements = DatabaseRequirements::default();
    requirements.require(feature);
    requirements
}
