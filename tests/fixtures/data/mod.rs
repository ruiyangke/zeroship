//! Shared database setup, compiled as a module by each test owner.

#[path = "sqlite.rs"]
pub mod tables;

#[path = "../postgres/mod.rs"]
pub mod postgres;

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Prefix for readable fixture app identifiers.
pub const TEST_APP_PREFIX: &str = "zst_";

/// Compose a stable app id from the test name and discriminator.
///
/// The id scopes the schema and runtime role together. The readable head is
/// bounded so the role wrapper fits PostgreSQL's identifier limit; a digest of
/// the full input keeps long test names distinct. Stable ids let a rerun reclaim
/// its own namespace after a crash.
pub fn test_app_id_from(name: &str, discriminator: &str) -> String {
    use sha2::{Digest, Sha256};

    // `std::any::type_name` of a nested dummy fn renders as
    // `<binary>::<test fn>::{{closure}}::f` under `#[compio::test]` and
    // `<binary>::<test fn>::f` under a plain `#[test]`. Take the last segment
    // that names something the author wrote.
    // `str::split` over a `&str` pattern is not double-ended, so this is a
    // forward scan to the last surviving segment rather than a `next_back`.
    let leaf = name
        .split("::")
        .filter(|seg| !seg.is_empty() && *seg != "f" && !seg.starts_with('{'))
        .last()
        .unwrap_or(name);

    let head: String = leaf
        .chars()
        .map(|c: char| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .take(36)
        .collect();

    use std::fmt::Write as _;
    let digest = Sha256::digest(format!("{name}\u{1}{discriminator}").as_bytes());
    let mut token = String::with_capacity(12);
    for byte in &digest[..6] {
        write!(&mut token, "{byte:02x}").expect("writing to a String cannot fail");
    }

    let id = format!("{TEST_APP_PREFIX}{head}_{token}");
    assert!(
        id.len() <= 54,
        "a test app id must leave room for the 9-byte role wrapper: {id} is {} bytes",
        id.len()
    );
    id
}

/// Expand to a per-test app id derived from the enclosing function's name.
///
/// `test_app_id!()` for a test that needs one app, `test_app_id!("b")` for the
/// second app of a test that needs two. The discriminator is folded into the
/// digest, so the two ids differ in the part PostgreSQL cannot truncate away.
///
/// It is a macro because the name has to come from the COMPILER, not from
/// `std::thread::current().name()`. The thread name does equal the test name,
/// but only on the test's own thread; several fixtures here read their app id
/// from helpers running elsewhere, where a thread-name read would be silently
/// wrong rather than absent.
#[macro_export]
macro_rules! test_app_id {
    () => {
        $crate::test_app_id!("")
    };
    ($discriminator:expr) => {{
        fn f() {}
        fn type_name_of<T>(_: T) -> &'static str {
            std::any::type_name::<T>()
        }
        $crate::support::test_app_id_from(type_name_of(f), $discriminator)
    }};
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
#[allow(dead_code)]
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

mod tracing;
pub use tracing::init_test_tracing;

pub(crate) mod roles;
