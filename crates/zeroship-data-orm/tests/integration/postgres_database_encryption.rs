//! At-rest column encryption keyed on the DATABASE, end to end.
//!
//! The two properties the decoupling has to create are opposite in direction,
//! and both are silent when they are wrong - a read that fails over data the
//! reader is entitled to, and a read that succeeds over data lifted out of
//! another database. Neither can be measured on a hand-rolled fixture: the
//! schemas, the three roles per database and the two grant edges per binding
//! are all created here by `zeroship_migrate_server::datastore::cluster`, the
//! same functions the per-cluster reconciler calls, and every statement runs
//! through `zeroship_data_orm::orm::Database` under the binding role a session
//! narrows to.
//!
//! **Both arms pair the refusal with the permitted read it differs from.** An
//! `encryption_aead_failed` is produced by a wrong key AND by a wrong AAD, so
//! the lifted-ciphertext arm additionally decomposes the refusal at the crypto
//! boundary, holding one of the two constant at a time. Without that it would
//! report only "AEAD said no", which is true of a design that had stopped
//! binding the database at all.
//!
//! **The role DDL is cluster-shared, and that is not a convenience.**
//! `pg_authid` and `pg_auth_members` are shared by every database on a server,
//! so each arm serializes the migrating service's cluster-global convergence
//! against the worktree's other cases on the shared bare server they lease.
//!
//! **No arm here writes `zeroship_worker`'s own password.** That login is
//! cluster-global, so on the worktree's shared bare server its password
//! belongs to no single suite: the platform corpus
//! (`db/migrations-ts/20260702000100_schema_roles_extensions.ts`) and
//! `zeroship_workflow_testkit::journal_server::ensure_roles` each create it
//! with the dev password `zeroship_worker` only when it is missing, and a
//! sibling suite that reaches the role first may write anything else. Every arm
//! below instead mints a login of its own, a member of `zeroship_worker` and
//! nothing else, which reaches a live binding's privileges by assuming the
//! worker login rather than by being it.

use zeroship_testkit::postgres::server::Postgres;

use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use compio_postgres::{Client, NoTls};

use zeroship_core::database_derivation;
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::{typed_id, BindingId, DatabaseId};
use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::encryption::{
    self, AeadKey, KeyStore, ProjectKeySource, SuppliedProjectKeys,
};
use zeroship_data_orm::error::DbError;
use zeroship_data_orm::orm::{Database, Output};
use zeroship_data_orm::schema::{CollectionSchema, ColumnSchema, LogicalType, Schema};
use zeroship_data_orm::value;
use zeroship_migrate_server::apply::WORKER_ROLE;
use zeroship_migrate_server::datastore::cluster;

/// The deploy pin is `postgres:16` (`deploy/compose/docker-compose.yml`), and
/// the grant options the whole ladder rests on do not exist below it.
const MINIMUM_SERVER_VERSION_NUM: i32 = 160_000;

/// The one project both apps belong to. A database is owned by a project and a
/// binding is fenced to that project's apps, so co-binding-holders always
/// share a root key; the fixture says so explicitly rather than relying on it.
const PROJECT: &str = "prj_fixture";

/// The project root key the host supplies. Hexadecimal, 32 bytes.
const PROJECT_ROOT_HEX: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

/// The tenant that writes.
const APP_A: &str = "app_writer";

/// A DIFFERENT tenant, bound to the same database with its own edge.
const APP_B: &str = "app_reader";

/// The collection, the encrypted column and the row every arm uses.
const COLLECTION: &str = "notes";
const COLUMN: &str = "ssn";
const ROW_PK: &str = "row_a";
const PLAINTEXT: &str = "123-45-6789";

/// The advisory key that serializes cluster-global role provisioning.
///
/// Advisory locks are scoped to the database that takes them, so the lock is
/// taken on the server's `postgres` database while the role DDL runs in the
/// case's own database. `pg_authid` and `pg_auth_members` are cluster state,
/// not case state: the migration service owns them in production and converges
/// one cluster at a time. Cases sharing the worktree's server must not alter
/// the same role row at once, or PostgreSQL reports `tuple concurrently
/// updated`.
const ROLE_PROVISIONING_LOCK: i64 = 0x7a73_5f72_6f6c_655f;

async fn lock_role_provisioning(case_url: &str) -> Client {
    let mut url = url::Url::parse(case_url).expect("the fixture URL parses");
    url.set_path("postgres");
    let lock = connect(url.as_str()).await;
    lock.batch_execute(&format!("SELECT pg_advisory_lock({ROLE_PROVISIONING_LOCK})"))
        .await
        .expect("take the cluster role-provisioning lock");
    lock
}

async fn unlock_role_provisioning(lock: &Client) {
    lock.batch_execute(&format!(
        "SELECT pg_advisory_unlock({ROLE_PROVISIONING_LOCK})"
    ))
    .await
    .expect("release the cluster role-provisioning lock");
}

/// Connect a raw client and drive its protocol loop.
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

/// Refuse a server older than the pin before measuring anything.
async fn require_pinned_major(admin: &Client) {
    let version: i32 = admin
        .query_one("SELECT current_setting('server_version_num')::int", &[])
        .await
        .expect("the server reports its version")
        .get(0);
    assert!(
        version >= MINIMUM_SERVER_VERSION_NUM,
        "this target describes PostgreSQL {MINIMUM_SERVER_VERSION_NUM} and above; \
         the fixture server reports server_version_num {version}"
    );
}

/// The collection both databases declare: a text primary key and one column
/// the descriptor marks `encrypted`.
///
/// Both databases declaring the SAME collection and column is what makes the
/// lifted-ciphertext arm about the database rather than about a name mismatch.
fn notes_schema() -> Schema {
    let mut id = ColumnSchema::new(LogicalType::Text);
    id.primary_key = true;
    let mut ssn = ColumnSchema::new(LogicalType::Text);
    ssn.encrypted = true;
    Schema::new(vec![(
        COLLECTION.into(),
        CollectionSchema::new([("id".into(), id), (COLUMN.into(), ssn)]),
    )])
}

/// What an APPLY does over a converged schema, not what the reconciler does.
///
/// The reconciler mints the roles and grants `USAGE` on the schema; which
/// COLUMNS a capability may touch comes from the owner's own migration IR.
async fn seed_table(admin: &Client, database: &DatabaseId) {
    let schema = database_derivation::schema_name(database);
    let readwrite =
        database_derivation::capability_role_name(database, DatabaseCapability::ReadWrite)
            .expect("the fixture capability role name fits");
    admin
        .batch_execute(&format!(
            "CREATE TABLE \"{schema}\".\"{COLLECTION}\" \
                 (id text PRIMARY KEY, \"{COLUMN}\" bytea);
             GRANT SELECT (id, \"{COLUMN}\"), INSERT (id, \"{COLUMN}\"), \
                 UPDATE (id, \"{COLUMN}\") \
                 ON \"{schema}\".\"{COLLECTION}\" TO \"{readwrite}\";"
        ))
        .await
        .expect("an apply's column grants over a converged schema");
}

/// The URL a minted per-case login opens, built from the fixture's own.
fn login_url(base: &str, login: &str) -> String {
    let mut url = url::Url::parse(base).expect("the fixture URL parses");
    url.set_username(login)
        .expect("the fixture URL accepts a username");
    url.set_password(Some(login))
        .expect("the fixture URL accepts a password");
    url.to_string()
}

/// Mint a login unique to this case, a member of `role` and of nothing else,
/// so a session that connects as it reaches `role`'s privileges by assuming
/// the role rather than by being it. See the module doc for why no arm
/// connects as `role` itself on the shared bare server.
async fn mint_assuming_login(admin: &Client, role: &str) -> String {
    let login = typed_id::generate("wpl");
    admin
        .batch_execute(&format!(
            "CREATE ROLE \"{login}\" LOGIN PASSWORD '{login}'; GRANT \"{role}\" TO \"{login}\";"
        ))
        .await
        .expect("mint a per-case login that assumes the platform login");
    login
}

/// Drop a login [`mint_assuming_login`] minted, so it does not linger on the
/// server after this case ends.
async fn drop_login(admin: &Client, login: &str) {
    admin
        .batch_execute(&format!("DROP ROLE \"{login}\""))
        .await
        .expect("drop the per-case login");
}

/// A cluster the reconciler converged: two databases, and three bindings whose
/// shape is the whole subject - two apps on ONE database, and one app on TWO.
struct Cluster {
    url: String,
    /// Bound by both apps.
    shared: DatabaseId,
    /// Bound by `APP_A` alone.
    other: DatabaseId,
    a_on_shared: BindingId,
    b_on_shared: BindingId,
    a_on_other: BindingId,
    keys: Arc<SuppliedProjectKeys>,
    admin: Client,
    /// The per-case login this cluster minted, dropped by [`Cluster::teardown`].
    /// Never the worker login itself: see the module doc.
    login: String,
}

impl Cluster {
    async fn build(url: String) -> Self {
        let mut admin = connect(&url).await;
        require_pinned_major(&admin).await;
        let lock = lock_role_provisioning(&url).await;
        cluster::apply_bootstrap_corpus(&mut admin)
            .await
            .expect("the datastore bootstrap corpus applies");

        let shared = DatabaseId::mint();
        let other = DatabaseId::mint();
        assert_ne!(shared, other, "the control: two mints are two databases");
        let a_on_shared = BindingId::mint();
        let b_on_shared = BindingId::mint();
        let a_on_other = BindingId::mint();

        for database in [&shared, &other] {
            cluster::converge_database(&mut admin, database)
                .await
                .expect("the reconciler converges the database");
            seed_table(&admin, database).await;
        }
        for (binding, database) in [
            (&a_on_shared, &shared),
            (&b_on_shared, &shared),
            (&a_on_other, &other),
        ] {
            cluster::grant_binding(&admin, binding, database, DatabaseCapability::ReadWrite)
                .await
                .expect("the reconciler grants the binding's two edges");
        }

        let login = mint_assuming_login(&admin, WORKER_ROLE).await;
        unlock_role_provisioning(&lock).await;

        // One project, one root key, two tenants - exactly what Control serves
        // when a project binds two of its apps to one of its databases.
        let keys = Arc::new(
            SuppliedProjectKeys::new()
                .with_hex(PROJECT, PROJECT_ROOT_HEX)
                .expect("the fixture project root key parses"),
        );
        for app in [APP_A, APP_B] {
            keys.bind_app(app, PROJECT)
                .expect("the host authorizes the app against its project");
        }

        Self {
            url: login_url(&url, &login),
            shared,
            other,
            a_on_shared,
            b_on_shared,
            a_on_other,
            keys,
            admin,
            login,
        }
    }

    /// Drop the per-case login this cluster minted, before the caller drops
    /// the cluster itself.
    async fn teardown(&self) {
        drop_login(&self.admin, &self.login).await;
    }

    /// Run `body` against this cluster, always dropping the minted login before
    /// the cluster itself drops - including when `body` panics. The body runs
    /// under `catch_unwind`, so a failing assertion cannot skip the teardown
    /// and leave the login on the shared bare server.
    async fn run(self, body: impl AsyncFnOnce(&Self)) {
        let outcome = AssertUnwindSafe(body(&self)).catch_unwind().await;
        self.teardown().await;
        if let Err(panic) = outcome {
            drop(self);
            std::panic::resume_unwind(panic);
        }
    }

    fn binding(app: &str, database: &DatabaseId, edge: &BindingId) -> DbBinding {
        DbBinding::to_database(
            app,
            "deploy_fixture",
            database.clone(),
            edge.clone(),
            DatabaseCapability::ReadWrite,
        )
        .expect("the fixture ids compose a legal role name")
    }

    /// Open the ORM on one binding, with the host's project keys installed.
    async fn open(&self, binding: DbBinding) -> Database {
        Database::connect(
            binding,
            zeroship_data_orm::ConnectOptions::new(
                &self.url,
                ProjectKeySource::supplied(self.keys.clone()),
            ),
            notes_schema(),
        )
        .await
        .expect("open the database under its binding")
    }

    /// The key store the production path resolves through.
    fn key_store(&self) -> KeyStore {
        KeyStore::new(ProjectKeySource::supplied(self.keys.clone()))
    }

    /// The stored ciphertext for `ROW_PK`, read with the admin client so the
    /// bytes are the ones on disk rather than anything the ORM produced.
    async fn stored_ciphertext(&self, database: &DatabaseId) -> Vec<u8> {
        let schema = database_derivation::schema_name(database);
        let rows = self
            .admin
            .query_text_params(
                &format!(
                    "SELECT encode(\"{COLUMN}\", 'hex') AS hex \
                       FROM \"{schema}\".\"{COLLECTION}\" WHERE id = $1"
                ),
                &[ROW_PK],
            )
            .await
            .expect("read the stored bytes");
        assert_eq!(rows.len(), 1, "the row must be present to be read");
        let hex: String = rows[0].get("hex");
        assert!(!hex.is_empty(), "the column must hold bytes, not NULL");
        let mut out = Vec::with_capacity(hex.len() / 2);
        for pair in hex.as_bytes().chunks(2) {
            let pair = std::str::from_utf8(pair).expect("hex is ASCII");
            out.push(u8::from_str_radix(pair, 16).expect("hex digits"));
        }
        out
    }

    /// Plant `bytes` into `database` at the same collection, column and row.
    async fn plant(&self, database: &DatabaseId, bytes: &[u8]) {
        use std::fmt::Write as _;
        let schema = database_derivation::schema_name(database);
        let mut hex = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
        }
        self.admin
            .execute(
                &format!(
                    "INSERT INTO \"{schema}\".\"{COLLECTION}\" (id, \"{COLUMN}\") \
                     VALUES ($1, decode($2, 'hex'))"
                ),
                &[&ROW_PK, &hex.as_str()],
            )
            .await
            .expect("plant the lifted ciphertext");
    }
}

/// Write the one encrypted row through the ORM, under `database`'s binding.
async fn write_row(database: &Database) {
    database
        .collection(COLLECTION)
        .expect("the collection is declared")
        .insert(value!({ "id": ROW_PK, COLUMN: PLAINTEXT }))
        .await
        .expect("the write path encrypts and stores the row");
}

/// Read the encrypted column back through the ORM.
async fn read_column(database: &Database) -> Result<String, DbError> {
    let found = database
        .collection(COLLECTION)?
        .find(value!({ "id": ROW_PK }), value!({}))
        .await?;
    let Output::Rows(rows) = found else {
        panic!("find must return rows");
    };
    assert_eq!(rows.len(), 1, "the row must be present to be read");
    rows[0][COLUMN]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| DbError::internal("the column decoded as something other than text"))
}

/// Release the driver's sockets before the container that serves them.
async fn drain() {
    assert!(
        compio_postgres::drain_connections(std::time::Duration::from_secs(10)).await,
        "the test's PostgreSQL connections must close before the container stops"
    );
}

/// Assert an error is the AEAD refusal and not something upstream of it.
fn assert_aead_refusal(error: &DbError, what: &str) {
    match error {
        DbError::ValidationFailed { code, .. } => assert_eq!(
            *code, "encryption_aead_failed",
            "{what}: expected the AEAD refusal, got {error}"
        ),
        other => panic!("{what}: expected ValidationFailed, got {other:?}"),
    }
}

/// **The first property.** Two apps bound to ONE database read each other's
/// encrypted rows.
///
/// This is the capability the decoupling exists for and the one app keying
/// broke: app B is a different tenant with its own binding and its own role,
/// entitled by that binding to the same rows, and under app keying its read
/// failed as an AEAD error over data it was entitled to.
///
/// Three controls, because this arm could otherwise pass for three wrong
/// reasons: app A reads back what it wrote, so the write path worked; the
/// stored bytes are neither the plaintext nor readable under a foreign key, so
/// encryption is not a pass-through; and app B's own binding reaches the
/// schema, so the read is the ORM's and not a shortcut.
#[compio::test]
async fn two_apps_bound_to_one_database_read_each_others_encrypted_rows() {
    let postgres = Postgres::start();
    let cluster = Cluster::build(postgres.url()).await;
    cluster
        .run(async |cluster| {
            let writer = cluster
                .open(Cluster::binding(
                    APP_A,
                    &cluster.shared,
                    &cluster.a_on_shared,
                ))
                .await;
            write_row(&writer).await;

            // CONTROL 1: the writer reads back its own row, so the write path
            // stored something this stack can recover.
            assert_eq!(
                read_column(&writer)
                    .await
                    .expect("the writer reads its row"),
                PLAINTEXT
            );

            // CONTROL 2: what is on disk is ciphertext. Without this, a design
            // that skipped encryption would satisfy every other assertion
            // here.
            let stored = cluster.stored_ciphertext(&cluster.shared).await;
            assert_ne!(
                stored.as_slice(),
                PLAINTEXT.as_bytes(),
                "the column must hold ciphertext"
            );
            let foreign = AeadKey { k_enc: [0x5a; 32] };
            let aad =
                encryption::canonical_aad(&cluster.shared, COLLECTION, COLUMN, ROW_PK.as_bytes());
            assert!(
                encryption::decrypt(&foreign, &stored, &aad).is_err(),
                "the stored bytes must be authenticated under the derived key, not any key"
            );

            // THE SUBJECT, differing in one variable: a different tenant, its
            // own binding, the same database.
            let reader = cluster
                .open(Cluster::binding(
                    APP_B,
                    &cluster.shared,
                    &cluster.b_on_shared,
                ))
                .await;
            assert_eq!(
                read_column(&reader)
                    .await
                    .expect("a co-binding-holder must read the plaintext it is entitled to"),
                PLAINTEXT
            );

            // CONTROL 3: the two tenants really are two, so the read above is
            // not the writer's handle under another name.
            assert_ne!(
                writer.binding().app_id(),
                reader.binding().app_id(),
                "the control: two tenants"
            );
            assert_ne!(
                writer.binding().session_role(),
                reader.binding().session_role(),
                "the control: two binding roles, so two grant edges"
            );

            drop(writer);
            drop(reader);
        })
        .await;
    drain().await;
}

/// **The second property.** A ciphertext lifted out of one database does not
/// verify in another, for one app holding both.
///
/// The bytes are taken off disk in the first database and planted in the
/// second under the SAME collection, column and row id, so nothing about the
/// context differs except which database the row now sits in.
///
/// A wrong key and a wrong AAD produce the same `encryption_aead_failed`, and
/// here BOTH differ - so the end-to-end refusal alone cannot say which fence
/// caught it. The arm therefore decomposes it at the crypto boundary, holding
/// one variable constant at a time, and pins that each fence refuses alone.
#[compio::test]
async fn a_ciphertext_lifted_into_another_database_does_not_verify() {
    let postgres = Postgres::start();
    let cluster = Cluster::build(postgres.url()).await;
    cluster
        .run(async |cluster| {
            let here = cluster
                .open(Cluster::binding(
                    APP_A,
                    &cluster.shared,
                    &cluster.a_on_shared,
                ))
                .await;
            write_row(&here).await;
            let lifted = cluster.stored_ciphertext(&cluster.shared).await;
            cluster.plant(&cluster.other, &lifted).await;

            // CONTROL: the same app reads the row where it was written. The
            // blob is intact and the app is entitled to it, so the refusal
            // below is about the database and not about the row, the key store,
            // or the binding.
            assert_eq!(
                read_column(&here).await.expect("the writer reads its row"),
                PLAINTEXT
            );

            // THE SUBJECT: the SAME app, its OWN second database, the planted
            // bytes.
            let there = cluster
                .open(Cluster::binding(APP_A, &cluster.other, &cluster.a_on_other))
                .await;
            let refused = read_column(&there)
                .await
                .expect_err("a lifted ciphertext must not verify in another database");
            assert_aead_refusal(&refused, "the lifted read");

            // WHICH FENCE REFUSED IT. Both the key and the AAD moved with the
            // database, so the refusal above is over-determined. Decompose it.
            let keys = cluster.key_store();
            let key_here = keys
                .resolve(APP_A, &cluster.shared)
                .await
                .expect("the host supplied this project's root");
            let key_there = keys
                .resolve(APP_A, &cluster.other)
                .await
                .expect("the host supplied this project's root");
            let aad_here =
                encryption::canonical_aad(&cluster.shared, COLLECTION, COLUMN, ROW_PK.as_bytes());
            let aad_there =
                encryption::canonical_aad(&cluster.other, COLLECTION, COLUMN, ROW_PK.as_bytes());
            assert_ne!(key_here.k_enc, key_there.k_enc, "the salt is the database");
            assert_ne!(aad_here, aad_there, "the AAD carries the database");

            // The baseline: under the database it was written in, the blob
            // decrypts.
            assert_eq!(
                encryption::decrypt(&key_here, &lifted, &aad_here)
                    .expect("the lifted blob is intact under its own database"),
                PLAINTEXT.as_bytes()
            );
            // THE AAD ALONE, with the key held at the one the blob was written
            // under.
            assert_aead_refusal(
                &encryption::decrypt(&key_here, &lifted, &aad_there)
                    .expect_err("the AAD must refuse a ciphertext from another database"),
                "the AAD in isolation",
            );
            // THE KEY ALONE, with the AAD held at the one the blob was written
            // under.
            assert_aead_refusal(
                &encryption::decrypt(&key_there, &lifted, &aad_here)
                    .expect_err("the key must refuse a ciphertext from another database"),
                "the key in isolation",
            );

            drop(here);
            drop(there);
        })
        .await;
    drain().await;
}

// ---------------------------------------------------------------------------
// The minted login is dropped on every path
// ---------------------------------------------------------------------------

/// **The property.** A body that panics still drops the login [`Cluster::build`]
/// minted. The body reaches the cluster's own database first, so the panic is
/// on a cluster that really ran, then fails on purpose; [`Cluster::run`] catches
/// the panic, drops the login, and resumes. Reading the role catalog afterwards
/// proves nothing of this case's login remains on the shared bare server.
#[compio::test]
async fn a_panicking_encryption_cluster_arm_still_drops_its_minted_login() {
    let postgres = Postgres::start();
    let cluster = Cluster::build(postgres.url()).await;
    let login = cluster.login.clone();
    let caught = AssertUnwindSafe(cluster.run(async |cluster| {
        let writer = cluster
            .open(Cluster::binding(
                APP_A,
                &cluster.shared,
                &cluster.a_on_shared,
            ))
            .await;
        write_row(&writer).await;
        assert_eq!(
            read_column(&writer)
                .await
                .expect("the panic arm reads its row before it panics"),
            PLAINTEXT
        );
        panic!("this arm panics after the cluster ran, to prove the runner drops its login");
    }))
    .catch_unwind()
    .await;
    assert!(
        caught.is_err(),
        "the runner must propagate the body's panic"
    );

    let admin = connect(&postgres.url()).await;
    let remaining: i64 = admin
        .query_one(
            "SELECT count(*) FROM pg_roles WHERE rolname = $1",
            &[&login],
        )
        .await
        .expect("read the role catalog")
        .get(0);
    drop(admin);
    assert_eq!(
        remaining, 0,
        "a panicking arm must not leave its minted login on the shared bare server"
    );
    drain().await;
}
