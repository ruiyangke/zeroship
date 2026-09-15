use super::*;
use crate::sql::coordination::{
    AdvisoryKey, AdvisoryLock, AdvisoryLockAction, AdvisoryLockScope, SetTransactionSetting,
    SettingName,
};
use crate::sql::statement::Statement;
use crate::value::Value;

fn lock(key: AdvisoryKey, scope: AdvisoryLockScope, action: AdvisoryLockAction) -> Statement {
    Statement::AdvisoryLock(AdvisoryLock::new(key, scope, action).unwrap())
}

#[test]
fn postgres_renders_every_advisory_key_form_with_database_side_hashing() {
    use AdvisoryLockAction::{Release, Try, Wait};
    use AdvisoryLockScope::{Session, Transaction};
    for (statement, sql, params) in [
        (
            lock(AdvisoryKey::single(7), Transaction, Wait),
            "SELECT pg_advisory_xact_lock($1::int8)",
            vec![Value::from(7_i64)],
        ),
        (
            lock(AdvisoryKey::pair(3, 4), Transaction, Wait),
            "SELECT pg_advisory_xact_lock($1::int4, $2::int4)",
            vec![Value::from(3_i64), Value::from(4_i64)],
        ),
        (
            lock(AdvisoryKey::hashed_pair(3, "Subject"), Transaction, Wait),
            "SELECT pg_advisory_xact_lock($1::int4, hashtext($2::text))",
            vec![Value::from(3_i64), Value::from("Subject")],
        ),
        (
            lock(AdvisoryKey::hashed("Family"), Transaction, Wait),
            "SELECT pg_advisory_xact_lock(hashtext($1::text)::int8)",
            vec![Value::from("Family")],
        ),
        (
            lock(AdvisoryKey::hashed_lowercase("MiXeD"), Transaction, Wait),
            "SELECT pg_advisory_xact_lock(hashtext(lower($1::text))::int8)",
            vec![Value::from("MiXeD")],
        ),
        (
            lock(AdvisoryKey::single(7), Transaction, Try),
            "SELECT pg_try_advisory_xact_lock($1::int8) AS \"acquired\"",
            vec![Value::from(7_i64)],
        ),
        (
            lock(AdvisoryKey::single(7), Session, Try),
            "SELECT pg_try_advisory_lock($1::int8) AS \"acquired\"",
            vec![Value::from(7_i64)],
        ),
        (
            lock(AdvisoryKey::single(7), Session, Wait),
            "SELECT pg_advisory_lock($1::int8)",
            vec![Value::from(7_i64)],
        ),
        (
            lock(AdvisoryKey::single(7), Session, Release),
            "SELECT pg_advisory_unlock($1::int8) AS \"released\"",
            vec![Value::from(7_i64)],
        ),
    ] {
        let requirements = Requirements::for_statement(&statement);
        assert!(requirements.advisory_locks, "{sql}");
        let query = PostgresCompiler
            .compile(statement, &PostgresCompiler.support())
            .unwrap();
        assert_eq!(query.sql(), sql);
        // Text keys reach the database verbatim: the hashing and the case
        // folding are the database's, so raw callers contend on the same key.
        assert_eq!(query.params(), params, "{sql}");
        assert_eq!(query.params().len(), requirements.bind_parameters, "{sql}");
    }
}

#[test]
fn postgres_binds_the_transaction_setting_name_and_value() {
    let statement = Statement::SetTransactionSetting(
        SetTransactionSetting::new(SettingName::new("app_ns.flag").unwrap(), "on'; --").unwrap(),
    );
    let requirements = Requirements::for_statement(&statement);
    assert!(requirements.transaction_settings);
    let query = PostgresCompiler
        .compile(statement, &PostgresCompiler.support())
        .unwrap();
    assert_eq!(query.sql(), "SELECT set_config($1::text, $2::text, true)");
    assert_eq!(
        query.params(),
        &[Value::from("app_ns.flag"), Value::from("on'; --")]
    );
    assert_eq!(query.params().len(), requirements.bind_parameters);
}

#[test]
fn coordination_statements_are_refused_where_the_backend_has_neither() {
    let advisory = || lock(AdvisoryKey::single(7), AdvisoryLockScope::Transaction, AdvisoryLockAction::Wait);
    let setting = || {
        Statement::SetTransactionSetting(
            SetTransactionSetting::new(SettingName::new("app_ns.flag").unwrap(), "on").unwrap(),
        )
    };
    assert_eq!(
        SqliteCompiler
            .compile(advisory(), &SqliteCompiler.support())
            .unwrap_err(),
        CompileError::Unsupported("advisory locks")
    );
    assert_eq!(
        SqliteCompiler
            .compile(setting(), &SqliteCompiler.support())
            .unwrap_err(),
        CompileError::Unsupported("transaction settings")
    );
    let mut narrowed = PostgresCompiler.support();
    narrowed.advisory_locks = false;
    narrowed.transaction_settings = false;
    assert_eq!(
        PostgresCompiler.compile(advisory(), &narrowed).unwrap_err(),
        CompileError::Unsupported("advisory locks")
    );
    assert_eq!(
        PostgresCompiler.compile(setting(), &narrowed).unwrap_err(),
        CompileError::Unsupported("transaction settings")
    );
    // Control: the same statements compile where the backend supports them.
    assert!(PostgresCompiler
        .compile(advisory(), &PostgresCompiler.support())
        .is_ok());
    assert!(PostgresCompiler
        .compile(setting(), &PostgresCompiler.support())
        .is_ok());
}
