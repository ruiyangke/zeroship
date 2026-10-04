//! Shared database setup for the data-plane crates.
//!
//! The surface here speaks PLAIN data: a binding is a handful of ids and
//! strings, a route is an app id and an optional database id, and a field map
//! is `serde_json::Value`. Nothing here names a data-orm type, so a crate whose
//! own unit tests exercise the data plane can reach it as an ordinary
//! `[dev-dependencies]` entry rather than by compiling a private copy of the
//! source. Each consumer converts to its own types in a thin adapter beside its
//! tests, and the identities are derived through the same
//! `zeroship_core::database_derivation` composers production uses, so the
//! adapter cannot invent a schema or role a reconciler did not create.

pub mod tables;

pub mod platform;
pub mod schema;

use zeroship_core::database_derivation;
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::{BindingId, DatabaseId};

/// Quote a `PostgreSQL` identifier. No leaf crate exports a public one, so the
/// fixture owns its own rather than re-deriving the escaping.
pub(crate) fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Deploy token used when a host does not inject `ZEROSHIP_DEPLOY_ID` (local
/// dev, raw-JS deploys, and narrow test harnesses).
pub const COLD_START_DEPLOY_TOKEN: &str = "cold_start";

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

/// One harness app's identity as plain data: the tenant, the deploy, the
/// database, the edge and the capability.
///
/// The schema and both role names are not stored: they are DERIVED from the ids
/// through `zeroship_core::database_derivation`, the same composers the cluster
/// reconciler creates the objects with. A consumer converts this into its own
/// binding type through its public constructor, so the two cannot disagree.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct HarnessBinding {
    app_id: String,
    deploy_token: String,
    database: DatabaseId,
    binding: BindingId,
    capability: DatabaseCapability,
}

impl HarnessBinding {
    #[must_use]
    pub fn new(
        app_id: impl Into<String>,
        deploy_token: impl Into<String>,
        database: DatabaseId,
        binding: BindingId,
        capability: DatabaseCapability,
    ) -> Self {
        Self {
            app_id: app_id.into(),
            deploy_token: deploy_token.into(),
            database,
            binding,
            capability,
        }
    }

    #[must_use]
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    #[must_use]
    pub fn deploy_token(&self) -> &str {
        &self.deploy_token
    }

    #[must_use]
    pub fn database(&self) -> &DatabaseId {
        &self.database
    }

    #[must_use]
    pub fn binding(&self) -> &BindingId {
        &self.binding
    }

    #[must_use]
    pub fn capability(&self) -> DatabaseCapability {
        self.capability
    }

    /// The physical schema this binding's statements are qualified with.
    #[must_use]
    pub fn schema(&self) -> String {
        database_derivation::schema_name(&self.database)
    }

    /// The role the session-setup batch narrows to: `zs_bind_<bnd>`.
    #[must_use]
    pub fn session_role(&self) -> String {
        database_derivation::binding_role_name(&self.binding)
            .expect("a minted binding composes a legal binding role name")
    }

    /// The role an audited raw-column read assumes: `zs_db_<dbs>_unmask`.
    #[must_use]
    pub fn unmask_role(&self) -> String {
        database_derivation::unmask_role_name(&self.database)
            .expect("a minted database composes a legal unmask role name")
    }

    /// The lane key this binding's work is held under.
    #[must_use]
    pub fn route(&self) -> HarnessRoute {
        HarnessRoute {
            app_id: self.app_id.clone(),
            database: Some(self.database.clone()),
        }
    }
}

/// One binding's lane key as plain data: the tenant and its optional database.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HarnessRoute {
    app_id: String,
    database: Option<DatabaseId>,
}

impl HarnessRoute {
    #[must_use]
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    #[must_use]
    pub fn database(&self) -> Option<&DatabaseId> {
        self.database.as_ref()
    }
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
        std::collections::HashMap<String, HarnessBinding>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// The binding a harness with no live isolate runs `app_id` under.
///
/// Stable for the life of the calling thread, and a real database edge: the
/// schema is `db_<dbs>` and the session narrows to `zs_bind_<bnd>`, exactly
/// as a resolved production binding does.
#[must_use]
pub fn harness_binding(app_id: &str) -> HarnessBinding {
    HARNESS_BINDINGS.with(|bindings| {
        bindings
            .borrow_mut()
            .entry(app_id.to_owned())
            .or_insert_with(|| {
                HarnessBinding::new(
                    app_id,
                    COLD_START_DEPLOY_TOKEN,
                    DatabaseId::mint(),
                    BindingId::mint(),
                    HARNESS_CAPABILITY,
                )
            })
            .clone()
    })
}

/// One app's binding as a SECOND deploy of it holds it.
///
/// Same tenant, same database, same edge - only the deploy token differs, which
/// is what the descriptor and protection-floor caches key on.
#[must_use]
pub fn harness_binding_at_deploy(app_id: &str, deploy_token: &str) -> HarnessBinding {
    let base = harness_binding(app_id);
    HarnessBinding::new(
        base.app_id(),
        deploy_token,
        base.database().clone(),
        base.binding().clone(),
        base.capability(),
    )
}

/// The same binding at another deploy token, for a fixture that already holds
/// one rather than only an app id.
///
/// A test with TWO databases holds two bindings for one app, so the app-keyed
/// [`harness_binding_at_deploy`] cannot name the second.
#[must_use]
pub fn harness_binding_at_deploy_for(
    binding: &HarnessBinding,
    deploy_token: &str,
) -> HarnessBinding {
    HarnessBinding::new(
        binding.app_id(),
        deploy_token,
        binding.database().clone(),
        binding.binding().clone(),
        binding.capability(),
    )
}

/// Every app's harness binding, in the order asked for.
///
/// A consumer builds its own binding store from these: the store is a
/// data-plane type, so the testkit hands over the plain identities only.
#[must_use]
pub fn harness_bindings<'a>(
    app_ids: impl IntoIterator<Item = &'a str>,
) -> Vec<HarnessBinding> {
    app_ids.into_iter().map(harness_binding).collect()
}

/// The physical schema a harness's work for `app_id` is qualified with.
///
/// On PostgreSQL it is the namespace; on SQLite it is the `ATTACH` alias, which
/// occupies the same position in a qualified table name. A fixture that
/// created its tables under any other name would put them where the ORM does
/// not look.
#[must_use]
pub fn harness_alias(app_id: &str) -> String {
    harness_binding(app_id).schema()
}

/// The binding whose schema is `alias`, for a fixture that already holds the
/// physical name and needs the identity back.
///
/// # Panics
///
/// Panics when no binding on this thread addresses `alias`. A fixture that
/// invented a qualifier has nothing to attach it for.
#[must_use]
pub fn harness_binding_for_alias(alias: &str) -> HarnessBinding {
    HARNESS_BINDINGS.with(|bindings| {
        bindings
            .borrow()
            .values()
            .find(|binding| binding.schema() == alias)
            .cloned()
            .unwrap_or_else(|| panic!("no harness binding on this thread addresses {alias}"))
    })
}

/// The lane key a harness's work for `app_id` is held under.
#[must_use]
pub fn harness_route(app_id: &str) -> HarnessRoute {
    harness_binding(app_id).route()
}

/// The database a harness's encrypted columns for `app_id` are keyed on.
///
/// At-rest keys and the ciphertext AAD are derived from the DATABASE, so a
/// fixture that composed ciphertext by hand has to name the same one the
/// pipeline will, and that is whatever [`harness_binding`] minted for this app
/// on this thread.
#[must_use]
pub fn harness_database(app_id: &str) -> DatabaseId {
    harness_binding(app_id).database().clone()
}

/// The capability every harness binding is minted with.
///
/// Read-write, because a harness provisions BOTH capability roles on the
/// cluster and then exercises the engine, which writes. A read-only harness
/// binding is composed per test by the one test that is about a read-only
/// binding, so this constant is what every other test varies nothing about.
pub const HARNESS_CAPABILITY: DatabaseCapability = DatabaseCapability::ReadWrite;

/// The capability role a binding's column grants are issued to.
///
/// The binding role holds no privileges of its own - it inherits exactly one
/// database role - so a grant issued to the binding role directly would make
/// the fixture pass while production, where the apply emits grants to the
/// capability role, fails.
#[must_use]
pub fn harness_capability_role(binding: &HarnessBinding) -> String {
    database_derivation::capability_role_name(binding.database(), HARNESS_CAPABILITY)
        .expect("a minted database composes a legal capability role name")
}

/// Grant a table created after the fixture ladder was provisioned.
///
/// Production provisioning covers later migrator-owned tables with default
/// privileges. Some tests create tables through a separate admin owner, so they
/// grant those fixtures explicitly.
pub async fn grant_all_runtime_table_columns(
    pool: &compio_postgres::Pool,
    binding: &HarnessBinding,
    table: &str,
) {
    let schema = binding.schema();
    let rows = pool
        .query_text_params(
            "SELECT column_name FROM information_schema.columns \
              WHERE table_schema = $1 AND table_name = $2 \
              ORDER BY ordinal_position",
            &[&schema, table],
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
    let table = format!("{}.{}", quote_ident(&schema), quote_ident(table));
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
pub async fn grant_runtime_select_columns(
    pool: &compio_postgres::Pool,
    binding: &HarnessBinding,
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
        quote_ident(&binding.schema()),
        quote_ident(table),
        quote_ident(&role),
    ))
    .await
    .expect("grant fixture read columns to the runtime role");
}

pub mod tracing;
pub use tracing::init_test_tracing;

pub mod roles;
