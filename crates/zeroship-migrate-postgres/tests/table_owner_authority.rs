mod support;

use zeroship_migrate_backend::guard::GuardConfig;
use zeroship_migrate_postgres::{guard::SqlGuard, DIALECT};

#[test]
fn table_owner_transfer_requires_explicit_role_authority_and_schema_scope() {
    let confined = SqlGuard::new(GuardConfig::from_policy(
        support::no_inject("workflow"),
        DIALECT,
        "workflow",
    ));
    let platform = SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(
            r#"
policy_version = 1
[[grant]]
key = "access.role"
value = true
scope = "all"
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["workflow"] }
[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#,
        ),
        DIALECT,
        "workflow",
    ));
    let transfer = "ALTER TABLE workflow.runs OWNER TO zeroship_workflow_migrator";
    assert!(confined.check(transfer).is_err());
    platform.check(transfer).unwrap();
    platform.check_raw_island_sql_backstop(transfer).unwrap();
    for sql in [
        "ALTER TABLE other.runs OWNER TO zeroship_workflow_migrator",
        "ALTER TABLE workflow.runs OWNER TO pg_read_server_files",
        "ALTER TABLE workflow.runs OWNER TO pg_write_server_files",
        "ALTER TABLE workflow.runs OWNER TO postgres",
        "ALTER TABLE workflow.runs OWNER TO CURRENT_USER",
        "ALTER TABLE workflow.runs OWNER TO SESSION_USER",
        "ALTER TABLE workflow.runs OWNER TO zeroship_workflow_migrator, DISABLE TRIGGER ALL",
        "ALTER TABLE workflow.runs INHERIT workflow.parent",
        "SET ROLE zeroship_workflow_migrator",
    ] {
        assert!(platform.check(sql).is_err(), "{sql}");
    }
}
