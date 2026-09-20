#![recursion_limit = "256"]

//! The app/database decoupling, driven end to end in ONE run.
//!
//! Every slice of `docs/proposals/2026-08-28-app-database-decoupling.md` has
//! been measured in isolation against its own live cluster. Nothing had ever
//! driven the whole path in one exercise, and a path is not the sum of its
//! slices: the seams between them are where a design stops working.
//!
//! # What this drives, and what "real" means at each step
//!
//! - Two databases created through `zeroship_control::databases::create_database`
//!   and bound through `bind_database` - the same functions the HTTP handlers
//!   call - and converged by `zeroship_migrate_server::datastore::Reconciler`,
//!   the production loop, against a throwaway PostgreSQL 16 cluster. Nothing
//!   here writes `active` and nothing hand-rolls a schema or a role.
//! - Migrations applied through `zeroship_migrate_server::apply::apply_ir_documents`,
//!   once per database, each naming its own target.
//! - A deploy per app through `Registry::deploy`, which is where
//!   `publication::catalog::admit_bindings` refuses an artifact naming a
//!   database the app holds no live binding to.
//! - Creator JavaScript in a real V8 isolate with the real `DbPlugin`, reaching
//!   `env.db` and `env.databases.<label>`.
//!
//! # Two servers, and that is the point
//!
//! Control declares into ITS database and the reconciler makes ANOTHER server
//! match. A fixture that faked either side could not exhibit the property the
//! whole design turns on - that the control row and the catalog it describes
//! are written separately.
//!
//! # Every claim is paired with the control that gives it meaning
//!
//! A cross-app read proves nothing without the write it reads; an absence
//! proves nothing without the presence beside it; a refusal proves nothing
//! without the permitted case it differs from in one variable. Each stage
//! below names its own control.

mod common;

/// The tenant cluster fixture, reached where it already lives rather than
/// copied.
///
/// `zeroship-migrate-server`'s reconciler target owns this fixture and the
/// reconciler under test here is measured against THAT server shape - the
/// deployed major, `fsync=off`, a throwaway container per test because
/// `pg_authid` and `pg_auth_members` are cluster-shared. A second spelling in
/// this crate would let the two drift and make a failure here unattributable.
#[path = "../../zeroship-migrate-server/tests/fixture/tenant.rs"]
#[allow(
    dead_code,
    reason = "the shared fixture also serves the version-floor arm, which this target does not have"
)]
mod tenant;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use compio_postgres::error::SqlState;
use compio_postgres::{Client, NoTls};
use serde_json::{json, Value as Json};
use uuid::Uuid;

use zeroship_bundle::{Manifest, RuntimeDescriptorEntry};
use zeroship_control::databases::{
    self, BindDatabaseBody, CreateDatabaseBody, CAPABILITY_READWRITE,
};
use zeroship_control::organizations::{self, CreateOrganizationBody};
use zeroship_control::publication::CatalogError;
use zeroship_control::Registry;
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::{database_derivation, AppId, BindingId, DatabaseId, UserId};
use zeroship_data_orm::connection::ConnectionFactory;
use zeroship_data_orm::encryption::SuppliedProjectKeys;
use zeroship_data_orm::resolved_bindings::{ResolvedBinding, SuppliedAppBindings};
use zeroship_data_v8::service::{DbService, DbServiceConfig};
use zeroship_migrate_server::apply::{apply_ir_documents, ApplyMigrationsRequest, WORKER_ROLE};
use zeroship_migrate_server::datastore::control::ControlStore;
use zeroship_migrate_server::datastore::{PassReport, Reconciler};
use zeroship_migrate_server::policy::ManagedPolicyConfig;
use zeroship_migrate_server::schema_apply_store::SchemaApplyStore;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, SettledFetch};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The password the fixture gives the shared worker login.
///
/// The bootstrap corpus creates that login WITHOUT one, because a bootstrap
/// that baked a known password into every tenant cluster would be worse than
/// the manual step it saves. Supplying one is the operator's side of the
/// deployment, and the container is thrown away with the test.
const WORKER_PASSWORD: &str = "fixture";

/// The project root key the host supplies, hexadecimal, 32 bytes.
///
/// Control serves this from `zeroship.project_data_keys`; what the data path
/// does with it is expand it per DATABASE, which is the axis this target
/// measures. A fixture value keeps the key-delivery protocol out of the
/// subject.
const PROJECT_ROOT_HEX: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

/// A syntactically valid descriptor hash. Nothing on the deploy path compares
/// it any more - the schema-equality gate is deleted - but `Manifest::validate`
/// still refuses one that is not 64 hexadecimal characters.
const DESCRIPTOR_HASH: &str = "1111111111111111111111111111111111111111111111111111111111111111";

/// The MAC key the confined migration ceiling is sealed with.
const SEAL_KEY: &[u8] = b"database decoupling e2e seal key 32 bytes";

/// The creator's local label for the database both apps share.
const SHARED_LABEL: &str = "main";

/// The creator's local label for the database only app A binds.
const PRIVATE_LABEL: &str = "analytics";

const SHARED_COLLECTION: &str = "notes";
const PRIVATE_COLLECTION: &str = "events";

/// The plaintext app A writes into the encrypted column.
const SECRET: &str = "123-45-6789";

/// The plaintext app A writes into a column that is NOT encrypted, so the
/// cross-app read has a control that does not depend on the crypto path.
const HEADLINE: &str = "shared across two apps";

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

async fn connect(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, NoTls)
        .await
        .unwrap_or_else(|error| panic!("connect to {url}: {error}"));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

/// The control database as `zeroship_control`: the least-privilege login the
/// migration service actually opens, so the reconciler's statements run under
/// the grants `db/migrations-ts/20260919000200_database_entities.ts` hands it
/// rather than under the fixture's superuser.
async fn control_as_service(url: &str) -> Arc<Client> {
    let mut parsed = url::Url::parse(url).expect("the fixture DSN parses");
    parsed
        .set_username("zeroship_control")
        .expect("the DSN accepts a username");
    parsed
        .set_password(Some("zeroship_control"))
        .expect("the DSN accepts a password");
    Arc::new(connect(parsed.as_str()).await)
}

/// The tenant cluster reached as the shared worker login, which is the only
/// login a data-plane session ever opens.
fn worker_url(base: &str) -> String {
    let mut url = url::Url::parse(base).expect("the fixture URL parses");
    url.set_username(WORKER_ROLE)
        .expect("the URL accepts a username");
    url.set_password(Some(WORKER_PASSWORD))
        .expect("the URL accepts a password");
    url.into()
}

// ---------------------------------------------------------------------------
// The world: an organization, a project in its own zone, and two apps
// ---------------------------------------------------------------------------

struct World {
    registry: Registry,
    control_url: String,
    pg: Client,
    zone: String,
    project: String,
    owner: UserId,
}

impl World {
    async fn new(label: &str) -> Self {
        let control_url = common::require_control_db();
        let pg = connect(&control_url).await;
        let registry = Registry::new(&control_url).await.expect("control registry");
        common::ensure_builtin_plans(&registry).await;

        // THE ZONE IS PRIVATE TO THIS TEST. Placement filters on it, so the
        // cluster this test's reconciler registers is invisible to every other
        // test's project, and this project can be placed on nothing else.
        let zone = zeroship_core::typed_id::generate("ezn");
        pg.execute(
            "INSERT INTO zeroship.execution_zones (id, name, status) VALUES ($1, $2, 'active')",
            &[&zone, &format!("{label}-{}", Uuid::new_v4().simple())],
        )
        .await
        .expect("declare this test's execution zone");

        let owner = seed_user(&pg, label).await;
        let organization = organizations::create_organization(
            &registry,
            &owner,
            &CreateOrganizationBody {
                name: format!("{label} {}", Uuid::new_v4().simple()),
                slug: Some(format!("{label}-{}", Uuid::new_v4().simple())),
                billing_email: None,
            },
            None,
        )
        .await
        .expect("mint this test's organization through the production path");

        // The project is INSERTed rather than created through
        // `organizations::create_project`, which takes the zone from the column
        // default. This world's whole point is a project in a zone of its own.
        let project = zeroship_core::typed_id::generate("prj");
        pg.execute(
            "INSERT INTO zeroship.projects \
                 (id, organization_id, slug, name, execution_zone_id) \
             VALUES ($1, $2, $3, $4, $5)",
            &[
                &project,
                &organization.id,
                &format!("{label}-{}", Uuid::new_v4().simple()),
                &label,
                &zone,
            ],
        )
        .await
        .expect("seed this test's project in this test's zone");

        Self {
            registry,
            control_url,
            pg,
            zone,
            project,
            owner,
        }
    }

    /// An app in this world's project and zone.
    ///
    /// Written directly rather than through `Registry::create_app`, which
    /// resolves a zone by name and refuses to choose when a deployment declares
    /// more than one active zone - and this world declares a second one for the
    /// duration of the test.
    async fn app(&self, label: &str) -> AppId {
        let app = AppId::mint();
        self.pg
            .execute(
                "INSERT INTO zeroship.apps \
                     (id, name, plan_id, project_id, organization_id, execution_zone_id) \
                 VALUES ($1, $2, $3, $4, \
                         (SELECT organization_id FROM zeroship.projects WHERE id = $4), $5)",
                &[
                    &app.as_str(),
                    &format!("{label}-{}", Uuid::new_v4().simple()),
                    &zeroship_control::plan_catalog::free_plan_id(),
                    &self.project,
                    &self.zone,
                ],
            )
            .await
            .expect("seed an app in this test's project");
        app
    }

    async fn database_row(&self, database: &DatabaseId) -> (String, i32) {
        let row = self
            .pg
            .query_one(
                "SELECT status, schema_epoch FROM zeroship.databases WHERE id = $1",
                &[&database.as_str()],
            )
            .await
            .expect("the database row must exist to be read");
        (row.get("status"), row.get("schema_epoch"))
    }

    async fn binding_row(&self, binding: &BindingId) -> (String, i32, i32) {
        let row = self
            .pg
            .query_one(
                "SELECT status, generation, observed_generation \
                   FROM zeroship.database_bindings WHERE id = $1",
                &[&binding.as_str()],
            )
            .await
            .expect("the binding row must exist to be read");
        (
            row.get("status"),
            row.get("generation"),
            row.get("observed_generation"),
        )
    }

    /// Every live binding the control plane would serve `app`, read through the
    /// ONE predicate that serves them - `LIVE_BINDINGS_FROM_WHERE`, the constant
    /// `GET /internal/apps/{app_id}/bindings` and the CDC relay both select on.
    ///
    /// The route itself is behind a zone-scoped service assertion, which is a
    /// different subject; what a worker installs is this result set, so this
    /// reads the same rows through the same SQL rather than restating it.
    async fn live_bindings(&self, app: &AppId) -> Vec<ResolvedBinding> {
        let sql = format!(
            "SELECT b.id AS binding_id, b.database_id, d.schema_epoch {} ORDER BY b.id",
            zeroship_core::live_binding::LIVE_BINDINGS_FROM_WHERE
        );
        self.pg
            .query(&sql, &[&app.as_str()])
            .await
            .expect("read this app's live bindings")
            .iter()
            .map(|row| {
                let epoch: i32 = row.get("schema_epoch");
                ResolvedBinding {
                    database: DatabaseId::parse(row.get::<_, String>("database_id").as_str())
                        .expect("control stores a typed database id"),
                    binding: BindingId::parse(row.get::<_, String>("binding_id").as_str())
                        .expect("control stores a typed binding id"),
                    epoch: u32::try_from(epoch).expect("a schema epoch is not negative"),
                }
            })
            .collect()
    }
}

async fn seed_user(pg: &Client, label: &str) -> UserId {
    let id = UserId::mint();
    pg.execute(
        "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
         VALUES ($1, $2::citext, $3, NOW())",
        &[
            &id.as_str(),
            &format!("{label}-{}@zeroship.test", id.as_str()),
            &label,
        ],
    )
    .await
    .expect("seed a principal");
    id
}

// ---------------------------------------------------------------------------
// The migration service
// ---------------------------------------------------------------------------

fn policy_config() -> ManagedPolicyConfig {
    ManagedPolicyConfig::default_confined(SEAL_KEY.to_vec(), 1).expect("the confined ceiling")
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("zs-dbd-e2e-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&path).expect("create the request scratch directory");
    path
}

/// The creator migration for the SHARED database: a title and a `bytea` column
/// the runtime descriptor marks `encrypted`.
///
/// No `id` and no primary key: the confined ceiling's mandatory `[[inject]]`
/// rule supplies both (`policies/confined-system-shape.inject.toml`, whose
/// `author_primary_key = "forbid"` refuses one the creator declares), and a
/// migration that declared them would be refused rather than duplicated.
fn shared_migration() -> Json {
    json!({
        "kind": "ir",
        "descriptor_sha256": DESCRIPTOR_HASH,
        "documents": [{
            "filename": "0001_create_notes.ir.json",
            "body": {
                "ir_version": 1,
                "name": "create_notes",
                "ops": [{
                    "op": "createTable",
                    "name": SHARED_COLLECTION,
                    "columns": [
                        {"name": "title", "type": "text", "nullable": false},
                        {"name": "ssn", "type": "bytes"}
                    ]
                }]
            }
        }]
    })
}

/// The creator migration for the database only app A binds.
fn private_migration() -> Json {
    json!({
        "kind": "ir",
        "descriptor_sha256": DESCRIPTOR_HASH,
        "documents": [{
            "filename": "0001_create_events.ir.json",
            "body": {
                "ir_version": 1,
                "name": "create_events",
                "ops": [{
                    "op": "createTable",
                    "name": PRIVATE_COLLECTION,
                    "columns": [
                        {"name": "kind", "type": "text", "nullable": false}
                    ]
                }]
            }
        }]
    })
}

/// Apply one creator migration into one database, through the migration
/// service's own apply.
async fn apply_into(
    tenant_url: &str,
    control_url: &str,
    app: &AppId,
    database: &DatabaseId,
    principal: &UserId,
    request: Json,
    label: &str,
) {
    let request: ApplyMigrationsRequest =
        serde_json::from_value(request).expect("the fixture is a legal apply request");
    let tmp = tmpdir(label);
    let report = apply_ir_documents(
        tenant_url,
        &tmp,
        zeroship_migrate_server::apply::ApplyTarget {
            app_id: app,
            database_id: database,
        },
        &request,
        &policy_config(),
        &SchemaApplyStore::new(control_url.to_owned()),
        principal,
    )
    .await
    .unwrap_or_else(|error| panic!("apply into {label}: {error}"));
    assert!(
        !report.applied.is_empty() && report.skipped.is_empty(),
        "the apply must advance the journal rather than skip: {report:?}"
    );
    let _ = std::fs::remove_dir_all(tmp);
}

// ---------------------------------------------------------------------------
// Reading the tenant catalog back
// ---------------------------------------------------------------------------

async fn tables_in(cluster: &Client, database: &DatabaseId) -> Vec<String> {
    let schema = database_derivation::schema_name(database);
    cluster
        .query(
            "SELECT c.relname FROM pg_class c \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
              WHERE n.nspname = $1 AND c.relkind IN ('r', 'p') AND NOT c.relispartition \
              ORDER BY c.relname",
            &[&schema],
        )
        .await
        .expect("read the tenant catalog")
        .iter()
        .map(|row| row.get::<_, String>("relname"))
        .collect()
}

/// Whether a capability role may read a column of a creator table, asked of
/// PostgreSQL rather than inferred from the statements an apply sent.
async fn has_column_privilege(
    cluster: &Client,
    role: &str,
    database: &DatabaseId,
    table: &str,
    column: &str,
    privilege: &str,
) -> bool {
    let qualified = format!("{}.{table}", database_derivation::schema_name(database));
    cluster
        .query_one(
            "SELECT has_column_privilege($1, $2, $3, $4) AS granted",
            &[&role, &qualified, &column, &privilege],
        )
        .await
        .expect("ask PostgreSQL for the privilege")
        .get("granted")
}

/// The CREATOR-declared tables of one database's schema.
///
/// The engine's migration journal and the unmask audit table live in the same
/// schema, under the `__zeroship_` prefix that a creator collection name is
/// refused from - which is what makes filtering on it exact rather than a
/// guess. Without the filter, "this database holds this collection and no
/// other" could only be written as a `contains`, and a `contains` passes over
/// an apply that wrote into every database the app is bound to.
async fn creator_tables(cluster: &Client, database: &DatabaseId) -> Vec<String> {
    tables_in(cluster, database)
        .await
        .into_iter()
        .filter(|name| !name.starts_with("__zeroship_"))
        .collect()
}

/// Every role holding any privilege on a table, as PostgreSQL's own ACL says.
///
/// Carried into the tripwire's message so a failure names what the apply DID
/// grant rather than only what it did not.
async fn table_grantees(cluster: &Client, database: &DatabaseId, table: &str) -> Vec<String> {
    let schema = database_derivation::schema_name(database);
    cluster
        .query(
            "SELECT DISTINCT acl.grantee AS grantee \
               FROM pg_class c \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
               CROSS JOIN LATERAL aclexplode(COALESCE(c.relacl, acldefault('r', c.relowner))) acl \
              WHERE n.nspname = $1 AND c.relname = $2",
            &[&schema, &table],
        )
        .await
        .expect("read the table ACL")
        .iter()
        .map(|row| {
            let grantee: u32 = row.get("grantee");
            grantee.to_string()
        })
        .collect()
}

/// Grant the capability roles what a creator session needs on one creator
/// table.
///
/// **This stands in for a production step that does not exist.** The apply
/// creates the table as `zs_db_<dbs>_mig` and emits no privilege for
/// `zs_db_<dbs>_rw` or `_ro`, so without this every statement below would fail
/// at the grant instead of measuring what it is about. It is column-scoped
/// rather than table-wide because that is the shape the design calls for: a
/// classified column is withheld and the column holding its mask granted
/// instead, and a table-level `GRANT SELECT` beside a column list widens rather
/// than narrows.
async fn stand_in_for_the_capability_grants_the_apply_does_not_emit(
    cluster: &Client,
    database: &DatabaseId,
    table: &str,
) {
    let schema = database_derivation::schema_name(database);
    let columns: Vec<String> = cluster
        .query(
            "SELECT a.attname AS name FROM pg_attribute a \
               JOIN pg_class c ON c.oid = a.attrelid \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
              WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
              ORDER BY a.attnum",
            &[&schema, &table],
        )
        .await
        .expect("read the table's columns")
        .iter()
        .map(|row| row.get::<_, String>("name"))
        .collect();
    assert!(
        !columns.is_empty(),
        "the table must exist and carry columns for this grant to mean anything"
    );
    let list = columns
        .iter()
        .map(|column| format!("\"{column}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let readwrite =
        database_derivation::capability_role_name(database, DatabaseCapability::ReadWrite)
            .expect("the capability role name fits");
    let readonly =
        database_derivation::capability_role_name(database, DatabaseCapability::ReadOnly)
            .expect("the capability role name fits");
    cluster
        .batch_execute(&format!(
            "GRANT SELECT ({list}), INSERT ({list}), UPDATE ({list}) \
                 ON \"{schema}\".\"{table}\" TO \"{readwrite}\";
             GRANT DELETE ON \"{schema}\".\"{table}\" TO \"{readwrite}\";
             GRANT SELECT ({list}) ON \"{schema}\".\"{table}\" TO \"{readonly}\";"
        ))
        .await
        .expect("the stand-in grants apply over a converged schema");
}

/// The ciphertext on disk for one row, read with the admin client so the bytes
/// are the ones PostgreSQL stored rather than anything the ORM produced.
async fn stored_bytes(
    cluster: &Client,
    database: &DatabaseId,
    table: &str,
    column: &str,
) -> Vec<u8> {
    let schema = database_derivation::schema_name(database);
    let rows = cluster
        .query(
            &format!("SELECT encode(\"{column}\", 'hex') AS hex FROM \"{schema}\".\"{table}\""),
            &[],
        )
        .await
        .expect("read the stored bytes");
    assert_eq!(rows.len(), 1, "exactly one row must be present to be read");
    let hex: String = rows[0].get("hex");
    assert!(!hex.is_empty(), "the column must hold bytes, not NULL");
    hex.as_bytes()
        .chunks(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("hex is ASCII"), 16)
                .expect("hex digits")
        })
        .collect()
}

/// Read one row under `role`, the way the data plane narrows: `SET LOCAL ROLE`
/// as the first statement of an explicit transaction.
///
/// Returns the server's own `ErrorResponse` on a refusal. A transport failure
/// carries no SQLSTATE and treating one as "some error" is how a denial arm
/// stops measuring the denial.
async fn count_under_role(
    client: &mut Client,
    role: &str,
    database: &DatabaseId,
    table: &str,
) -> Result<i64, compio_postgres::Error> {
    let schema = database_derivation::schema_name(database);
    let transaction = client.transaction().await?;
    let narrowed = async {
        transaction
            .simple_query(&format!("SET LOCAL ROLE \"{role}\""))
            .await?;
        transaction
            .query(
                &format!("SELECT count(*)::bigint AS n FROM \"{schema}\".\"{table}\""),
                &[],
            )
            .await
    }
    .await;
    // Explicit on both paths: a failed statement leaves the session in an
    // aborted transaction, and the next arm's refusal would then be 25P02
    // rather than the one being measured.
    let rolled_back = transaction.rollback().await;
    let rows = narrowed?;
    rolled_back?;
    Ok(rows[0].get("n"))
}

fn server_error(error: &compio_postgres::Error) -> &compio_postgres::error::DbError {
    error
        .as_db_error()
        .unwrap_or_else(|| panic!("expected a server error response, got: {error}"))
}

// ---------------------------------------------------------------------------
// Runtime descriptors
// ---------------------------------------------------------------------------

/// The system fields the confined ceiling's mandatory injection adds to every
/// creator table, taken from the generated artifact rather than restated.
///
/// `crates/zeroship-data-orm/tests/fixtures/schema.runtime.json` is the
/// committed output of the generator both the migration ceiling and the type
/// generator read, so splicing from it keeps one description of the injected
/// shape. A hand-written copy here would drift the day the ceiling changes and
/// the drift would surface as an unexplained column error.
fn generated_fields(authored: Json) -> Json {
    const ORACLE: &str = include_str!("../../zeroship-data-orm/tests/fixtures/schema.runtime.json");
    let oracle: Json = serde_json::from_str(ORACLE).expect("the generated descriptor parses");
    let mut fields = serde_json::Map::new();
    for (name, definition) in oracle["collections"]["posts"]["fields"]
        .as_object()
        .expect("the oracle declares a field map")
    {
        if definition.get("assign").is_some() {
            fields.insert(name.clone(), definition.clone());
        }
    }
    for (name, definition) in authored.as_object().expect("an authored field map") {
        match fields.get_mut(name) {
            Some(existing) => existing
                .as_object_mut()
                .expect("an injected field is an object")
                .extend(
                    definition
                        .as_object()
                        .expect("an authored field is an object")
                        .clone(),
                ),
            None => {
                fields.insert(name.clone(), definition.clone());
            }
        }
    }
    // The per-field `softDelete` and `concurrency` markers describe the
    // collection, not the column, and the runtime carries them in `options`.
    for definition in fields.values_mut() {
        if let Some(object) = definition.as_object_mut() {
            object.remove("softDelete");
            object.remove("concurrency");
        }
    }
    Json::Object(fields)
}

/// One database's v2 schema descriptor.
fn descriptor(collection: &str, authored: Json) -> Json {
    json!({
        "version": 2,
        "collections": {
            collection: {
                "fields": generated_fields(authored),
                "options": {
                    "softDelete": false,
                    "versioning": false,
                    "strictness": "strict",
                },
                "indexes": [],
            },
        },
    })
}

fn shared_descriptor() -> Json {
    descriptor(
        SHARED_COLLECTION,
        json!({
            "title": { "type": "string", "required": true },
            // The one column the decoupling re-keyed: its at-rest key and its
            // AAD are expanded from the DATABASE, so a co-tenant of that
            // database can read it and a neighbouring database cannot.
            "ssn": { "type": "string", "encrypted": true },
        }),
    )
}

fn private_descriptor() -> Json {
    descriptor(
        PRIVATE_COLLECTION,
        json!({ "kind": { "type": "string", "required": true } }),
    )
}

/// The descriptor document a host hands the runtime: one entry per database,
/// exactly one of them primary.
fn document(entries: Vec<(&str, &DatabaseId, bool, Json)>) -> String {
    let databases: Vec<Json> = entries
        .into_iter()
        .map(|(label, database, primary, schema)| {
            json!({
                "label": label,
                "database_id": database.as_str(),
                "primary": primary,
                "schema": schema,
            })
        })
        .collect();
    json!({ "version": 1, "databases": databases }).to_string()
}

// ---------------------------------------------------------------------------
// Creator dispatch
// ---------------------------------------------------------------------------

struct Dispatch {
    worker_url: String,
    app: AppId,
    project: String,
    bindings: Vec<ResolvedBinding>,
    document: String,
    source: String,
    procedure: &'static str,
}

/// Run one creator RPC in a real isolate, on a thread of its own.
///
/// V8 is per thread and so is the database plugin's context, so a dispatch gets
/// a thread the way the worker gives it one. The thread also owns the pooled
/// PostgreSQL connections the dispatch opens, and it is joined before the
/// fixture's containers are dropped.
fn dispatch(spec: Dispatch) -> (u16, Json) {
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                init_v8();

                let app_bindings = Arc::new(SuppliedAppBindings::new());
                assert!(
                    !spec.bindings.is_empty(),
                    "a dispatch with no resolved binding measures nothing: an app with \
                     none has no env.db at all"
                );
                for resolved in &spec.bindings {
                    app_bindings
                        .supply(spec.app.as_str(), resolved.clone())
                        .expect("the store accepts each database's binding");
                }

                // The host authorizes the app against the project whose root
                // key it supplied. Control serves both together.
                let project_keys = Arc::new(
                    SuppliedProjectKeys::new()
                        .with_hex(&spec.project, PROJECT_ROOT_HEX)
                        .expect("the fixture project root key parses"),
                );
                project_keys
                    .bind_app(spec.app.as_str(), &spec.project)
                    .expect("the host authorizes the app against its project");

                let plugins: Vec<Arc<dyn NativePlugin>> = vec![DbService::new(DbServiceConfig {
                    app_bindings,
                    project_keys,
                    connection: ConnectionFactory::for_url(&spec.worker_url)
                        .expect("the worker DSN is usable"),
                    cdc_relay: None,
                    meter: None,
                })
                .expect("the database service composes")
                .plugin()];

                let runtime = Runtime::builder()
                    .modules(vec![ModuleEntry {
                        specifier: "index.js".into(),
                        source: spec.source.clone(),
                    }])
                    .env_vars(HashMap::from([(
                        "APP_ID".to_string(),
                        spec.app.as_str().to_string(),
                    )]))
                    .plugins(plugins)
                    .runtime_descriptor(Some(spec.document.clone()))
                    .build();

                let outcome = runtime.call_fetch_handler(
                    "POST",
                    &format!("http://localhost/__zeroship/v1/{}", spec.procedure),
                    &[("content-type".into(), "application/json".into())],
                    r#"{"json":null}"#,
                    &EnvSnapshot::empty(),
                    RequestCtx::new(CancelFlag::new()),
                );
                let (status, body) = match outcome {
                    FetchOutcome::Response { status, body, .. } => (status, body),
                    FetchOutcome::Pending { rx, cancel: _ } => {
                        let rt = compio::runtime::Runtime::new()
                            .expect("build this thread's compio runtime");
                        rt.block_on(async {
                            runtime.start_pump();
                            let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                                .await
                                .expect("the creator dispatch settled within its budget")
                                .expect("the dispatch channel delivered a settled fetch");
                            match settled {
                                SettledFetch::Response { status, body, .. } => (status, body),
                                SettledFetch::Stream { .. } => panic!(
                                    "a creator RPC settles as a buffered response; a stream here \
                                     means the dispatch took the streaming path"
                                ),
                                SettledFetch::WebSocketUpgrade { .. } => {
                                    panic!("a creator RPC must not upgrade to a WebSocket")
                                }
                            }
                        })
                    }
                    FetchOutcome::Stream { .. } => {
                        panic!("a creator RPC settles as a buffered response, not a stream")
                    }
                    FetchOutcome::WebSocketUpgrade { .. } => {
                        panic!("a creator RPC must not upgrade to a WebSocket")
                    }
                };
                let body = String::from_utf8_lossy(&body).into_owned();
                let json =
                    serde_json::from_str(&body).unwrap_or_else(|_| Json::String(body.clone()));
                (status, json)
            })
            .join()
            .expect("the creator dispatch thread must not panic")
    })
}

/// The payload an RPC returned, refusing anything but a 200.
fn json_of(status: u16, body: &Json) -> Json {
    assert_eq!(status, 200, "the creator dispatch must succeed: {body}");
    body["json"].clone()
}

/// The envelope every `env.db.<collection>` call answers with outside a
/// transaction, unwrapped once so a failed write cannot read as a success.
///
/// `packages/db/src/types.ts` declares `Result<T>` as
/// `{ data: T; error: null } | { data: null; error: Error }`, and the runtime
/// adapter returns it from `ok(...)` / `err(...)`. Creator code that ignored it
/// would see `{ data: null, error }` as a truthy value and carry on.
const UNWRAP: &str = r#"
function unwrap(result, what) {
    if (result === null || typeof result !== "object"
        || !("data" in result) || !("error" in result)) {
        throw new Error(what + ": expected the { data, error } envelope, got "
            + JSON.stringify(result));
    }
    if (result.error) {
        throw new Error(what + ": " + (result.error.message ?? String(result.error)));
    }
    return result.data;
}
"#;

/// App A: writes into BOTH of its databases and reads both back.
fn app_a_source() -> String {
    format!(
        r#"
import {{ env }} from "zeroship";
{UNWRAP}
async function seed() {{
    unwrap(
        await env.db.__SHARED__.insert({{ title: "__HEADLINE__", ssn: "__SECRET__" }}),
        "insert through env.db",
    );
    unwrap(
        await env.databases.__PRIVATE_LABEL__.__PRIVATE__.insert({{ kind: "a-only" }}),
        "insert through env.databases.__PRIVATE_LABEL__",
    );
    return {{ ok: true }};
}}

async function inspect() {{
    const notes = unwrap(await env.db.__SHARED__.find({{}}), "read through env.db");
    const events = unwrap(
        await env.databases.__PRIVATE_LABEL__.__PRIVATE__.find({{}}),
        "read through env.databases.__PRIVATE_LABEL__",
    );
    return {{
        labels: Object.keys(env.databases).sort(),
        primary_is_env_db: env.db === env.databases.__SHARED_LABEL__,
        notes: notes.map((row) => ({{ title: row.title, ssn: row.ssn }})),
        events: events.map((row) => ({{ kind: row.kind }})),
        // The two databases declare DIFFERENT collections, so a handle that
        // reached the wrong database would find the name and not the table.
        analytics_has_notes: typeof env.databases.__PRIVATE_LABEL__.__SHARED__,
        main_has_events: typeof env.databases.__SHARED_LABEL__.__PRIVATE__,
    }};
}}

export default {{ rpc: {{ seed, inspect }} }};
"#
    )
    .replace("__SHARED__", SHARED_COLLECTION)
    .replace("__PRIVATE__", PRIVATE_COLLECTION)
    .replace("__SHARED_LABEL__", SHARED_LABEL)
    .replace("__PRIVATE_LABEL__", PRIVATE_LABEL)
    .replace("__HEADLINE__", HEADLINE)
    .replace("__SECRET__", SECRET)
}

/// App B: a DIFFERENT tenant, with its own binding to the SAME database.
fn app_b_source() -> String {
    format!(
        r#"
import {{ env }} from "zeroship";
{UNWRAP}
async function inspect() {{
    const notes = unwrap(await env.db.__SHARED__.find({{}}), "read through env.db");
    return {{
        labels: Object.keys(env.databases).sort(),
        notes: notes.map((row) => ({{ title: row.title, ssn: row.ssn }})),
        // App B holds no binding to the other database, so the host resolved
        // none and the document names none: there is no handle to reach.
        has_analytics: Object.prototype.hasOwnProperty.call(
            env.databases, "__PRIVATE_LABEL__"),
    }};
}}

export default {{ rpc: {{ inspect }} }};
"#
    )
    .replace("__SHARED__", SHARED_COLLECTION)
    .replace("__PRIVATE_LABEL__", PRIVATE_LABEL)
}

// ---------------------------------------------------------------------------
// The exercise
// ---------------------------------------------------------------------------

/// Run one reconciler pass and refuse anything but a completed one.
async fn pass(reconciler: &Reconciler) -> (String, PassReport) {
    let (datastore, report) = reconciler
        .reconcile_once()
        .await
        .expect("the reconciler pass must complete");
    assert!(
        report.failures.is_empty(),
        "a pass that recorded a per-row failure has not converged: {report:?}"
    );
    (datastore.as_str().to_owned(), report)
}

#[ntex::test]
async fn the_whole_decoupled_path_runs_in_one_exercise() {
    let cluster_fixture = tenant::Cluster::start();
    let mut cluster = connect(cluster_fixture.url()).await;
    let world = World::new("dbd-e2e").await;
    let reconciler = Reconciler::new(
        ControlStore::new(control_as_service(&world.control_url).await),
        cluster_fixture.url(),
        Some(world.zone.clone()),
    );

    // -----------------------------------------------------------------------
    // STAGE 0. The operator's side: a cluster registers itself and bootstraps.
    // -----------------------------------------------------------------------
    let (datastore, first) = pass(&reconciler).await;
    assert!(
        first.registered && first.datastore_activated,
        "the first pass registers the cluster and applies the bootstrap corpus: {first:?}"
    );

    // -----------------------------------------------------------------------
    // STAGE 1. Two databases, created through the control surface.
    // -----------------------------------------------------------------------
    let shared = databases::create_database(
        &world.registry,
        &world.owner,
        &world.project,
        &CreateDatabaseBody {
            name: "orders".to_owned(),
        },
        None,
    )
    .await
    .expect("the owner creates a database in their own project");
    let private = databases::create_database(
        &world.registry,
        &world.owner,
        &world.project,
        &CreateDatabaseBody {
            name: "analytics".to_owned(),
        },
        None,
    )
    .await
    .expect("the owner creates a second database in the same project");

    assert_eq!(
        shared.datastore_id, datastore,
        "placement chose the cluster this test's reconciler registered"
    );
    assert_eq!(private.datastore_id, datastore);
    assert_ne!(shared.id, private.id, "two creates are two databases");
    // THE CEILING. Control declares and stops; nothing in that process can
    // reach a cluster it holds no credential for.
    assert_eq!(
        (shared.status.as_str(), private.status.as_str()),
        ("provisioning", "provisioning"),
        "the control surface must not claim a convergence it did not perform"
    );

    let shared_id = DatabaseId::parse(&shared.id).expect("control mints a typed database id");
    let private_id = DatabaseId::parse(&private.id).expect("control mints a typed database id");

    // -----------------------------------------------------------------------
    // STAGE 2. Two apps and three bindings, through the control surface.
    // -----------------------------------------------------------------------
    let app_a = world.app("shop").await;
    let app_b = world.app("reports").await;

    let mut edges = Vec::new();
    for (app, database, what) in [
        (&app_a, &shared_id, "app A on the shared database"),
        (&app_a, &private_id, "app A on its own database"),
        (&app_b, &shared_id, "app B on the shared database"),
    ] {
        let record = databases::bind_database(
            &world.registry,
            &world.owner,
            database,
            &BindDatabaseBody {
                app_id: app.as_str().to_owned(),
                capability: CAPABILITY_READWRITE.to_owned(),
            },
            None,
        )
        .await
        .unwrap_or_else(|error| panic!("bind {what}: {error:?}"));
        assert_eq!(
            record.status, "pending",
            "a binding stops at pending until a reconciler has made the cluster match"
        );
        edges.push(BindingId::parse(&record.id).expect("control mints a typed binding id"));
    }
    let (a_shared, a_private, b_shared) = (edges[0].clone(), edges[1].clone(), edges[2].clone());

    // CONTROL, before the pass: nothing on the cluster names either database.
    let schemas_before = cluster
        .query(
            "SELECT nspname FROM pg_namespace WHERE left(nspname, 3) = 'db_'",
            &[],
        )
        .await
        .expect("read the namespace catalog");
    assert!(
        schemas_before.is_empty(),
        "no database schema may exist before the pass that creates them"
    );

    let (_, second) = pass(&reconciler).await;
    let mut activated = second.databases_activated.clone();
    activated.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    let mut expected = vec![shared_id.clone(), private_id.clone()];
    expected.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    assert_eq!(
        activated, expected,
        "both declared databases converged in one pass: {second:?}"
    );
    assert_eq!(
        second.bindings_activated.len(),
        3,
        "all three declared bindings converged: {second:?}"
    );

    // THE BINDINGS ARE LIVE, read back from the rows rather than the report.
    for (edge, what) in [
        (&a_shared, "app A on the shared database"),
        (&a_private, "app A on its own database"),
        (&b_shared, "app B on the shared database"),
    ] {
        let (status, generation, observed) = world.binding_row(edge).await;
        assert_eq!(status, "active", "{what} must be active");
        assert!(
            observed >= generation,
            "{what}: a live binding's observed generation has caught up \
             ({observed} >= {generation})"
        );
    }
    for database in [&shared_id, &private_id] {
        assert_eq!(
            world.database_row(database).await.0,
            "active",
            "a converged database is active"
        );
    }

    // -----------------------------------------------------------------------
    // STAGE 3. A migration into EACH database, through the migration service.
    // -----------------------------------------------------------------------
    let principal = seed_user(&world.pg, "migrator").await;
    // CONTROL: both schemas exist and neither carries a creator table, so what
    // each apply put where is measurable.
    for database in [&shared_id, &private_id] {
        assert_eq!(
            tables_in(&cluster, database).await,
            Vec::<String>::new(),
            "a converged schema carries no creator table before its apply"
        );
    }

    apply_into(
        cluster_fixture.url(),
        &world.control_url,
        &app_a,
        &shared_id,
        &principal,
        shared_migration(),
        "shared",
    )
    .await;
    apply_into(
        cluster_fixture.url(),
        &world.control_url,
        &app_a,
        &private_id,
        &principal,
        private_migration(),
        "private",
    )
    .await;

    // EXACT, in both directions: the collection its own migration declared and
    // no other. `contains` alone would pass over an apply that wrote into every
    // database the app is bound to.
    assert_eq!(
        creator_tables(&cluster, &shared_id).await,
        vec![SHARED_COLLECTION.to_owned()],
        "the shared database holds the collection its own migration created, and only it"
    );
    assert_eq!(
        creator_tables(&cluster, &private_id).await,
        vec![PRIVATE_COLLECTION.to_owned()],
        "the private database holds the collection its own migration created, and only it"
    );

    // -----------------------------------------------------------------------
    // STAGE 3b. THE SEAM BETWEEN THE APPLY AND THE RUNTIME, AND IT IS OPEN.
    //
    // `zeroship_migrate_server::datastore::cluster` grants the capability roles
    // `USAGE` on the schema and nothing else, and says why: "those grants are
    // regenerated by the apply path inside the same transaction as the DDL that
    // creates the table". Asked of PostgreSQL rather than taken from that
    // sentence, the answer is that they are not. The apply grants only the
    // unmask audit table (`apply::apply_ir_documents` ->
    // `provisioning::grant_audit_unmask_to_capabilities`); the creator table it
    // just created is owned by `zs_db_<dbs>_mig` and reachable by nothing else.
    //
    // THE ASSERTION BELOW IS A TRIPWIRE ON THAT STATE, and the stand-in after
    // it is the production step that does not exist. When the apply emits the
    // capability grants, this goes red: delete it and the stand-in together,
    // and the rest of this exercise keeps measuring what it measures now.
    // -----------------------------------------------------------------------
    let shared_rw =
        database_derivation::capability_role_name(&shared_id, DatabaseCapability::ReadWrite)
            .expect("the capability role name fits");
    let private_rw =
        database_derivation::capability_role_name(&private_id, DatabaseCapability::ReadWrite)
            .expect("the capability role name fits");
    let grantees = table_grantees(&cluster, &shared_id, SHARED_COLLECTION).await;
    assert!(
        !has_column_privilege(
            &cluster,
            &shared_rw,
            &shared_id,
            SHARED_COLLECTION,
            "title",
            "SELECT",
        )
        .await,
        "TRIPWIRE: the apply now leaves the readwrite capability role able to read the \
         table it created, so the stand-in below is obsolete and both must go. \
         Grantees on the table: {grantees:?}"
    );
    // The unmask audit table IS granted, which is the control that keeps the
    // tripwire from passing because the apply granted nothing anywhere.
    assert!(
        has_column_privilege(
            &cluster,
            &shared_rw,
            &shared_id,
            zeroship_migrate_server::provisioning::AUDIT_UNMASK_TABLE,
            "id",
            "INSERT",
        )
        .await,
        "the apply's own grant did run, so the absence above is about the creator \
         table specifically and not about an apply that failed"
    );
    for (database, table) in [
        (&shared_id, SHARED_COLLECTION),
        (&private_id, PRIVATE_COLLECTION),
    ] {
        stand_in_for_the_capability_grants_the_apply_does_not_emit(&cluster, database, table).await;
    }
    for (role, database, table, column) in [
        (&shared_rw, &shared_id, SHARED_COLLECTION, "title"),
        (&shared_rw, &shared_id, SHARED_COLLECTION, "ssn"),
        (&private_rw, &private_id, PRIVATE_COLLECTION, "kind"),
    ] {
        for privilege in ["SELECT", "INSERT"] {
            assert!(
                has_column_privilege(&cluster, role, database, table, column, privilege).await,
                "the stand-in must leave the readwrite capability role holding \
                 {privilege} on {table}.{column}, or every creator statement below \
                 fails on the grant rather than on what it is about"
            );
        }
    }

    // -----------------------------------------------------------------------
    // STAGE 4. A deploy per app, admitted on its bindings.
    // -----------------------------------------------------------------------
    let manifest_a = manifest(vec![
        (SHARED_LABEL, &shared_id, true),
        (PRIVATE_LABEL, &private_id, false),
    ]);
    common::deployments::deploy(&world.registry, &app_a, &world.owner, manifest_a)
        .await
        .expect("app A's deploy names two databases it holds live bindings to");

    let manifest_b = manifest(vec![(SHARED_LABEL, &shared_id, true)]);
    common::deployments::deploy(&world.registry, &app_b, &world.owner, manifest_b)
        .await
        .expect("app B's deploy names the one database it holds a live binding to");

    // THE REFUSAL, differing in ONE variable: the same app, the same shape, one
    // more database - the one it holds no binding to.
    let overreach = manifest(vec![
        (SHARED_LABEL, &shared_id, true),
        (PRIVATE_LABEL, &private_id, false),
    ]);
    let refused = common::deployments::deploy(&world.registry, &app_b, &world.owner, overreach)
        .await
        .expect_err("app B must not deploy an artifact naming a database it does not bind");
    match &refused {
        CatalogError::DatabaseNotBound { databases } => assert_eq!(
            databases.as_slice(),
            [private_id.as_str().to_owned()],
            "the refusal must name the database that is missing a binding, and only it"
        ),
        other => panic!("expected DatabaseNotBound, got {other:?}"),
    }

    // -----------------------------------------------------------------------
    // STAGE 5. Creator code, in a real isolate, through env.db and
    // env.databases.<label>.
    // -----------------------------------------------------------------------
    cluster
        .batch_execute(&format!(
            "ALTER ROLE \"{WORKER_ROLE}\" PASSWORD '{WORKER_PASSWORD}'"
        ))
        .await
        .expect("the operator supplies the worker login's authentication material");
    let worker = worker_url(cluster_fixture.url());

    let a_bindings = world.live_bindings(&app_a).await;
    let b_bindings = world.live_bindings(&app_b).await;
    assert_eq!(
        a_bindings.len(),
        2,
        "control serves app A both of its live bindings"
    );
    assert_eq!(
        b_bindings.len(),
        1,
        "control serves app B the one live binding it holds"
    );
    assert_eq!(
        b_bindings[0].database, shared_id,
        "app B's one binding names the SHARED database"
    );

    let a_document = document(vec![
        (SHARED_LABEL, &shared_id, true, shared_descriptor()),
        (PRIVATE_LABEL, &private_id, false, private_descriptor()),
    ]);
    let b_document = document(vec![(SHARED_LABEL, &shared_id, true, shared_descriptor())]);

    // App A writes one row into EACH of its databases.
    let (status, body) = dispatch(Dispatch {
        worker_url: worker.clone(),
        app: app_a.clone(),
        project: world.project.clone(),
        bindings: a_bindings.clone(),
        document: a_document.clone(),
        source: app_a_source(),
        procedure: "seed",
    });
    json_of(status, &body);

    // ---- PROPERTY 2: app A reaches BOTH its databases in one deploy, and a
    // write to one does not appear in the other.
    let (status, body) = dispatch(Dispatch {
        worker_url: worker.clone(),
        app: app_a.clone(),
        project: world.project.clone(),
        bindings: a_bindings.clone(),
        document: a_document,
        source: app_a_source(),
        procedure: "inspect",
    });
    let a_view = json_of(status, &body);
    assert_eq!(
        a_view["labels"],
        json!([PRIVATE_LABEL, SHARED_LABEL]),
        "env.databases carries a member per database the deployment declares: {a_view}"
    );
    assert_eq!(
        a_view["primary_is_env_db"],
        json!(true),
        "env.db and env.databases[primary] are ONE handle, by identity"
    );
    assert_eq!(
        a_view["notes"],
        json!([{ "title": HEADLINE, "ssn": SECRET }]),
        "app A reads back what it wrote through env.db: {a_view}"
    );
    assert_eq!(
        a_view["events"],
        json!([{ "kind": "a-only" }]),
        "app A reads back what it wrote through env.databases.analytics: {a_view}"
    );
    assert_eq!(
        (
            a_view["analytics_has_notes"].as_str(),
            a_view["main_has_events"].as_str()
        ),
        (Some("undefined"), Some("undefined")),
        "each database's collections are installed under ITS OWN binding, so a \
         write to one cannot surface in the other: {a_view}"
    );
    // The same claim at the storage layer, where a shared table would show.
    assert_eq!(
        creator_tables(&cluster, &private_id).await,
        vec![PRIVATE_COLLECTION.to_owned()],
        "the private database holds only its own collection"
    );

    // ---- PROPERTY 1: app B reads rows app A wrote in the SHARED database,
    // including the ENCRYPTED column.
    //
    // CONTROL first: what is on disk is ciphertext, so a design that had
    // stopped encrypting could not satisfy the read below for the wrong reason.
    let on_disk = stored_bytes(&cluster, &shared_id, SHARED_COLLECTION, "ssn").await;
    assert_ne!(
        on_disk.as_slice(),
        SECRET.as_bytes(),
        "the encrypted column must hold ciphertext, not the plaintext"
    );

    let (status, body) = dispatch(Dispatch {
        worker_url: worker.clone(),
        app: app_b.clone(),
        project: world.project.clone(),
        bindings: b_bindings.clone(),
        document: b_document,
        source: app_b_source(),
        procedure: "inspect",
    });
    let b_view = json_of(status, &body);
    assert_eq!(
        b_view["notes"],
        json!([{ "title": HEADLINE, "ssn": SECRET }]),
        "a DIFFERENT app, with its own binding and its own role, reads the row app A \
         wrote - the plaintext column AND the encrypted one. This is the capability \
         the decoupling exists for and app keying broke: {b_view}"
    );
    assert_eq!(
        b_view["labels"],
        json!([SHARED_LABEL]),
        "app B's deployment declares one database: {b_view}"
    );
    assert_eq!(
        b_view["has_analytics"],
        json!(false),
        "an app holds no handle for a database it holds no binding to: {b_view}"
    );

    // ---- PROPERTY 3: app B is REFUSED the database it holds no binding to,
    // by PostgreSQL itself.
    //
    // The isolate has no handle, which is the first fence; this is the second.
    // The role app B's session narrows to is real and converged, and it is
    // pointed at the neighbouring schema.
    let b_role = database_derivation::binding_role_name(&b_shared, b_bindings[0].epoch)
        .expect("the binding role name fits");
    // CONTROL: the same role, the same statement shape, its OWN database.
    assert_eq!(
        count_under_role(&mut cluster, &b_role, &shared_id, SHARED_COLLECTION)
            .await
            .expect("app B's binding role reaches the database it names"),
        1,
        "the control for the refusal below: this role reads its own database"
    );
    let crossed = count_under_role(&mut cluster, &b_role, &private_id, PRIVATE_COLLECTION)
        .await
        .expect_err("app B's binding role must not reach a database it holds no binding to");
    let crossed = server_error(&crossed);
    assert_eq!(
        crossed.code(),
        &SqlState::INSUFFICIENT_PRIVILEGE,
        "the refusal must be PostgreSQL's own 42501 and not a local check: {crossed:?}"
    );
    assert!(
        crossed.message().contains("permission denied for schema"),
        "42501 must be the SCHEMA denial - the fence is USAGE on the neighbour's \
         namespace, not a missing table: {crossed:?}"
    );

    // -----------------------------------------------------------------------
    // STAGE 6. PROPERTY 4: a revoked binding is observable.
    //
    // The data path separates a REVOKED binding from a RETIRED schema epoch by
    // which error `SET LOCAL ROLE` returns - 42501 means the role stands and
    // this login may not assume it, 22023 means there is no such role - and
    // the reconciler's revoke arm exists to keep the first reachable: it
    // withdraws both edges and leaves the role standing.
    //
    // WHAT THE CONTROL SURFACE ACTUALLY DOES IS THE OTHER ONE.
    // `databases::unbind_database` DELETES the binding row, and a role no
    // declaration names is REAPED, so the refusal a creator reaches through the
    // only call a creator has is the retired-epoch one. The two halves below
    // measure both, in that order.
    // -----------------------------------------------------------------------
    databases::unbind_database(&world.registry, &world.owner, &shared_id, &app_b, None)
        .await
        .expect("the owner unbinds app B from the shared database");
    assert!(
        world
            .pg
            .query_opt(
                "SELECT 1 FROM zeroship.database_bindings WHERE id = $1",
                &[&b_shared.as_str()],
            )
            .await
            .expect("read the binding row back")
            .is_none(),
        "unbind removes the row rather than moving it to a revoking state"
    );
    assert!(
        world.live_bindings(&app_b).await.is_empty(),
        "control serves an unbound app no binding, so its next isolate has no env.db"
    );
    // AND THE CLUSTER HAS NOT MOVED. The unbind is a declaration, not an
    // effect: until a pass reaps the role, a session that already resolved the
    // binding still reads. This is the window the design's `revoking` state
    // exists to close, and the control surface does not open it.
    assert_eq!(
        count_under_role(&mut cluster, &b_role, &shared_id, SHARED_COLLECTION)
            .await
            .expect("the reaped-not-yet role still reads"),
        1,
        "between the unbind and the next reconciler pass the binding role still \
         carries its grants"
    );

    let (_, reaping) = pass(&reconciler).await;
    assert!(
        reaping.roles_reaped.contains(&b_role),
        "the pass drops the binding role no declaration names: {reaping:?}"
    );
    let reaped = count_under_role(&mut cluster, &b_role, &shared_id, SHARED_COLLECTION)
        .await
        .expect_err("a reaped binding role cannot be assumed");
    let reaped = server_error(&reaped);
    assert_eq!(
        reaped.code(),
        &SqlState::INVALID_PARAMETER_VALUE,
        "a REAPED role answers 22023, which the data path classifies as a retired \
         epoch and re-resolves; the terminal 42501 is not what an unbind produces: \
         {reaped:?}"
    );
    assert_eq!(
        reaped.message(),
        format!("role \"{b_role}\" does not exist"),
        "22023 is the generic bad-GUC code, so the message is what separates a \
         missing role from any other rejected SET: {reaped:?}"
    );

    // THE 42501 SHAPE, and the only declaration that reaches it.
    // `zeroship.database_bindings.status = 'revoking'` is what the reconciler's
    // revoke arm reads, and NO caller in the control plane writes it, so it is
    // declared here directly.
    let a_shared_epoch = a_bindings
        .iter()
        .find(|resolved| resolved.database == shared_id)
        .expect("app A holds a live binding to the shared database")
        .epoch;
    let a_shared_role = database_derivation::binding_role_name(&a_shared, a_shared_epoch)
        .expect("the binding role name fits");
    let a_private_epoch = a_bindings
        .iter()
        .find(|resolved| resolved.database == private_id)
        .expect("app A holds a live binding to its own database")
        .epoch;
    let a_private_role = database_derivation::binding_role_name(&a_private, a_private_epoch)
        .expect("the binding role name fits");
    world
        .pg
        .execute(
            "UPDATE zeroship.database_bindings SET status = 'revoking' WHERE id = $1",
            &[&a_shared.as_str()],
        )
        .await
        .expect("declare app A's shared binding revoking");

    let (_, revoking) = pass(&reconciler).await;
    assert_eq!(
        revoking.bindings_revoked,
        vec![a_shared.clone()],
        "the pass withdraws exactly the declared binding: {revoking:?}"
    );
    // The role SURVIVES, which is what keeps the refusal 42501 rather than the
    // 22023 the reap above produced.
    assert_eq!(
        world_role_count(&cluster, &a_shared_role).await,
        1,
        "revoking withdraws the edges and leaves the role standing"
    );
    let revoked = count_under_role(&mut cluster, &a_shared_role, &shared_id, SHARED_COLLECTION)
        .await
        .expect_err("a revoked binding must be refused");
    let revoked = server_error(&revoked);
    assert_eq!(
        revoked.code(),
        &SqlState::INSUFFICIENT_PRIVILEGE,
        "a revoked binding is PostgreSQL's own 42501 on the role narrow: {revoked:?}"
    );
    // CONTROL: app A's OTHER binding is untouched, so the revoke reached one
    // edge and not the login.
    assert_eq!(
        count_under_role(
            &mut cluster,
            &a_private_role,
            &private_id,
            PRIVATE_COLLECTION
        )
        .await
        .expect("app A's other binding is unaffected by the revoke"),
        1,
        "revoking one binding must not disturb the same app's other database"
    );

    drop(reconciler);
    drop(cluster);
    drop(world);
    common::drain_pg().await;
}

/// How many roles the cluster carries under one name: one, or none.
async fn world_role_count(cluster: &Client, role: &str) -> i64 {
    cluster
        .query_one(
            "SELECT count(*)::bigint AS n FROM pg_roles WHERE rolname = $1",
            &[&role],
        )
        .await
        .expect("read the role catalog")
        .get("n")
}

/// A manifest whose runtime descriptor names `entries`, one entry per database.
fn manifest(entries: Vec<(&str, &DatabaseId, bool)>) -> Manifest {
    let mut manifest = Manifest::passthrough();
    manifest.runtime_descriptor = entries
        .into_iter()
        .map(|(label, database, primary)| RuntimeDescriptorEntry {
            label: label.to_owned(),
            database_id: database.clone(),
            primary,
            hash: DESCRIPTOR_HASH.to_owned(),
        })
        .collect();
    manifest
}
