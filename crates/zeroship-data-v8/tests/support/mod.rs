//! Shared fixtures for plugin-db's integration targets.
//!
//! Declared by `integration.rs` and `sqlite_integration.rs`; each test binary
//! compiles its own copy, which is why every item here has to be reachable from
//! either parent without one depending on the other.

pub mod tables;

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Escape a literal for use inside a `LIKE` pattern.
///
/// `_` is a single-character wildcard, and every identifier this suite sweeps on
/// contains one, so an unescaped pattern would match names it was never meant
/// to reach.
fn like_escape(literal: &str) -> String {
    literal
        .replace('\\', "\\\\")
        .replace('_', "\\_")
        .replace('%', "\\%")
}

/// Prefix on every app id [`test_app_id_from`] mints.
///
/// It exists so [`sweep_prior_run_residue`] can find the previous run's leavings
/// without a hand-maintained list of names. A census of expected fixture names
/// goes stale the moment someone adds a test; a prefix does not.
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

/// Drop prior test schemas and their runtime roles in the current database.
///
/// Only [`sweep_prior_run_residue_once`] may call this before tests using the
/// suite prefix begin. Relay fixtures own and clean their replication objects.
pub async fn sweep_prior_run_residue(pool: &compio_postgres::Pool) {
    // Schemas first, then the roles that own them: a role with dependent
    // objects cannot be dropped, and CASCADE on the schema is what removes them.
    let schema_pattern = format!("{}%", like_escape(TEST_APP_PREFIX));
    if let Ok(rows) = pool
        .query_text_params(
            "SELECT nspname FROM pg_namespace WHERE nspname LIKE $1",
            &[&schema_pattern],
        )
        .await
    {
        for row in rows {
            let name: String = row.get(0);
            let _ = pool
                .execute(
                    &format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_ident(&name)),
                    &[],
                )
                .await;
        }
    }
    // `per_app_role_name` is `app_<app_id>_role`; this is that shape with the
    // app id replaced by the prefix and a wildcard, so the two cannot drift
    // apart silently.
    let role_pattern = format!(
        "{}%{}",
        like_escape(&format!("app_{TEST_APP_PREFIX}")),
        like_escape("_role"),
    );
    if let Ok(rows) = pool
        .query_text_params(
            "SELECT rolname FROM pg_roles WHERE rolname LIKE $1",
            &[&role_pattern],
        )
        .await
    {
        for row in rows {
            let name: String = row.get(0);
            let quoted = quote_ident(&name);
            let _ = pool.execute(&format!("DROP OWNED BY {quoted}"), &[]).await;
            let _ = pool
                .execute(&format!("DROP ROLE IF EXISTS {quoted}"), &[])
                .await;
        }
    }
}

/// Run [`sweep_prior_run_residue`] exactly once per test binary, as a BARRIER.
///
/// `Once::call_once` blocks every other caller until the first returns, so no
/// two callers sweep at once and none proceeds until the first has finished.
///
/// WHAT MAKES AN UNBOUNDED SWEEP CORRECT IS THE PREFIX, NOT THE PROCESS, and
/// that distinction started mattering when the crate's per-file test targets
/// were merged into one binary per feature set. This function drops everything
/// matching [`TEST_APP_PREFIX`], and the only things wearing that prefix are ids
/// minted by `test_app_id!`. Exactly the two modules that call `test_app_id!`
/// also call THIS - so every schema the sweep can destroy belongs to a module
/// that is already behind this barrier.
///
/// The sibling modules sharing the binary are not protected by the barrier and
/// do not need to be: they never mint the prefix, so the sweep cannot see their
/// fixtures. If a module ever starts minting `test_app_id!` ids without calling
/// this first, that stops being true and the sweep becomes able to delete a live
/// sibling's schema mid-run.
///
/// This paragraph previously said the sweep was safe because "every test in
/// these binaries reaches `require_pg` first". That was true of a one-file
/// binary and is not true of a merged one - `require_pg` does not call this, and
/// several modules here reach PostgreSQL through neither.
///
/// It runs on its own thread with its own compio runtime because the callers are
/// already inside a `block_on`, and a nested one panics.
pub fn sweep_prior_run_residue_once(url: &str) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let url = url.to_string();
        std::thread::spawn(move || {
            compio::runtime::Runtime::new()
                .expect("build the sweep runtime")
                .block_on(async move {
                    let Ok(pool) = compio_postgres::Pool::connect(&url, 1).await else {
                        return;
                    };
                    sweep_prior_run_residue(&pool).await;
                    // Paired teardown, inside the runtime that opened it - the
                    // property `integration.rs::direct_connection_sites_do_not_grow`
                    // pins this site for. A pool dropped as the runtime dies
                    // leaves the server backend alive for the process lifetime.
                    drop(pool);
                    compio_postgres::drain_connections(std::time::Duration::from_secs(2)).await;
                });
        })
        .join()
        .expect("the residue sweep thread must not panic");
    });
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
        if zeroship_core::declared_env_os!(external, "RUST_LOG", zeroship_core::config::TestHarness)
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
