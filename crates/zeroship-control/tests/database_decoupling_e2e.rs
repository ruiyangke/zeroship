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
use zeroship_control::publication::{Acceptance, CatalogError};
use zeroship_control::Registry;
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::types::LiveBinding;
use zeroship_core::{database_derivation, AppId, BindingId, DatabaseId, UserId};
use zeroship_data_orm::connection::ConnectionFactory;
use zeroship_data_orm::encryption::SuppliedProjectKeys;
use zeroship_data_orm::resolved_bindings::{ResolvedBinding, SuppliedAppBindings};
use zeroship_data_v8::service::{DbService, DbServiceConfig};
use zeroship_migrate_server::apply::{
    apply_ir_documents, ApplyMigrationsRequest, ApplyMigrationsResponse, WORKER_ROLE,
};
use zeroship_migrate_server::datastore::control::ControlStore;
use zeroship_migrate_server::datastore::{PassReport, Reconciler};
use zeroship_migrate_server::policy::ManagedPolicyConfig;
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

/// The collection a SECOND migration adds to the shared database, after both
/// apps have already deployed against a schema that does not carry it.
const CO_TENANT_COLLECTION: &str = "ledger";

/// The shared database's first migration document, under the filename the
/// engine journals it by.
const SHARED_FIRST_DOCUMENT: &str = "0001_create_notes.ir.json";

/// The document the co-tenant migration adds to that history.
const CO_TENANT_DOCUMENT: &str = "0002_create_ledger.ir.json";

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

    async fn database_status(&self, database: &DatabaseId) -> String {
        self.pg
            .query_one(
                "SELECT status FROM zeroship.databases WHERE id = $1",
                &[&database.as_str()],
            )
            .await
            .expect("the database row must exist to be read")
            .get("status")
    }

    /// The deploy hash the app row carries, which is the projection the gateway
    /// dispatches from.
    ///
    /// A deploy that returned a result and left this column where it was would
    /// satisfy every assertion about the returned value and ship nothing, so
    /// "the deploy succeeded" is read here rather than from the acceptance.
    async fn live_deploy_hash(&self, app: &AppId) -> Option<String> {
        self.pg
            .query_one(
                "SELECT deploy_hash FROM zeroship.apps WHERE id = $1",
                &[&app.as_str()],
            )
            .await
            .expect("the app row must exist to be read")
            .get("deploy_hash")
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
            "SELECT b.id AS binding_id, b.database_id, b.capability {} \
             ORDER BY b.id",
            zeroship_core::live_binding::LIVE_BINDINGS_FROM_WHERE
        );
        self.pg
            .query(&sql, &[&app.as_str()])
            .await
            .expect("read this app's live bindings")
            .iter()
            .map(|row| {
                ResolvedBinding {
                    database: DatabaseId::parse(row.get::<_, String>("database_id").as_str())
                        .expect("control stores a typed database id"),
                    binding: BindingId::parse(row.get::<_, String>("binding_id").as_str())
                        .expect("control stores a typed binding id"),
                    // Read through the one codec, exactly as the handler does.
                    // The column's CHECK admits these two spellings and nothing
                    // else, so a row this cannot read is a row the handler
                    // would refuse to serve.
                    capability: DatabaseCapability::from_wire(
                        row.get::<_, String>("capability").as_str(),
                    )
                    .expect("control stores a capability the CHECK admits"),
                }
            })
            .collect()
    }

    /// The live binding set `app`'s VERSION FEED entry carries.
    ///
    /// The other half of the same fact: `/internal/apps/{app_id}/bindings`
    /// serves the edges a worker installs, and this feed is what tells the
    /// worker to go and install them. The two are read by different statements
    /// over the same predicate, so this is the place a disagreement between
    /// them shows.
    async fn version_bindings(
        &self,
        app: &AppId,
    ) -> std::collections::BTreeMap<DatabaseId, LiveBinding> {
        self.registry
            .get_versions()
            .await
            .expect("the worker version feed")
            .get(app)
            .expect("an app control serves has a version feed entry")
            .live_bindings
            .clone()
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
            "filename": SHARED_FIRST_DOCUMENT,
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

/// The shared database's COMPLETE history, with one document the first apply
/// did not carry.
///
/// The whole history and not only the new file: `attest_complete_history`
/// (`crates/zeroship-migrate-server/src/apply.rs`) refuses an apply whose
/// supplied manifest set does not cover every version already in the engine
/// journal, so a request carrying `0002` alone would fail on coverage and
/// measure nothing about what a co-tenant's migration does to a deploy.
fn co_tenant_migration() -> Json {
    let mut request = shared_migration();
    request["documents"]
        .as_array_mut()
        .expect("the fixture request carries a document array")
        .push(json!({
            "filename": CO_TENANT_DOCUMENT,
            "body": {
                "ir_version": 1,
                "name": "create_ledger",
                "ops": [{
                    "op": "createTable",
                    "name": CO_TENANT_COLLECTION,
                    "columns": [
                        {"name": "entry", "type": "text", "nullable": false}
                    ]
                }]
            }
        }));
    request
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
///
/// `expect_skipped` is what the engine journal already covers, named by the
/// `mig_` ids the response reports rather than by filename: one `.ir.json`
/// document lowers to several plans and the response speaks in plans. Every
/// first apply names none; a request re-supplying a history so the coverage
/// attestation passes names what that history applied, and naming it is what
/// keeps "the apply advanced the journal" from being satisfied by an apply that
/// re-ran nothing and advanced nothing. Compared as a SET, because the response
/// does not promise the order the earlier apply reported.
async fn apply_into(
    tenant_url: &str,
    database: &DatabaseId,
    principal: &UserId,
    request: Json,
    label: &str,
    expect_skipped: &[String],
) -> ApplyMigrationsResponse {
    let request: ApplyMigrationsRequest =
        serde_json::from_value(request).expect("the fixture is a legal apply request");
    let tmp = tmpdir(label);
    let report = apply_ir_documents(
        tenant_url,
        &tmp,
        database,
        &request,
        &policy_config(),
        principal,
    )
    .await
    .unwrap_or_else(|error| panic!("apply into {label}: {error}"));
    assert!(
        !report.applied.is_empty(),
        "the apply must advance the journal rather than skip: {report:?}"
    );
    let mut skipped = report.skipped.clone();
    skipped.sort();
    let mut expected = expect_skipped.to_vec();
    expected.sort();
    assert_eq!(
        skipped, expected,
        "the apply must skip exactly the history it re-supplied: {report:?}"
    );
    assert!(
        report
            .applied
            .iter()
            .all(|plan| !expect_skipped.contains(plan)),
        "what this apply advanced must be disjoint from what it re-supplied, or \
         `applied` is the earlier history counted twice: {report:?}"
    );
    let _ = std::fs::remove_dir_all(tmp);
    report
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

/// Whether a capability role holds one privilege on a WHOLE table, asked of
/// PostgreSQL rather than inferred from the statements an apply sent.
///
/// The counterpart to [`has_column_privilege`], and the two answer different
/// questions: a table-level grant beside a column list does not narrow it, it
/// widens it, so "the column is reachable" is only half of what the apply's
/// emission has to satisfy.
async fn has_table_privilege(
    cluster: &Client,
    role: &str,
    database: &DatabaseId,
    table: &str,
    privilege: &str,
) -> bool {
    let qualified = format!("{}.{table}", database_derivation::schema_name(database));
    cluster
        .query_one(
            "SELECT has_table_privilege($1, $2, $3) AS granted",
            &[&role, &qualified, &privilege],
        )
        .await
        .expect("ask PostgreSQL for the privilege")
        .get("granted")
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
                    connection: ConnectionFactory::for_app_url(&spec.worker_url)
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
            world.database_status(database).await,
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

    let shared_first_apply = apply_into(
        cluster_fixture.url(),
        &shared_id,
        &principal,
        shared_migration(),
        "shared",
        &[],
    )
    .await;
    apply_into(
        cluster_fixture.url(),
        &private_id,
        &principal,
        private_migration(),
        "private",
        &[],
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
    // STAGE 3b. THE SEAM BETWEEN THE APPLY AND THE RUNTIME.
    //
    // `zeroship_migrate_server::datastore::cluster` grants the capability roles
    // `USAGE` on the schema and nothing else, because which COLUMNS each of them
    // may touch is a fact about the tables an apply creates. The apply emits it:
    // `apply::run_apply` -> `capability_grants::grant_capability_columns`, over
    // the live catalog of each database's own schema.
    //
    // Asked of PostgreSQL rather than taken from that sentence. Every creator
    // statement the isolates below run depends on this, so measuring it here is
    // what makes a later failure attributable to the runtime rather than to a
    // grant that never landed.
    // -----------------------------------------------------------------------
    let shared_rw =
        database_derivation::capability_role_name(&shared_id, DatabaseCapability::ReadWrite)
            .expect("the capability role name fits");
    let private_rw =
        database_derivation::capability_role_name(&private_id, DatabaseCapability::ReadWrite)
            .expect("the capability role name fits");
    for (role, database, table, column) in [
        (&shared_rw, &shared_id, SHARED_COLLECTION, "title"),
        (&shared_rw, &shared_id, SHARED_COLLECTION, "ssn"),
        (&private_rw, &private_id, PRIVATE_COLLECTION, "kind"),
    ] {
        for privilege in ["SELECT", "INSERT"] {
            assert!(
                has_column_privilege(&cluster, role, database, table, column, privilege).await,
                "the apply must leave the readwrite capability role holding \
                 {privilege} on {table}.{column}, or every creator statement below \
                 fails on the grant rather than on what it is about"
            );
        }
    }
    // AND NO WIDER. A table-level `GRANT SELECT` beside a column list returns
    // every column the list withheld, so the column answers above are only half
    // the property: the same role must hold no table-level SELECT at all.
    // `DELETE` is the one verb with no column form, and is the control that
    // keeps this from passing over a role that holds nothing.
    for (role, database, table) in [
        (&shared_rw, &shared_id, SHARED_COLLECTION),
        (&private_rw, &private_id, PRIVATE_COLLECTION),
    ] {
        assert!(
            !has_table_privilege(&cluster, role, database, table, "SELECT").await,
            "the apply must grant SELECT on {table} column by column, never at the table"
        );
        assert!(
            has_table_privilege(&cluster, role, database, table, "DELETE").await,
            "DELETE has no column form, so it is the one verb granted at {table}"
        );
    }
    // The platform's own table in the same schema keeps its narrower grant:
    // `INSERT` so an unmask audit row lands, and no `SELECT`, so no session can
    // read another actor's audit trail back through the app.
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
        "the unmask audit table's own grant must survive the creator-table emission"
    );
    assert!(
        !has_column_privilege(
            &cluster,
            &shared_rw,
            &shared_id,
            zeroship_migrate_server::provisioning::AUDIT_UNMASK_TABLE,
            "collection",
            "SELECT",
        )
        .await,
        "the creator-table emission must not sweep the platform table and hand it \
         a SELECT over every actor's audit trail"
    );

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
    // STAGE 4b. ARM (g): a migration one app applies to the SHARED database
    // does not fail the other app's deploy.
    //
    // It runs here, between the deploys and the isolates, because it needs both
    // apps live on one database and it leaves the shared schema grown by one
    // relation the stages below neither read nor forbid. The refusal directly
    // above is its control, differing in one variable.
    // -----------------------------------------------------------------------
    a_migration_one_app_applies_to_a_shared_database_does_not_fail_the_other_apps_deploy(
        CoTenantDeploy {
            world: &world,
            cluster: &cluster,
            tenant_url: cluster_fixture.url(),
            principal: &principal,
            applying: Bound {
                app: &app_a,
                binding: &a_shared,
            },
            deploying: Bound {
                app: &app_b,
                binding: &b_shared,
            },
            shared: &shared_id,
            already_applied: &shared_first_apply.applied,
        },
    )
    .await;

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
    // The capability control serves is the one the bind declared. Every bind in
    // this world asked for `CAPABILITY_READWRITE`, and the isolates below then
    // write, so a projection that served the other one would leave every write
    // in this test refused before a statement ran.
    for resolved in a_bindings.iter().chain(b_bindings.iter()) {
        assert_eq!(
            resolved.capability,
            DatabaseCapability::from_wire(CAPABILITY_READWRITE)
                .expect("the constant is one of the two stored spellings"),
            "control must serve the capability the bind declared"
        );
    }
    // The VERSION FEED reports the same set, and it is the feed that tells a
    // resident worker to go and re-read the edges above: an isolate captures
    // the bindings its sessions narrow with while it builds, so a feed that
    // disagreed with this endpoint would either never ask for the re-read or
    // ask on every poll for a set it already holds. Both apps, because a feed
    // that answered with one app's set for every app would satisfy either
    // alone.
    for (app, bindings) in [(&app_a, &a_bindings), (&app_b, &b_bindings)] {
        assert_eq!(
            world.version_bindings(app).await,
            bindings
                .iter()
                .map(|resolved| (
                    resolved.database.clone(),
                    LiveBinding {
                        binding: resolved.binding.clone(),
                        capability: resolved.capability,
                    }
                ))
                .collect::<std::collections::BTreeMap<_, _>>(),
            "the version feed and the binding endpoint must not disagree about \
             which databases this app binds, nor about the EDGE it binds each \
             of them through - the edge is what the session role is derived \
             from, so a feed reporting another one would ask the worker to \
             install a set the endpoint does not serve"
        );
    }

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
    let b_role =
        database_derivation::binding_role_name(&b_shared).expect("the binding role name fits");
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
    // The data path reads a REVOKED binding off the SQLSTATE `SET LOCAL ROLE`
    // returns - 42501 means the role stands and this login may not assume it,
    // 22023 means there is no such role - and the reconciler's revoke arm
    // exists to keep the first reachable: it withdraws both edges and leaves
    // the role standing.
    //
    // WHAT THE CONTROL SURFACE ACTUALLY DOES IS THE OTHER ONE.
    // `databases::unbind_database` DELETES the binding row, and a role no
    // declaration names is REAPED, so the refusal a creator reaches through the
    // only call a creator has is the generic 22023. The two halves below
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
    // AND THE VERSION FEED SAYS SO. The empty set is a statement rather than an
    // absence, and it is the only thing a worker holding app B's isolate ever
    // sees of this unbind: nothing else about the app moved, so without it the
    // isolate keeps composing the role the unbind retired until the worker
    // process restarts. Its control is app A, still bound, whose feed entry
    // must NOT have emptied.
    assert!(
        world.version_bindings(&app_b).await.is_empty(),
        "an unbound app's version feed entry carries the empty set"
    );
    assert_eq!(
        world.version_bindings(&app_a).await.len(),
        2,
        "the control: unbinding app B leaves app A's feed entry alone"
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
        "a REAPED role answers 22023, the generic bad-GUC code; the terminal 42501 \
         is not what an unbind produces: {reaped:?}"
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
    assert!(
        a_bindings
            .iter()
            .any(|resolved| resolved.database == shared_id),
        "app A holds a live binding to the shared database"
    );
    let a_shared_role =
        database_derivation::binding_role_name(&a_shared).expect("the binding role name fits");
    assert!(
        a_bindings
            .iter()
            .any(|resolved| resolved.database == private_id),
        "app A holds a live binding to its own database"
    );
    let a_private_role =
        database_derivation::binding_role_name(&a_private).expect("the binding role name fits");
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

/// One app's side of a shared database: the app, and the binding it holds to
/// that database. Carried as a pair because an app id alone cannot say which
/// edge was converged for it.
struct Bound<'a> {
    app: &'a AppId,
    binding: &'a BindingId,
}

/// Everything arm (g) needs to measure, in the spec shape [`Dispatch`] already
/// uses in this file.
struct CoTenantDeploy<'a> {
    world: &'a World,
    cluster: &'a Client,
    tenant_url: &'a str,
    principal: &'a UserId,
    /// The app that applies the migration.
    applying: Bound<'a>,
    /// The app that deploys after it and must not be failed by it.
    deploying: Bound<'a>,
    shared: &'a DatabaseId,
    /// What the FIRST apply into `shared` reported as applied, which the second
    /// must re-supply and skip.
    already_applied: &'a [String],
}

/// ARM (g) OF THE MANDATORY REGRESSION SET: on a database two apps share, one
/// app's migration does not fail the other app's deploy.
///
/// This is the property the deleted descriptor-equality gate destroyed. That
/// gate compared the manifest's `runtime_descriptor.hash` against the hash on
/// the app's newest applied row, so ANY migration - a purely additive one
/// included - invalidated the build of every other app bound to that database
/// while every one of them kept running correctly.
/// `publication::catalog::admit_bindings` now compares no schema at all: it
/// reads `database_bindings` and `databases` and nothing else, so the property
/// is free of a mechanism rather than guarded by one.
///
/// # This asserts that something does NOT happen, so it passes for free if the
/// # scenario never arises
///
/// Five ways it could be green over a tree where the property is false, and the
/// assertion that forecloses each:
///
/// - **The two apps are not actually on one database.** Two independent
///   databases make a co-tenant migration a migration of nobody's schema.
///   Foreclosed by comparing the two database ids for EQUALITY, each read out
///   of its own app's live-binding projection rather than from this test's
///   variables, and by `assert_ne!` on the two app ids.
/// - **A binding is not live, so the deploy was never admitted on it.**
///   Foreclosed by asserting the three conjuncts
///   `zeroship_core::live_binding::LIVE_BINDINGS_FROM_WHERE` spells - the
///   binding `active`, `observed_generation >= generation`, the database
///   `active` - which are the three `admit_bindings` re-implements over the
///   ORM, read from the rows for both apps.
/// - **The migration applied nothing.** An apply that skipped every document
///   leaves the schema where it was and there is no co-tenant change to survive.
///   Foreclosed by requiring `applied` to be non-empty and disjoint from the
///   plans the FIRST apply into this database reported, and `skipped` to be
///   exactly those plans - and then by asking `PostgreSQL` rather than the
///   service: the shared schema carries a relation after the apply that it
///   demonstrably did not carry before.
/// - **The artifact named no database**, which returns from `admit_bindings` on
///   an empty list before any row is read - the shape
///   `deploy_declaring_no_database_needs_no_binding_and_goes_live`
///   (`crates/zeroship-control/tests/deploy_http_test.rs`) measures on purpose.
///   Foreclosed on the VERIFIED deployment rather than on the manifest, because
///   `VerifiedDeployment::databases` is the slice `catalog::accept` hands to
///   `admit_bindings` and a manifest entry that did not survive verification
///   would leave that slice empty.
/// - **The deploy returned a result and shipped nothing.** Foreclosed by
///   requiring `Acceptance::Accepted` rather than the receipt replay an
///   identical hash answers with, and by reading the app row's `deploy_hash`
///   back and finding the new build there.
///
/// # The control
///
/// The refusal that differs from this in one variable is in the same exercise,
/// a few lines above the call: the same app deploying the same shape with ONE
/// more database - the one it holds no binding to - is refused
/// `CatalogError::DatabaseNotBound`. The HTTP-surface form of that control,
/// with its status, body and remedy, is
/// `deploy_naming_an_unbound_database_is_refused_and_names_the_binding_call`
/// and its own control in `crates/zeroship-control/tests/deploy_http_test.rs`;
/// neither is restated here.
async fn a_migration_one_app_applies_to_a_shared_database_does_not_fail_the_other_apps_deploy(
    scene: CoTenantDeploy<'_>,
) {
    let CoTenantDeploy {
        world,
        cluster,
        tenant_url,
        principal,
        applying,
        deploying,
        shared,
        already_applied,
    } = scene;
    let (applying, applying_binding) = (applying.app, applying.binding);
    let (deploying, deploying_binding) = (deploying.app, deploying.binding);
    // PRECONDITION 1. TWO APPS, ONE DATABASE. Read each app's own live-binding
    // projection and compare the DATABASE IDS to each other. "Each is
    // non-empty" would hold over two separate databases, which is the scenario
    // in which this whole arm asserts nothing.
    let applying_live = world.live_bindings(applying).await;
    let deploying_live = world.live_bindings(deploying).await;
    let applying_edge = applying_live
        .iter()
        .find(|resolved| &resolved.binding == applying_binding)
        .expect("the applying app holds its shared binding in the live projection");
    let deploying_edge = deploying_live
        .iter()
        .find(|resolved| &resolved.binding == deploying_binding)
        .expect("the deploying app holds its shared binding in the live projection");
    assert_eq!(
        applying_edge.database, deploying_edge.database,
        "the two apps must be bound to the SAME database, or a migration by one \
         is not a migration of the other's schema and this arm measures nothing"
    );
    assert_eq!(
        &applying_edge.database, shared,
        "and that one database is the shared one this stage migrates"
    );
    assert_ne!(
        applying, deploying,
        "two apps, not one app named twice: the deploy below must be the app that \
         did NOT apply the migration"
    );
    assert_ne!(
        applying_binding, deploying_binding,
        "two bindings, one per app, and not one binding read twice"
    );

    // PRECONDITION 2. BOTH BINDINGS ARE LIVE BY THE PREDICATE DEPLOY USES.
    // `admit_bindings` re-implements `LIVE_BINDINGS_FROM_WHERE` over the ORM,
    // and its three conjuncts are these. A deploy admitted over a binding that
    // was not live would be admitted for a reason this arm is not about.
    let mut rows_before = Vec::new();
    for (binding, what) in [
        (applying_binding, "the applying app's binding"),
        (deploying_binding, "the deploying app's binding"),
    ] {
        let row = world.binding_row(binding).await;
        let (status, generation, observed) = &row;
        assert_eq!(status, "active", "{what} must be active");
        assert!(
            observed >= generation,
            "{what}: a live binding's observed generation has caught up \
             ({observed} >= {generation})"
        );
        rows_before.push((binding, what, row));
    }
    assert_eq!(
        world.database_status(shared).await,
        "active",
        "the shared database must be active, the third conjunct of LIVE"
    );

    // PRECONDITION 3. THE DEPLOYING APP IS ALREADY LIVE ON A BUILD THAT
    // PREDATES THE MIGRATION. Without this the deploy below could be a first
    // deploy, and "its deploy did not start failing" would have no before.
    let deployed_before = world
        .live_deploy_hash(deploying)
        .await
        .expect("the deploying app went live earlier in this exercise");

    // PRECONDITION 4. THE SCHEMA BEFORE, asked of PostgreSQL. Exact, so the
    // relation the apply adds is one that demonstrably did not exist.
    assert_eq!(
        creator_tables(cluster, shared).await,
        vec![SHARED_COLLECTION.to_owned()],
        "before the co-tenant migration the shared database carries one collection"
    );

    // THE CO-TENANT MIGRATION, by the app that is NOT deploying below.
    //
    // The history it re-supplies has to be a real one before `apply_into` can
    // hold it to anything: an empty expectation would be satisfied by an apply
    // that skipped nothing because there was nothing to skip.
    assert!(
        !already_applied.is_empty(),
        "the first apply into the shared database reported the plans it applied"
    );
    let report = apply_into(
        tenant_url,
        shared,
        principal,
        co_tenant_migration(),
        "shared-co-tenant",
        already_applied,
    )
    .await;
    assert!(
        !report.applied.is_empty(),
        "the co-tenant migration advanced the journal: {report:?}"
    );

    // AND THE SCHEMA REALLY MOVED. A non-empty `applied` is the service's own
    // report of itself; this is the catalog.
    assert_eq!(
        creator_tables(cluster, shared).await,
        vec![
            CO_TENANT_COLLECTION.to_owned(),
            SHARED_COLLECTION.to_owned()
        ],
        "the shared database now carries a relation it did not carry before"
    );
    assert_eq!(
        world.database_status(shared).await,
        "active",
        "an apply does not take the database out of the state that makes a \
         binding live - which is the only way it could fail a deploy from here"
    );
    // AND IT MOVED NOTHING LIVENESS DEPENDS ON. The deploy below succeeding
    // because the apply left every binding row exactly where it was is the
    // mechanism; asserting it here means the arm measures that rather than
    // reasoning about it, and it closes the window where a rotation that
    // bumped `generation` would refuse every co-tenant until a pass caught up.
    for (binding, what, before) in &rows_before {
        assert_eq!(
            &world.binding_row(binding).await,
            before,
            "{what} must be exactly what it was before the apply"
        );
    }

    // THE DEPLOY, by the app that did not apply. Its descriptor is the one it
    // built against BEFORE the migration; only the compiler label differs from
    // the artifact already live, so this is a new deployment rather than a
    // replay of the stored receipt.
    let redeploy = manifest_labelled(
        "after-a-co-tenants-migration",
        vec![(SHARED_LABEL, shared, true)],
    );
    assert!(
        redeploy
            .runtime_descriptor
            .iter()
            .all(|entry| entry.hash == DESCRIPTOR_HASH),
        "the artifact carries the descriptor hash of the build that went live \
         before the migration, which is exactly what the deleted equality gate \
         would now refuse"
    );
    // THE VALUE ADMISSION ACTUALLY RECEIVES. `admit_bindings` returns Ok on an
    // empty list before it reads a single row, so an artifact whose declared
    // set came out empty would make every assertion below pass over a deploy
    // that verified nothing. `accept` reads exactly this slice, so this is the
    // one place the claim can be made rather than inferred from the manifest.
    let deployment = common::deployments::verified(redeploy);
    assert_eq!(
        deployment.databases(),
        std::slice::from_ref(shared),
        "the verified artifact declares the shared database, and only it, to the \
         admission that is about to read binding rows for it"
    );
    let accepted = world
        .registry
        .deploy(common::deployments::command(
            deploying,
            &world.owner,
            deployment,
        ))
        .await
        .expect(
            "a migration another app applied to the shared database must not fail \
             this app's deploy",
        );
    let Acceptance::Accepted(result) = accepted else {
        panic!("the deploy must be admitted, not replayed from a receipt: {accepted:?}");
    };
    assert_ne!(
        result.deploy_hash, deployed_before,
        "the admitted artifact is a new build and not the one already live"
    );
    assert!(
        result.lifecycle_revision.is_some(),
        "an active app's admitted deploy allocates a lifecycle revision: {result:?}"
    );
    assert_eq!(
        world.live_deploy_hash(deploying).await.as_deref(),
        Some(result.deploy_hash.as_str()),
        "and it is LIVE in the projection the gateway dispatches from, which is \
         the half a returned result cannot carry"
    );
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

/// The same manifest told apart by its compiler label.
///
/// `catalog::accept` answers a second deploy of an identical hash from the
/// stored receipt, and a replay performs no admission of its own. A label makes
/// the second deploy a DIFFERENT artifact, so it is admitted rather than
/// replayed - while its `runtime_descriptor` keeps the hash and the database set
/// of the build that went live before the migration.
fn manifest_labelled(label: &str, entries: Vec<(&str, &DatabaseId, bool)>) -> Manifest {
    let mut manifest = manifest(entries);
    manifest.metadata.compiler = Some(label.to_owned());
    manifest
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
