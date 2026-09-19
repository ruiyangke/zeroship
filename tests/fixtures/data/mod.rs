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
macro_rules! test_app_id {
    () => {
        $crate::tests::fixtures::test_app_id!("")
    };
    ($discriminator:expr) => {{
        fn f() {}
        fn type_name_of<T>(_: T) -> &'static str {
            std::any::type_name::<T>()
        }
        $crate::tests::fixtures::test_app_id_from(type_name_of(f), $discriminator)
    }};
}

std::thread_local! {
    /// One binding per app id on this thread.
    ///
    /// A harness has no isolate and no control plane to resolve a binding from,
    /// so it mints one. It must mint the SAME one every time it is asked for an
    /// app: the schema, the transaction lane and the role the session narrows to
    /// all come off the binding, and a fixture that provisioned one binding's
    /// schema and then ran under another's would fail at `SET LOCAL ROLE` rather
    /// than exercising anything.
    ///
    /// Per thread, because libtest runs each test on its own thread and a shared
    /// map would let one test's ids reach another's cluster.
    static HARNESS_BINDINGS: std::cell::RefCell<
        std::collections::HashMap<String, zeroship_data_orm::binding::DbBinding>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// The binding a harness with no live isolate runs `app_id` under.
///
/// Stable for the life of the calling thread, and a real database edge: the
/// schema is `db_<dbs>` and the session narrows to `zs_bind_<bnd>_e1`, exactly
/// as a resolved production binding does.
pub fn harness_binding(app_id: &str) -> zeroship_data_orm::binding::DbBinding {
    HARNESS_BINDINGS.with(|bindings| {
        bindings
            .borrow_mut()
            .entry(app_id.to_owned())
            .or_insert_with(|| {
                zeroship_data_orm::binding::DbBinding::to_database(
                    app_id,
                    zeroship_data_orm::binding::COLD_START_DEPLOY_TOKEN,
                    zeroship_core::DatabaseId::mint(),
                    zeroship_core::BindingId::mint(),
                    HARNESS_EPOCH,
                )
                .expect("a minted database and edge compose a legal role name")
            })
            .clone()
    })
}

/// One app's binding as a SECOND deploy of it holds it.
///
/// Same tenant, same database, same edge, same epoch - only the deploy token
/// differs, which is what the descriptor and protection-floor caches key on.
pub fn harness_binding_at_deploy(
    app_id: &str,
    deploy_token: &str,
) -> zeroship_data_orm::binding::DbBinding {
    let base = harness_binding(app_id);
    let edge = base
        .edge()
        .expect("a harness binding addresses a database");
    zeroship_data_orm::binding::DbBinding::to_database(
        base.app_id(),
        deploy_token,
        edge.database().clone(),
        edge.binding().clone(),
        edge.epoch(),
    )
    .expect("a harness binding's ids compose a legal role name")
}

/// The binding store a harness hands the database service.
///
/// Only the V8 adapter builds a `DbServiceConfig`, so this is unused in the
/// ORM's own test build of this shared module.
///
/// A worker installs what Control resolved; a harness installs what
/// [`harness_binding`] composed, so an isolate minted for one of these apps
/// reads the same binding the fixture provisioned the cluster for. An app that
/// is not listed gets no `env.db`, which is the production refusal.
#[allow(dead_code)]
pub fn harness_app_bindings<'a>(
    app_ids: impl IntoIterator<Item = &'a str>,
) -> std::sync::Arc<zeroship_data_orm::resolved_bindings::SuppliedAppBindings> {
    let store = std::sync::Arc::new(
        zeroship_data_orm::resolved_bindings::SuppliedAppBindings::new(),
    );
    for app_id in app_ids {
        let binding = harness_binding(app_id);
        let edge = binding
            .edge()
            .expect("a harness binding addresses a database");
        store
            .supply(
                app_id,
                zeroship_data_orm::resolved_bindings::ResolvedBinding::from(edge),
            )
            .expect("a fresh store accepts its first binding");
    }
    store
}

/// The physical schema a harness's work for `app_id` is qualified with.
///
/// On PostgreSQL it is the namespace; on SQLite it is the `ATTACH` alias, which
/// occupies the same position in a qualified table name. A fixture that
/// created its tables under any other name would put them where the ORM does
/// not look.
pub fn harness_alias(app_id: &str) -> String {
    harness_binding(app_id).schema().as_str().to_owned()
}

/// The binding whose schema is `alias`, for a fixture that already holds the
/// physical name and needs the identity back.
///
/// # Panics
///
/// Panics when no binding on this thread addresses `alias`. A fixture that
/// invented a qualifier has nothing to attach it for.
pub fn harness_binding_for_alias(alias: &str) -> zeroship_data_orm::binding::DbBinding {
    HARNESS_BINDINGS.with(|bindings| {
        bindings
            .borrow()
            .values()
            .find(|binding| binding.schema().as_str() == alias)
            .cloned()
            .unwrap_or_else(|| panic!("no harness binding on this thread addresses {alias}"))
    })
}

/// The lane key a harness's work for `app_id` is held under.
pub fn harness_route(app_id: &str) -> zeroship_data_orm::binding::DbRoute {
    harness_binding(app_id).route()
}

/// The schema epoch every harness binding is minted at.
///
/// Named rather than spelled at each site so a fixture that provisions the
/// ladder and a test that asserts against a retired epoch cannot disagree about
/// which epoch is live.
pub const HARNESS_EPOCH: u32 = 1;

/// The capability role a binding's column grants are issued to.
///
/// The binding role holds no privileges of its own - it inherits exactly one
/// database role - so a grant issued to the binding role directly would make
/// the fixture pass while production, where the apply emits grants to the
/// capability role, fails.
pub fn harness_capability_role(binding: &zeroship_data_orm::binding::DbBinding) -> String {
    zeroship_core::database_derivation::capability_role_name(
        binding
            .database()
            .expect("a harness binding addresses a database"),
        zeroship_core::database_role::DatabaseCapability::ReadWrite,
    )
    .expect("a minted database composes a legal capability role name")
}

/// Grant a table created after the fixture ladder was provisioned.
///
/// Production provisioning covers later migrator-owned tables with default
/// privileges. Some tests create tables through a separate admin owner, so they
/// grant those fixtures explicitly.
pub async fn grant_all_runtime_table_columns(
    pool: &compio_postgres::Pool,
    binding: &zeroship_data_orm::binding::DbBinding,
    table: &str,
) {
    let schema = binding.schema().as_str();
    let rows = pool
        .query_text_params(
            "SELECT column_name FROM information_schema.columns \
              WHERE table_schema = $1 AND table_name = $2 \
              ORDER BY ordinal_position",
            &[schema, table],
        )
        .await
        .expect("read fixture table columns");
    let columns = rows
        .iter()
        .map(|row| quote_ident(row.get::<_, &str>("column_name")))
        .collect::<Vec<_>>();
    assert!(
        !columns.is_empty(),
        "fixture table {schema}.{table} is missing"
    );

    let role = harness_capability_role(binding);
    let columns = columns.join(", ");
    let table = format!("{}.{}", quote_ident(schema), quote_ident(table));
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

/// Build a fixture with deliberately narrower read authority.
#[allow(dead_code)]
pub async fn grant_runtime_select_columns(
    pool: &compio_postgres::Pool,
    binding: &zeroship_data_orm::binding::DbBinding,
    table: &str,
    columns: &[&str],
) {
    assert!(
        !columns.is_empty(),
        "a SELECT grant needs at least one column"
    );
    let role = harness_capability_role(binding);
    let columns = columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ");
    pool.batch_execute(&format!(
        "GRANT SELECT ({columns}) ON TABLE {}.{} TO {}",
        quote_ident(binding.schema().as_str()),
        quote_ident(table),
        quote_ident(&role),
    ))
    .await
    .expect("grant fixture read columns to the runtime role");
}

mod tracing;
pub use tracing::init_test_tracing;

pub(crate) mod roles;

pub(crate) use test_app_id;
