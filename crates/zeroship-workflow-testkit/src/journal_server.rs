//! The workflow journal's database roles and the server a case opens.
//!
//! The journal lives in `PostgreSQL` owned by the migration role and read
//! through the service login, exactly as the platform migration leaves it.
//! This module owns the roles, the grants and the server; the DDL and the
//! `OrmStore` that reads it belong to the workflow crate, which passes its
//! schema name and install SQL in. Nothing here names a workflow type.

#![expect(
    clippy::future_not_send,
    reason = "fixture connections stay on their compio runtime"
)]

use std::path::{Path, PathBuf};

use compio_postgres::NoTls;
use zeroship_testkit::postgres::server::Postgres;

/// The role that owns the journal's schema and tables, as the platform
/// migrations create it.
pub const JOURNAL_OWNER: &str = "zeroship_workflow_migrator";

/// The login the journal is read and written through: the workflow service's,
/// which the platform migration grants the journal to.
pub const JOURNAL_LOGIN: &str = "zeroship_workflow";

/// The file the journal binding keeps the journal in, inside `directory`.
///
/// On `SQLite` the binding's schema is the `ATTACH` alias, and the backend
/// keeps an attached alias in `zs-<alias>.sqlite` beside the session file.
pub fn journal_file(directory: &Path, schema: &str) -> PathBuf {
    directory.join(format!("zs-{schema}.sqlite"))
}

/// A bare server with the journal installed and granted to the service login.
pub struct JournalServer {
    /// The server's URL as its superuser.
    pub admin_url: String,
    /// The server's URL as the journal login, whose password is its own name.
    pub journal_url: String,
    /// The case's bare server. Declared after the URLs so a case's connections
    /// close before the server is removed.
    _database: Postgres,
}
impl JournalServer {
    /// Start the server and install the journal, as the platform migration
    /// would: `sql` is the workflow crate's journal DDL for `schema`.
    pub async fn start(schema: &str, sql: &str) -> Self {
        let database = Postgres::start();
        // A role's password is its name, as the platform migrations set it, and
        // the URL carries it because the server authenticates the published
        // port, not the container's loopback.
        let admin_url = database.url();
        let admin = connect(&admin_url).await;
        ensure_roles(&admin).await;
        install_journal(&admin, schema, sql).await;
        grant_journal(&admin, schema).await;
        let journal_url = admin_url.replacen(
            "postgres:fixture@",
            &format!("{JOURNAL_LOGIN}:{JOURNAL_LOGIN}@"),
            1,
        );
        Self {
            admin_url,
            journal_url,
            _database: database,
        }
    }

    /// The Docker id of the shared server this case's database lives on.
    #[must_use]
    pub fn container_id(&self) -> &str {
        self._database.container_id()
    }
}

/// Create the journal's owner, login and the platform roles it must stay closed
/// to, leaving a role that already exists.
///
/// The bare server outlives one case and every case of a run shares it, so the
/// roles are cluster-global and created once. The DO block catches the duplicate
/// two cases racing to create the same role raise, rather than serializing them.
pub async fn ensure_roles(admin: &compio_postgres::Client) {
    let mut roles = vec![
        (JOURNAL_OWNER, "NOLOGIN".to_owned()),
        (
            JOURNAL_LOGIN,
            format!("LOGIN PASSWORD '{JOURNAL_LOGIN}'"),
        ),
    ];
    for role in [
        "zeroship_worker",
        "zeroship_gateway",
        "zeroship_app",
        "zeroship_control",
    ] {
        roles.push((role, format!("LOGIN PASSWORD '{role}'")));
    }
    let guarded = roles
        .iter()
        .map(|(name, attributes)| {
            format!(
                "IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{name}') THEN \
                 BEGIN CREATE ROLE {name} {attributes}; \
                 EXCEPTION WHEN unique_violation OR duplicate_object THEN NULL; END; \
                 END IF;"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    admin
        .batch_execute(&format!("DO $$\nBEGIN\n{guarded}\nEND $$;"))
        .await
        .unwrap();
}

/// Install the journal into `admin`'s database as the platform migration does:
/// `schema` and every table in it owned by [`JOURNAL_OWNER`].
pub async fn install_journal(admin: &compio_postgres::Client, schema: &str, sql: &str) {
    admin
        .batch_execute(&format!(
            "CREATE SCHEMA {schema} AUTHORIZATION {JOURNAL_OWNER}; \
             SET ROLE {JOURNAL_OWNER};"
        ))
        .await
        .unwrap();
    admin.batch_execute(sql).await.unwrap();
    admin.batch_execute("RESET ROLE;").await.unwrap();
}

/// Grant `admin`'s journal to [`JOURNAL_LOGIN`] as the platform migrations do:
/// usage on the schema and DML on its tables, and no DDL.
pub async fn grant_journal(admin: &compio_postgres::Client, schema: &str) {
    admin
        .batch_execute(&format!(
            "GRANT USAGE ON SCHEMA {schema} TO {JOURNAL_LOGIN}; \
             GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA {schema} \
             TO {JOURNAL_LOGIN};"
        ))
        .await
        .unwrap();
}

pub async fn connect(url: &str) -> compio_postgres::Client {
    let (client, connection) = compio_postgres::connect(url, NoTls).await.unwrap();
    compio::runtime::spawn(async move {
        connection.run().await.unwrap();
    })
    .detach();
    client
}

impl std::fmt::Debug for JournalServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("JournalServer").finish_non_exhaustive()
    }
}
