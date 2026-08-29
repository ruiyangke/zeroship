//! Shared fixtures for plugin-db's integration targets.
//!
//! Declared by `integration.rs` and `sqlite_integration.rs`; each test binary
//! compiles its own copy, which is why every item here has to be reachable from
//! either parent without one depending on the other.

pub mod tables;

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Stand in for a readwrite binding's explicit column grants.
///
/// The integration provisioner intentionally grants no table DML. Fixtures
/// whose subject is CRUD still need authority, but it must be expressed as
/// column grants so an omitted column remains enforceable by PostgreSQL.
pub async fn grant_all_runtime_table_columns(pool: &compio_postgres::Pool, app: &str, table: &str) {
    let rows = pool
        .query_text_params(
            "SELECT column_name FROM information_schema.columns \
              WHERE table_schema = $1 AND table_name = $2 \
              ORDER BY ordinal_position",
            &[app, table],
        )
        .await
        .expect("read fixture table columns");
    let columns = rows
        .iter()
        .map(|row| quote_ident(row.get::<_, &str>("column_name")))
        .collect::<Vec<_>>();
    assert!(
        !columns.is_empty(),
        "fixture table {app}.{table} is missing"
    );

    let role = zeroship_core::database_role::per_app_role_name(app)
        .expect("fixture app id must produce a runtime role");
    let columns = columns.join(", ");
    let table = format!("{}.{}", quote_ident(app), quote_ident(table));
    let role = quote_ident(&role);
    pool.batch_execute(&format!(
        "GRANT SELECT ({columns}) ON TABLE {table} TO {role}; \
         GRANT INSERT ({columns}) ON TABLE {table} TO {role}; \
         GRANT UPDATE ({columns}) ON TABLE {table} TO {role}; \
         GRANT DELETE ON TABLE {table} TO {role};"
    ))
    .await
    .expect("grant fixture columns to the runtime role");
}

/// Grant only the columns an audited read fixture needs.
pub async fn grant_runtime_select_columns(
    pool: &compio_postgres::Pool,
    app: &str,
    table: &str,
    columns: &[&str],
) {
    assert!(
        !columns.is_empty(),
        "a SELECT grant needs at least one column"
    );
    let role = zeroship_core::database_role::per_app_role_name(app)
        .expect("fixture app id must produce a runtime role");
    let columns = columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ");
    pool.batch_execute(&format!(
        "GRANT SELECT ({columns}) ON TABLE {}.{} TO {}",
        quote_ident(app),
        quote_ident(table),
        quote_ident(&role),
    ))
    .await
    .expect("grant fixture read columns to the runtime role");
}

/// Install a tracing subscriber for an integration binary, at most once.
///
/// WHY THIS EXISTS. The runtime deliberately blanks non-allowlisted 5xx bodies
/// to `{"message":"internal error"}` and routes the real text to `tracing`; its
/// own source calls that "diagnosable only from a worker log"
/// (`crates/zeroship-runtime/src/core/dispatch.rs:245-246`). No plugin-db
/// integration binary installed a subscriber, so on this side of the boundary
/// that stream went nowhere: a failing test reported `internal error` and
/// `RUST_LOG` had no effect, because there was no subscriber to configure.
/// Four `native_transaction` failures were unattributable for exactly this
/// reason - the diagnostic the design relies on did not exist in the harness.
///
/// It is a NO-OP unless `RUST_LOG` is set, so a default run is byte-identical
/// to before and no test's behaviour depends on the variable. `RUST_LOG` is
/// read only to choose verbosity of diagnostic output; it carries no test
/// configuration and no test asserts on it.
pub fn init_test_tracing() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // `RUST_LOG` is a contract owned by tracing-subscriber, not a zeroship
        // knob, so it is declared `external` with a test-harness consumer
        // marker. Reading it through the typed accessor is what the
        // `disallowed_methods` lint on `std::env::var_os` is asking for; the
        // bare read was a deny-level clippy error that nothing surfaced,
        // because this target only lints under `--all-targets --all-features`.
        if zeroship_core::declared_env_os!(
            external,
            "RUST_LOG",
            zeroship_core::config::TestHarness
        )
        .is_none()
        {
            return;
        }
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
    });
}
