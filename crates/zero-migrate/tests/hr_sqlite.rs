//! Applies the JS-authored HR migration set, captured as preview IR, to real SQLite.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::executor::LockMode;
use zero_migrate::{
    confined_no_inject_policy, fold_to_field_defs, resolve_create_table_policy, Approval,
    ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, MigrationEngine, MigrationIr, Op,
    SqlDialect, SqliteBackend,
};

const PROJECT: &str = "app_hr";
const APP: &str = "app_hr";
const HR_PREVIEW_IR: &str = include_str!("fixtures/hr/migrations.json");
const HR_TABLES: [&str; 8] = [
    "departments",
    "job_grades",
    "employees",
    "positions",
    "employee_position_history",
    "payroll_runs",
    "payroll_items",
    "leave_requests",
];
const MIGRATION_NAMES: [&str; 16] = [
    "create_departments",
    "create_job_grades",
    "create_employees",
    "add_work_location",
    "create_positions",
    "create_position_history",
    "create_payroll",
    "create_leave_requests",
    "add_indexes",
    "seed_reference_data",
    "seed_employees",
    "seed_operations",
    "backfill_full_name",
    "adjust_compensation",
    "add_audit_columns",
    "rename_salary_column",
];

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths() -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join("hr.sqlite");
    let journal = dir.path().join("hr.migrations.sqlite");
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

fn registry() -> BTreeMap<String, String> {
    HR_TABLES
        .iter()
        .map(|table| ((*table).to_string(), APP.to_string()))
        .collect()
}

fn expected_rows(rows: &[&[&str]]) -> Vec<Vec<Option<String>>> {
    rows.iter()
        .map(|row| row.iter().map(|value| Some((*value).to_string())).collect())
        .collect()
}

#[compio::test]
async fn hr_migrations_apply_in_sequence_on_real_sqlite() {
    let paths = paths();
    let backend =
        SqliteBackend::open(&paths.app, &paths.journal).expect("open hardened SQLite backend");
    let exec_cfg = ExecutorConfig::new(PROJECT, PROJECT);
    let registry = registry();
    let guard = GuardConfig::confined_sqlite(PROJECT.to_string());
    let no_inject = confined_no_inject_policy(PROJECT).expect("author-owned table policy");
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &no_inject);
    let engine = MigrationEngine::new();

    let envelopes: Vec<MigrationIr> =
        serde_json::from_str(HR_PREVIEW_IR).expect("preview fixture is valid migration IR");
    assert_eq!(envelopes.len(), 16, "all HR preview envelopes are captured");
    assert!(
        envelopes.iter().all(|envelope| envelope.ir_version == 1),
        "every captured envelope uses IR v1"
    );
    assert_eq!(
        envelopes
            .iter()
            .map(|envelope| envelope.name.as_str())
            .collect::<Vec<_>>(),
        MIGRATION_NAMES,
        "preview envelopes remain in filename order"
    );

    let mut live = LiveSchema::default();
    let mut cumulative_ops: Vec<Op> = Vec::new();

    for envelope in envelopes {
        let migration_name = envelope.name.clone();
        let resolved = resolve_create_table_policy(&envelope, &no_inject, PROJECT)
            .unwrap_or_else(|error| panic!("resolve {migration_name}: {error}"));
        let ir_json = serde_json::to_string(&resolved)
            .unwrap_or_else(|error| panic!("serialize {migration_name}: {error}"));

        // The guarded/full-plan path is required here: the flat load_and_lower API
        // projects away DML, backfills, and SQLite rebuild rename steps.
        let artifact = author
            .load_and_lower_guarded(&ir_json, APP, &registry, &live, &guard)
            .unwrap_or_else(|error| panic!("lower {migration_name}: {error}"));
        let outcome = engine
            .apply_applied_plan_with_touched_and_depends(
                &artifact.plan,
                &artifact.touched_tables,
                &artifact.depends_on,
                Approval::Approved,
                &backend,
                &exec_cfg,
                "deploy",
                LockMode::Acquire,
            )
            .await
            .unwrap_or_else(|error| panic!("apply {migration_name}: {error}"));
        assert!(
            !outcome.applied.applied.is_empty(),
            "{migration_name} must apply at least one plan step"
        );
        println!("applied {migration_name}: {:?}", outcome.applied.applied);

        // Logical value-format contracts are authored facts and cannot be recovered
        // from SQLite affinity. Preserve them while refreshing the physical schema
        // from the database after every migration.
        live.advance_logical_columns(&resolved, SqlDialect::Sqlite, PROJECT, None)
            .unwrap_or_else(|error| {
                panic!("advance logical schema after {migration_name}: {error}")
            });
        cumulative_ops.extend(resolved.ops.iter().cloned());
        let snapshot = backend
            .snapshot_schema_sqlite()
            .await
            .unwrap_or_else(|error| panic!("snapshot after {migration_name}: {error}"));
        let logical_columns = live.logical_columns.clone();
        live = LiveSchema::from_catalog_snapshot(snapshot, APP);
        live.logical_columns = logical_columns;

        // SQLite rename rebuilds require the lossless SDK-shaped column facets in
        // addition to the catalog snapshot. Recover that map from the exact authored
        // IR accumulated so far; the SQLite catalog alone cannot represent it.
        live.sqlite_schemas =
            fold_to_field_defs(&cumulative_ops, SqlDialect::Sqlite, PROJECT, &no_inject)
                .unwrap_or_else(|error| {
                    panic!("fold SQLite schema after {migration_name}: {error}")
                });
    }

    let table_rows = backend
        .actor()
        .query(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name IN (\
               'departments', 'job_grades', 'employees', 'positions',\
               'employee_position_history', 'payroll_runs', 'payroll_items',\
               'leave_requests'\
             ) ORDER BY name",
        )
        .await
        .expect("query HR tables");
    println!("tables: {table_rows:?}");
    assert_eq!(
        table_rows,
        expected_rows(&[
            &["departments"],
            &["employee_position_history"],
            &["employees"],
            &["job_grades"],
            &["leave_requests"],
            &["payroll_items"],
            &["payroll_runs"],
            &["positions"],
        ]),
        "all eight HR tables exist"
    );

    let employee_column_rows = backend
        .actor()
        .query("PRAGMA main.table_info('employees')")
        .await
        .expect("query employees columns");
    let employee_columns: BTreeSet<String> = employee_column_rows
        .iter()
        .map(|row| row[1].clone().expect("employees column name"))
        .collect();
    println!("employees columns: {employee_columns:?}");
    assert_eq!(
        employee_columns,
        [
            "id",
            "dept_id",
            "grade_id",
            "email",
            "first_name",
            "last_name",
            "hire_date",
            "annual_base_salary",
            "employment_type",
            "status",
            "manager_id",
            "created_at",
            "work_location",
            "full_name",
            "updated_at",
        ]
        .into_iter()
        .map(str::to_string)
        .collect(),
        "the rename and all additive employee columns are present with no base_salary"
    );
    assert!(!employee_columns.contains("base_salary"));

    let employee_snapshot = &live.table_snapshots["employees"];
    let annual_salary = employee_snapshot
        .columns
        .iter()
        .find(|column| column.name == "annual_base_salary")
        .expect("renamed salary column is introspected");
    assert_eq!(
        annual_salary.data_type, "text",
        "a pure rename preserves SQLite's exact-decimal TEXT storage contract"
    );
    let employee_id = employee_snapshot
        .columns
        .iter()
        .find(|column| column.name == "id")
        .expect("employees.id is introspected");
    assert!(
        employee_id.value_format.is_some(),
        "the TypeID CHECK survives the rename rebuild and remains introspectable"
    );
    assert!(
        employee_snapshot.constraints.iter().any(|constraint| {
            constraint.kind == "PRIMARY KEY" && constraint.definition == "PRIMARY KEY (id)"
        }),
        "the authored employees primary key survives the rename rebuild"
    );
    let employee_foreign_keys = employee_snapshot
        .constraints
        .iter()
        .filter(|constraint| constraint.kind == "FOREIGN KEY")
        .map(|constraint| (constraint.name.clone(), constraint.definition.clone()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        employee_foreign_keys,
        BTreeMap::from([
            (
                "employees_dept_id_fkey".to_string(),
                "FOREIGN KEY (dept_id) REFERENCES departments(id) ON DELETE RESTRICT".to_string(),
            ),
            (
                "employees_grade_id_fkey".to_string(),
                "FOREIGN KEY (grade_id) REFERENCES job_grades(id) ON DELETE RESTRICT".to_string(),
            ),
            (
                "employees_manager_id_fkey".to_string(),
                "FOREIGN KEY (manager_id) REFERENCES employees(id) ON DELETE SET NULL".to_string(),
            ),
        ]),
        "all external and self-referential employee FKs survive the rename rebuild"
    );

    let employee_rows = backend
        .actor()
        .query(
            "SELECT email, full_name, status, COALESCE(manager_id, ''), \
                    printf('%.2f', annual_base_salary) \
             FROM employees ORDER BY email",
        )
        .await
        .expect("query employees");
    println!("employees: {employee_rows:?}");
    assert_eq!(
        employee_rows,
        expected_rows(&[
            &["ada@example.com", "Ada Lovelace", "active", "", "185000.00"],
            &[
                "alan@example.com",
                "Alan Turing",
                "active",
                "emp_01arz3ndektsv4rrffq69g6001",
                "120000.00",
            ],
            &[
                "barbara@example.com",
                "Barbara Liskov",
                "active",
                "emp_01arz3ndektsv4rrffq69g6001",
                "175000.00",
            ],
            &[
                "edsger@example.com",
                "Edsger Dijkstra",
                "active",
                "",
                "72000.00",
            ],
            &[
                "grace@example.com",
                "Grace Hopper",
                "terminated",
                "",
                "115000.00",
            ],
        ]),
        "five employees survive the rename with names, status update, managers, and salaries"
    );

    let leave_count_rows = backend
        .actor()
        .query(
            "SELECT COUNT(*), \
                    SUM(CASE WHEN status = 'rejected' THEN 1 ELSE 0 END) \
             FROM leave_requests",
        )
        .await
        .expect("query leave counts");
    assert_eq!(
        leave_count_rows,
        expected_rows(&[&["3", "0"]]),
        "three leave requests remain and rejected requests were deleted"
    );
    let leave_rows = backend
        .actor()
        .query(
            "SELECT employees.email, leave_requests.leave_type, \
                    leave_requests.status, printf('%.1f', leave_requests.days) \
             FROM leave_requests \
             JOIN employees ON employees.id = leave_requests.employee_id \
             ORDER BY employees.email",
        )
        .await
        .expect("query remaining leave requests");
    println!("leave requests: {leave_rows:?}");
    assert_eq!(
        leave_rows,
        expected_rows(&[
            &["alan@example.com", "vacation", "approved", "5.0"],
            &["edsger@example.com", "personal", "pending", "0.5"],
            &["grace@example.com", "sick", "approved", "1.0"],
        ])
    );

    let payroll_create_sql = live.table_snapshots["payroll_items"]
        .stored_create_sql
        .as_deref()
        .expect("payroll_items stored CREATE SQL");
    assert!(
        payroll_create_sql.contains("\"net_pay\"")
            && payroll_create_sql.contains("GENERATED ALWAYS AS")
            && payroll_create_sql.contains(" STORED"),
        "SQLite stores net_pay as a STORED generated column: {payroll_create_sql}"
    );
    let payroll_rows = backend
        .actor()
        .query(
            "SELECT printf('%.2f', gross_pay), printf('%.2f', tax), \
                    printf('%.2f', net_pay) \
             FROM payroll_items \
             WHERE employee_id = 'emp_01arz3ndektsv4rrffq69g6001'",
        )
        .await
        .expect("query Ada payroll");
    println!("Ada payroll: {payroll_rows:?}");
    assert_eq!(
        payroll_rows,
        expected_rows(&[&["15416.67", "5395.83", "10020.84"]]),
        "net_pay is gross_pay - tax"
    );

    let history_pk_rows = backend
        .actor()
        .query("PRAGMA main.table_info('employee_position_history')")
        .await
        .expect("query position-history primary key");
    let mut history_pk_rows = history_pk_rows
        .into_iter()
        .filter(|row| row[5].as_deref() != Some("0"))
        .map(|row| vec![row[1].clone(), row[5].clone()])
        .collect::<Vec<_>>();
    history_pk_rows.sort_by_key(|row| {
        row[1]
            .as_deref()
            .expect("primary-key ordinal")
            .parse::<u32>()
            .expect("numeric primary-key ordinal")
    });
    assert_eq!(
        history_pk_rows,
        expected_rows(&[&["employee_id", "1"], &["effective_from", "2"]]),
        "position history has the authored composite primary key"
    );
    let history_rows = backend
        .actor()
        .query(
            "SELECT employee_id, effective_from, position_id, COALESCE(effective_to, '') \
             FROM employee_position_history \
             WHERE employee_id = 'emp_01arz3ndektsv4rrffq69g6001' \
               AND effective_from = '2019-03-01'",
        )
        .await
        .expect("query composite-PK position-history row");
    println!("Ada position history: {history_rows:?}");
    assert_eq!(
        history_rows,
        expected_rows(&[&[
            "emp_01arz3ndektsv4rrffq69g6001",
            "2019-03-01",
            "01ARZ3NDEKTSV4RRFFQ69G7001",
            "",
        ]])
    );

    let manager_fk_rows = backend
        .actor()
        .query("PRAGMA main.foreign_key_list('employees')")
        .await
        .expect("query employees.manager_id foreign key");
    let manager_fk_rows = manager_fk_rows
        .into_iter()
        .filter(|row| row[3].as_deref() == Some("manager_id"))
        .map(|row| {
            vec![
                row[2].clone(),
                row[3].clone(),
                row[4].clone(),
                row[6].clone(),
            ]
        })
        .collect::<Vec<_>>();
    assert_eq!(
        manager_fk_rows,
        expected_rows(&[&["employees", "manager_id", "id", "SET NULL"]]),
        "manager_id remains a self-referential foreign key after the rebuild"
    );
    let manager_rows = backend
        .actor()
        .query(
            "SELECT email, manager_id FROM employees \
             WHERE manager_id IS NOT NULL ORDER BY email",
        )
        .await
        .expect("query employee managers");
    println!("employee managers: {manager_rows:?}");
    assert_eq!(
        manager_rows,
        expected_rows(&[
            &["alan@example.com", "emp_01arz3ndektsv4rrffq69g6001",],
            &["barbara@example.com", "emp_01arz3ndektsv4rrffq69g6001",],
        ]),
        "Alan and Barbara report to Ada"
    );

    let employee_index_rows = backend
        .actor()
        .query("PRAGMA main.index_list('employees')")
        .await
        .expect("query employee indexes after rename rebuild");
    let mut employee_index_rows = employee_index_rows
        .into_iter()
        .filter(|row| {
            matches!(
                row[1].as_deref(),
                Some("ix_employees_dept_status" | "uq_employees_email")
            )
        })
        .map(|row| vec![row[1].clone(), row[2].clone()])
        .collect::<Vec<_>>();
    employee_index_rows.sort_by(|left, right| left[0].cmp(&right[0]));
    assert_eq!(
        employee_index_rows,
        expected_rows(&[
            &["ix_employees_dept_status", "0"],
            &["uq_employees_email", "1"],
        ]),
        "the SQLite rename rebuild preserves employee indexes"
    );

    let foreign_key_violations = backend
        .actor()
        .query("PRAGMA main.foreign_key_check")
        .await
        .expect("check all HR foreign keys");
    assert!(
        foreign_key_violations.is_empty(),
        "the completed HR schema has no dangling foreign keys: {foreign_key_violations:?}"
    );
}
