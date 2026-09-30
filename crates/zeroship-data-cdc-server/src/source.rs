//! PostgreSQL capture belongs exclusively to the relay process.

use crate::hub::{Hub, Start, StreamKey};
use crate::transaction::{Action, TransactionBuffer};
use compio_postgres::replication::{self, pgoutput, ReplicationMessage, StartReplicationOptions};
use compio_postgres::{MakeRustlsConnect, Pool};
use futures::FutureExt;
use std::collections::HashMap;
use std::rc::Rc;
use zeroship_data_cdc_wire::{Event, Operation};

type Error = Box<dyn std::error::Error>;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub max_bytes: usize,
    pub max_changes: usize,
    pub max_relations: usize,
}

pub(crate) use zeroship_core::replication_names::SLOT_PREFIX;

/// The physical schema of the database this subscriber NAMED, once the app is
/// confirmed to hold a live binding to it.
///
/// **Read from Control's rows, never composed here.** The database id is a
/// control-plane fact and `db_<dbs>` is derived from it, so a relay that
/// composed a schema from the request alone would name something no reconciler
/// created - and would stream a schema the app may hold no binding to. This is
/// the same read Control's binding endpoint performs, taken against the pool
/// this process already reads `zeroship.worker_instances` from when it verifies
/// a worker.
///
/// **The app is the authorization subject and the database is the target.** The
/// request's database narrows the row set; the binding predicate decides
/// whether the pair is admissible at all. An app bound to two databases is
/// ordinary here and picks neither by accident: a subscriber that named neither
/// would not have parsed as a request.
///
/// The predicate is [`zeroship_core::live_binding::LIVE_BINDINGS_FROM_WHERE`],
/// the one Control serves bindings from and the one the migration service
/// admits an apply against.
async fn bound_database_schema(pool: &Pool, app: &str, database: &str) -> Result<String, Error> {
    let database = zeroship_core::DatabaseId::parse(database)?;
    let rows = pool
        .query(&binding_lookup(), &[&app, &database.as_str().to_owned()])
        .await?;
    if rows.is_empty() {
        return Err(NoLiveBinding.into());
    }
    Ok(zeroship_core::database_derivation::schema_name(&database))
}

/// The statement [`bound_database_schema`] runs: `$1` the app, `$2` the
/// database.
fn binding_lookup() -> String {
    format!(
        "SELECT b.database_id {} AND b.database_id = $2 LIMIT 1",
        zeroship_core::live_binding::LIVE_BINDINGS_FROM_WHERE
    )
}

/// Run the binding lookup for a pair no row can match.
///
/// `PostgreSQL` checks a statement's privileges when it starts executing,
/// whatever rows it would return, so this answers exactly one question: may
/// this login read every column the lookup names. The relay asks it at boot,
/// because a login that cannot would otherwise refuse every subscriber while
/// the process reports itself listening.
pub(crate) async fn probe_binding_lookup(pool: &Pool) -> Result<(), Error> {
    pool.query(&binding_lookup(), &[&"", &""])
        .await
        .map_err(|error| {
            format!(
                "the live-binding lookup is refused: {}",
                crate::cause_chain(&error)
            )
        })?;
    Ok(())
}

/// The relay's own refusal: the app holds no live binding to the database the
/// subscriber named. Distinct from every error the lookup can raise, so a
/// refused pair is never confused with a statement the server refused.
#[derive(Debug)]
pub(crate) struct NoLiveBinding;

impl std::fmt::Display for NoLiveBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("app holds no live binding to the database the subscribe request named")
    }
}

impl std::error::Error for NoLiveBinding {}

/// The slot THIS capture owns.
///
/// It takes the whole [`StreamKey`] because a capture is per (app, database):
/// naming the slot from the app alone gave one app's two databases one name,
/// and a logical slot admits exactly one consumer, so the second capture's
/// `pg_create_logical_replication_slot` came back `42710` and its subscribers
/// disconnected. Threading the key rather than a field is what keeps the slot
/// keyed on the same pair the capture is.
pub(crate) fn slot_name(key: &StreamKey) -> Result<String, Error> {
    Ok(zeroship_core::replication_names::relay_slot_name(
        &key.app,
        &key.database,
    )?)
}

/// The caller holds the relay's database advisory lock for this task's life.
/// Every exit drops the replication socket before attempting slot cleanup.
pub(crate) async fn run(
    hub: Rc<Hub>,
    key: StreamKey,
    start: Start,
    pool: Pool,
    url: String,
    limits: Limits,
) {
    let slot = match slot_name(&key) {
        Ok(slot) => slot,
        Err(_) => {
            hub.end(&key, start.generation);
            return;
        }
    };
    let result = {
        let capture = capture(&hub, &key, start.generation, &pool, &url, &slot, limits).fuse();
        let stop = start.shutdown.recv_async().fuse();
        futures::pin_mut!(capture, stop);
        futures::select! { result = capture => result, _ = stop => Ok(()) }
    };
    if let Err(error) = &result {
        tracing::warn!(
            app_id = %key.app,
            database_id = %key.database,
            error = %crate::cause_chain(&**error),
            "CDC capture stopped; subscribers must reconnect and resnapshot"
        );
    }
    // Only this capture's exact name is ever deleted, and it carries the
    // database as well as the app, so a sibling capture of the same app is out
    // of reach here. An active slot is never terminated; a failed cleanup is
    // retried at relay startup, where the prefix scan reclaims every inactive
    // relay slot whatever pair composed it.
    if pool.query("SELECT pg_drop_replication_slot(slot_name) FROM pg_replication_slots WHERE slot_name = $1 AND NOT active AND database = current_database()", &[&slot]).await.is_err() {
        tracing::warn!(app_id = %key.app, database_id = %key.database, "CDC slot cleanup failed");
    }
    hub.end(&key, start.generation);
}

async fn capture(
    hub: &Hub,
    key: &StreamKey,
    generation: u64,
    pool: &Pool,
    url: &str,
    slot: &str,
    limits: Limits,
) -> Result<(), Error> {
    let Limits {
        max_bytes,
        max_changes,
        max_relations,
    } = limits;
    // THE DATASTORE'S ONE PUBLICATION. It is created and kept in membership by
    // the migration service, one entry set per database, so a relay that found
    // it absent is looking at a cluster no apply has ever reached.
    let publication = zeroship_core::replication_names::DATASTORE_PUBLICATION;
    if pool
        .query(
            "SELECT 1 FROM pg_publication WHERE pubname = $1",
            &[&publication],
        )
        .await?
        .is_empty()
    {
        return Err("the datastore publication is absent".into());
    }
    // THE TENANT BOUNDARY IN THIS STREAM. The publication is relay-owned and
    // spans every database on the datastore, so its membership is NOT a filter:
    // reading namespaces out of it would admit every co-tenant's relations to
    // this subscriber. What separates them is this comparison, against the
    // schema of the database this subscriber NAMED and holds a live binding to.
    let schema = bound_database_schema(pool, &key.app, &key.database).await?;
    // A source restart begins a new snapshot contract. Discard any inactive
    // previous slot rather than claiming that an in-memory queue is durable.
    pool.query("SELECT pg_drop_replication_slot(slot_name) FROM pg_replication_slots WHERE slot_name = $1 AND NOT active AND database = current_database()", &[&slot]).await?;
    let rows = pool.query("SELECT lsn::text FROM pg_create_logical_replication_slot($1, 'pgoutput', false, false)", &[&slot]).await?;
    let lsn: String = rows
        .first()
        .ok_or("slot creation returned no position")?
        .try_get(0)?;
    let config: compio_postgres::Config = url.parse()?;
    let tls = MakeRustlsConnect::from_config(&config)?;
    let connection = replication::connect_replication(tls, &config).await?;
    let mut stream = connection
        .start_logical_replication(StartReplicationOptions {
            slot_name: slot,
            start_lsn: &lsn,
            publication_names: &[publication],
            ..Default::default()
        })
        .await?;
    let mut transactions = TransactionBuffer::new(max_bytes, max_changes);
    let mut relations: HashMap<u32, String> = HashMap::new();
    hub.ready(key, generation);
    while let Some(message) = stream.next().await? {
        match message {
            ReplicationMessage::XLogData { body, .. } => {
                use pgoutput::PgOutputMessage as M;
                let message = match pgoutput::decode(&body)? {
                    M::Relation {
                        xid: None,
                        rel_id,
                        namespace,
                        name,
                        ..
                    } => {
                        if namespace == schema {
                            if !relations.contains_key(&rel_id) && relations.len() >= max_relations
                            {
                                return Err("relation cache capacity exhausted".into());
                            }
                            relations.insert(rel_id, name);
                        }
                        continue;
                    }
                    message @ (M::Insert {
                        xid: None, rel_id, ..
                    }
                    | M::Update {
                        xid: None, rel_id, ..
                    }
                    | M::Delete {
                        xid: None, rel_id, ..
                    }) => {
                        if !relations.contains_key(&rel_id) {
                            continue;
                        }
                        message
                    }
                    M::Truncate {
                        xid: None,
                        options,
                        mut relation_ids,
                    } => {
                        relation_ids.retain(|rel_id| relations.contains_key(rel_id));
                        if relation_ids.is_empty() {
                            continue;
                        }
                        M::Truncate {
                            xid: None,
                            options,
                            relation_ids,
                        }
                    }
                    message => message,
                };
                match transactions
                    .push(message)
                    .map_err(|_| "invalid transaction sequence")?
                {
                    Action::Pending => {}
                    Action::Commit(batch) => {
                        if batch.needs_resync {
                            hub.publish(key, generation, Event::Resync);
                        } else {
                            for message in batch.changes {
                                let (rel_id, operation) = match message {
                                    M::Insert { rel_id, .. } => (rel_id, Operation::Insert),
                                    M::Update { rel_id, .. } => (rel_id, Operation::Update),
                                    M::Delete { rel_id, .. } => (rel_id, Operation::Delete),
                                    M::Truncate { .. } => {
                                        hub.publish(key, generation, Event::Resync);
                                        continue;
                                    }
                                    _ => return Err("unexpected buffered message".into()),
                                };
                                match relations.get(&rel_id) {
                                    Some(collection) => hub.publish(
                                        key,
                                        generation,
                                        Event::Change {
                                            collection: collection.clone(),
                                            operation,
                                        },
                                    ),
                                    None => return Err("relation metadata missing".into()),
                                }
                            }
                        }
                        // wal_end in XLogData/Keepalive is the server's WAL tip,
                        // not a delivered position. Only COMMIT advances ACKs.
                        stream.advance_lsn(batch.end_lsn);
                    }
                }
            }
            ReplicationMessage::PrimaryKeepalive {
                reply_requested, ..
            } => {
                if reply_requested {
                    stream.send_standby_status_update(false).await?;
                }
            }
        }
    }
    Err("replication stream ended".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform_fixture::{declare_binding, Platform};
    use std::time::Duration;

    async fn event(lease: &crate::hub::Lease) -> Event {
        compio::time::timeout(Duration::from_secs(10), lease.events.recv_async())
            .await
            .expect("CDC delivery timed out")
            .expect("CDC source stopped")
    }

    /// The relay's pool, on the login a deployment gives it.
    async fn relay_pool(platform: &Platform, size: usize) -> Pool {
        Pool::connect(&platform.relay_url(), size)
            .await
            .expect("the relay's login connects")
    }

    /// The superuser pool a test declares rows, schemas and writes through.
    async fn admin_pool(platform: &Platform, size: usize) -> Pool {
        Pool::connect(&platform.admin_url(), size)
            .await
            .expect("required PostgreSQL")
    }

    /// The one value a catalog question answers, asked as `pool`'s login.
    async fn answer<T: for<'a> compio_postgres::types::FromSql<'a>>(
        pool: &Pool,
        sql: &str,
        params: &[&(dyn compio_postgres::types::ToSql + Sync)],
    ) -> T {
        pool.query(sql, params)
            .await
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
            .first()
            .unwrap_or_else(|| panic!("{sql} answered no row"))
            .try_get(0)
            .expect("the answer decodes")
    }

    /// Every column the relay's two lookups read, per `zeroship` table: the
    /// worker registry read in [`crate::auth::public_key`] and the binding read
    /// in [`bound_database_schema`]. The platform migrations grant the relay's
    /// login exactly these; the census and the per-column arm below prove each
    /// direction.
    const READS: &[(&str, &[&str])] = &[
        ("worker_instances", &["id", "status", "public_key"]),
        (
            "database_bindings",
            &[
                "app_id",
                "database_id",
                "status",
                "generation",
                "observed_generation",
            ],
        ),
        ("databases", &["id", "status"]),
    ];

    /// Run, as `relay`, the lookup that reads `zeroship.<table>`, over rows under
    /// which it answers: `app` holds a live binding to `database`.
    async fn lookup(
        relay: &Pool,
        table: &str,
        app: &str,
        database: &zeroship_core::DatabaseId,
    ) -> Result<(), Error> {
        match table {
            "worker_instances" => {
                crate::auth::public_key(relay, &zeroship_core::typed_id::generate("wkr"))
                    .await
                    .map(drop)
            }
            "database_bindings" | "databases" => {
                bound_database_schema(relay, app, database.as_str())
                    .await
                    .map(drop)
            }
            other => panic!("no relay lookup reads zeroship.{other}"),
        }
    }

    /// The relay's own refusal, [`NoLiveBinding`], and nothing else: not a
    /// statement the server refused, and not a request that failed to parse.
    #[track_caller]
    fn refused(result: Result<String, Error>, why: &str) {
        let error = result.expect_err(why);
        assert!(
            error.downcast_ref::<NoLiveBinding>().is_some(),
            "{why}: expected the relay's refusal, got: {}",
            crate::cause_chain(&*error)
        );
    }

    /// A statement the server refused for want of a privilege: `42501`.
    #[track_caller]
    fn denied<T: std::fmt::Debug>(result: Result<T, Error>, why: &str) {
        let error = result.expect_err(why);
        assert_eq!(
            error
                .downcast_ref::<compio_postgres::Error>()
                .and_then(compio_postgres::Error::code),
            Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
            "{why}: expected 42501, got: {}",
            crate::cause_chain(&*error)
        );
    }

    /// One WARN event this crate emitted, with its fields rendered.
    #[derive(Debug)]
    struct Warning {
        message: String,
        fields: std::collections::HashMap<String, String>,
    }

    struct Warnings(std::sync::Arc<std::sync::Mutex<Vec<Warning>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Warnings {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            #[derive(Default)]
            struct Fields(std::collections::HashMap<String, String>);
            impl tracing::field::Visit for Fields {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.insert(field.name().to_owned(), format!("{value:?}"));
                }
            }
            let metadata = event.metadata();
            if *metadata.level() != tracing::Level::WARN
                || !metadata.target().starts_with("zeroship_data_cdc_server")
            {
                return;
            }
            let mut fields = Fields::default();
            event.record(&mut fields);
            let message = fields.0.remove("message").unwrap_or_default();
            self.0.lock().expect("the warning buffer").push(Warning {
                message,
                fields: fields.0,
            });
        }
    }

    /// The warnings this crate logs while `future` runs on this thread.
    async fn warnings_during<F: std::future::Future>(future: F) -> (F::Output, Vec<Warning>) {
        use tracing_subscriber::layer::SubscriberExt;
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(Warnings(captured.clone())),
        );
        let output = future.await;
        drop(guard);
        let warnings = std::mem::take(&mut *captured.lock().expect("the warning buffer"));
        (output, warnings)
    }

    /// How many slots exist under `slot`.
    async fn slots(admin: &Pool, slot: &str) -> i64 {
        answer(
            admin,
            "SELECT count(*) FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot],
        )
        .await
    }

    /// **An app that holds several live bindings resolves the named one.**
    ///
    /// The subscribe request carries the database, so holding more than one
    /// live binding is an ordinary state here rather than an ambiguity: the
    /// request selects, and nothing has to pick. A resolver that answered from
    /// the app alone could only refuse this row set or guess at it, and the
    /// guess is the silent half.
    ///
    /// The lookup runs as `zeroship_cdc` over the schema the platform corpus
    /// builds, so what it may read is what `db/migrations-ts` grants that login
    /// and nothing this test added.
    ///
    /// Three controls, each differing from the admitted case in one variable:
    /// the app's OTHER live database resolves its own distinct schema (so the
    /// request's database really selects); a database that exists and is
    /// `active` but which this app holds no binding row for is refused; and a
    /// database this app IS bound to whose binding is `pending` is refused,
    /// which is the liveness conjunct rather than the ownership one.
    #[compio::test]
    async fn a_named_database_resolves_among_an_app_s_several_live_bindings() {
        let postgres = crate::postgres_fixture::Postgres::start();
        let platform = Platform::apply(postgres.url());
        let admin = admin_pool(&platform, 2).await;
        let relay = relay_pool(&platform, 2).await;
        let app = zeroship_core::typed_id::generate(zeroship_core::typed_id::APP_PREFIX);
        let neighbour = zeroship_core::typed_id::generate(zeroship_core::typed_id::APP_PREFIX);

        let mine = zeroship_core::DatabaseId::mint();
        let theirs = zeroship_core::DatabaseId::mint();
        let unbound = zeroship_core::DatabaseId::mint();
        let not_yet = zeroship_core::DatabaseId::mint();
        declare_binding(&admin, &app, &mine, "active").await;
        declare_binding(&admin, &app, &theirs, "active").await;
        declare_binding(&admin, &app, &not_yet, "pending").await;
        // `unbound` is a real, active database - the neighbour's - so the
        // refusal below is about THIS app's binding topology and not about a
        // row that is simply absent.
        declare_binding(&admin, &neighbour, &unbound, "active").await;

        // PRECONDITION: the app really holds TWO live bindings, compared to
        // each other. Two separately-minted ids that happened to be equal
        // would make the whole arm vacuous.
        assert_ne!(mine, theirs, "the app's two databases must be different");
        let live: i64 = admin
            .query(
                &format!(
                    "SELECT count(*) {}",
                    zeroship_core::live_binding::LIVE_BINDINGS_FROM_WHERE
                ),
                &[&app],
            )
            .await
            .expect("count the app's live bindings")[0]
            .try_get(0)
            .expect("the count decodes");
        assert_eq!(
            live, 2,
            "the arm is about an app that holds MORE THAN ONE live binding"
        );

        let my_schema = bound_database_schema(&relay, &app, mine.as_str())
            .await
            .expect("a named live binding resolves rather than being refused");
        let their_schema = bound_database_schema(&relay, &app, theirs.as_str())
            .await
            .expect("the app's other live binding resolves too");
        assert_eq!(
            my_schema,
            zeroship_core::database_derivation::schema_name(&mine)
        );
        assert_ne!(
            my_schema, their_schema,
            "the request's database selects which schema is streamed"
        );

        refused(
            bound_database_schema(&relay, &app, unbound.as_str()).await,
            "a database this app holds no binding to must be refused",
        );
        refused(
            bound_database_schema(&relay, &app, not_yet.as_str()).await,
            "a binding that is not yet live must be refused",
        );

        relay.close().await;
        admin.close().await;
    }

    /// **The relay's login reads what its lookups name, and nothing else.**
    ///
    /// The relay reads two things from Control's rows: an enrolled worker's
    /// identity ([`crate::auth::public_key`]) and a subscriber's live binding
    /// ([`bound_database_schema`]). The positive arms are those lookups, run as
    /// `zeroship_cdc`. The controls are read from the catalog rather than
    /// listed, so a table, column, sequence or membership added later is
    /// covered without being named here:
    ///
    /// - on each table a lookup reads, the readable columns are the lookup's
    ///   columns exactly;
    /// - across every relation outside the system catalogs and extensions,
    ///   those tables are the only ones readable, and no relation carries any
    ///   write, table-wide or on a column;
    /// - no sequence grants the login anything;
    /// - the login belongs to no role. `zeroship_cdc` is `NOINHERIT`, so a
    ///   privilege held by a role it belongs to is invisible to every
    ///   `has_*_privilege` answer above and still reachable with `SET ROLE`.
    ///
    /// Each census names one member it must contain, so none of them can pass
    /// over an empty catalog. A statement past the grant is also sent, so one
    /// refusal is `PostgreSQL`'s `42501` rather than a catalog function's word.
    #[compio::test]
    async fn the_relay_login_reads_what_its_lookups_name_and_nothing_else() {
        let postgres = crate::postgres_fixture::Postgres::start();
        let platform = Platform::apply(postgres.url());
        let admin = admin_pool(&platform, 2).await;
        let relay = relay_pool(&platform, 2).await;
        let login: String = answer(&relay, "SELECT current_user::text", &[]).await;
        assert_eq!(login, crate::platform_fixture::RELAY_LOGIN);

        // THE POSITIVE ARMS, as the relay runs them.
        let app = zeroship_core::typed_id::generate(zeroship_core::typed_id::APP_PREFIX);
        let database = zeroship_core::DatabaseId::mint();
        declare_binding(&admin, &app, &database, "active").await;
        for (table, _) in READS {
            lookup(&relay, table, &app, &database)
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "the relay's login runs the lookup reading {table}: {}",
                        crate::cause_chain(&*error)
                    )
                });
        }

        // THE COLUMN CENSUS, per table a lookup reads.
        for (table, granted) in READS {
            let relation = format!("zeroship.{table}");
            let columns: Vec<String> = admin
                .query(
                    "SELECT attname::text FROM pg_attribute \
                     WHERE attrelid = $1::text::regclass AND attnum > 0 AND NOT attisdropped \
                     ORDER BY attnum",
                    &[&relation],
                )
                .await
                .expect("read the table's columns")
                .iter()
                .map(|row| row.try_get(0).expect("the column name decodes"))
                .collect();
            for column in *granted {
                assert!(
                    columns.iter().any(|name| name == column),
                    "{relation}.{column} must exist for the lookup to name it"
                );
            }
            assert!(
                columns.len() > granted.len(),
                "{relation} must carry columns beyond the lookup's, or the denied half is vacuous"
            );
            for column in &columns {
                let readable: bool = answer(
                    &relay,
                    "SELECT has_column_privilege($1::text, $2::text, 'SELECT')",
                    &[&relation, column],
                )
                .await;
                assert_eq!(
                    readable,
                    granted.contains(&column.as_str()),
                    "{relation}.{column}: the relay reads the lookup's columns exactly"
                );
            }
        }

        // THE RELATION CENSUS, over every relation outside the system catalogs.
        // An extension's own relations are left out: the extension grants them
        // to PUBLIC, and no platform migration authors them.
        let census: Vec<(String, bool, Vec<String>)> = relay
            .query(
                "SELECT n.nspname || '.' || c.relname, \
                        has_any_column_privilege(c.oid, 'SELECT'), \
                        ARRAY(SELECT p FROM unnest(ARRAY['SELECT', 'INSERT', 'UPDATE', 'DELETE', \
                                                         'TRUNCATE', 'REFERENCES', 'TRIGGER']) AS p \
                               WHERE has_table_privilege(c.oid, p)) \
                        || ARRAY(SELECT 'column ' || p \
                                   FROM unnest(ARRAY['INSERT', 'UPDATE', 'REFERENCES']) AS p \
                                  WHERE has_any_column_privilege(c.oid, p)) \
                   FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') \
                    AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
                    AND left(n.nspname, 3) <> 'pg_' \
                    AND NOT EXISTS (SELECT 1 FROM pg_depend d \
                                     WHERE d.classid = 'pg_class'::regclass \
                                       AND d.objid = c.oid AND d.deptype = 'e') \
                  ORDER BY (n.nspname || '.' || c.relname) COLLATE \"C\"",
                &[],
            )
            .await
            .expect("census the platform's relations")
            .iter()
            .map(|row| {
                (
                    row.try_get(0).expect("the relation decodes"),
                    row.try_get(1).expect("the read privilege decodes"),
                    row.try_get(2).expect("the held privileges decode"),
                )
            })
            .collect();
        assert!(
            census
                .iter()
                .any(|(relation, readable, _)| relation == "zeroship.datastores" && !readable),
            "the census must reach a Control table no lookup reads, and find it unreadable: \
             {census:?}"
        );
        let readable: Vec<&str> = census
            .iter()
            .filter(|(_, readable, _)| *readable)
            .map(|(relation, _, _)| relation.as_str())
            .collect();
        let mut expected: Vec<String> = READS
            .iter()
            .map(|(table, _)| format!("zeroship.{table}"))
            .collect();
        expected.sort();
        assert_eq!(
            readable, expected,
            "the relay's login reads these tables and no other platform relation"
        );
        for (relation, _, held) in &census {
            assert!(
                held.is_empty(),
                "{relation}: the relay's login holds {held:?}; it reads columns and writes nothing"
            );
        }

        // THE SEQUENCE CENSUS.
        let sequences: Vec<(String, Vec<String>)> = relay
            .query(
                "SELECT n.nspname || '.' || c.relname, \
                        ARRAY(SELECT p FROM unnest(ARRAY['USAGE', 'SELECT', 'UPDATE']) AS p \
                               WHERE has_sequence_privilege(c.oid, p)) \
                   FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE c.relkind = 'S' \
                    AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
                    AND left(n.nspname, 3) <> 'pg_' \
                    AND NOT EXISTS (SELECT 1 FROM pg_depend d \
                                     WHERE d.classid = 'pg_class'::regclass \
                                       AND d.objid = c.oid AND d.deptype = 'e') \
                  ORDER BY (n.nspname || '.' || c.relname) COLLATE \"C\"",
                &[],
            )
            .await
            .expect("census the platform's sequences")
            .iter()
            .map(|row| {
                (
                    row.try_get(0).expect("the sequence decodes"),
                    row.try_get(1).expect("the held privileges decode"),
                )
            })
            .collect();
        assert!(
            sequences
                .iter()
                .any(|(sequence, _)| sequence == "zeroship.audit_events_id_seq"),
            "the census must reach a platform sequence: {sequences:?}"
        );
        for (sequence, held) in &sequences {
            assert!(
                held.is_empty(),
                "{sequence}: the relay's login holds {held:?} on a sequence"
            );
        }

        denied(
            relay
                .query("SELECT capability FROM zeroship.database_bindings", &[])
                .await
                .map_err(Error::from),
            "a column the lookup does not name is refused by PostgreSQL",
        );

        // THE MEMBERSHIP CENSUS, and its control: a role granted here is seen
        // by it, is invisible to `has_table_privilege` on this NOINHERIT login,
        // and is still reachable with `SET ROLE` - which is why it is censused.
        let memberships = "SELECT count(*) FROM pg_auth_members \
                           WHERE member = (SELECT oid FROM pg_roles WHERE rolname = current_user)";
        assert_eq!(
            answer::<i64>(&relay, memberships, &[]).await,
            0,
            "the relay's login belongs to no role"
        );
        let probe = zeroship_core::typed_id::generate("tst");
        admin
            .batch_execute(&format!(
                "CREATE ROLE \"{probe}\" NOLOGIN; \
                 GRANT USAGE ON SCHEMA zeroship TO \"{probe}\"; \
                 GRANT SELECT ON zeroship.datastores TO \"{probe}\"; \
                 GRANT \"{probe}\" TO \"{}\"",
                crate::platform_fixture::RELAY_LOGIN
            ))
            .await
            .expect("grant the relay's login a membership");
        assert_eq!(
            answer::<i64>(&relay, memberships, &[]).await,
            1,
            "the membership census sees a granted role"
        );
        assert!(
            !answer::<bool>(
                &relay,
                "SELECT has_table_privilege('zeroship.datastores', 'SELECT')",
                &[],
            )
            .await,
            "a NOINHERIT login's memberships are invisible to has_table_privilege"
        );
        let session = relay.acquire().await.expect("a relay session");
        session
            .batch_execute(&format!("SET ROLE \"{probe}\""))
            .await
            .expect("the relay's login may assume a role it belongs to");
        session
            .query("SELECT count(*) FROM zeroship.datastores", &[])
            .await
            .expect("an assumed role's privileges are the session's");
        session
            .batch_execute("RESET ROLE")
            .await
            .expect("return the session to the relay's login");
        drop(session);
        admin
            .batch_execute(&format!("DROP OWNED BY \"{probe}\"; DROP ROLE \"{probe}\""))
            .await
            .expect("withdraw the membership");
        assert_eq!(
            answer::<i64>(&relay, memberships, &[]).await,
            0,
            "the withdrawn membership is gone again"
        );

        relay.close().await;
        admin.close().await;
    }

    /// **Every column the relay is granted is one its lookups read.**
    ///
    /// The census proves nothing beyond [`READS`] is granted; this proves the
    /// other direction. For each column in it, the column's grant is withdrawn
    /// and the lookup that reads its table, run as `zeroship_cdc`, must be
    /// refused `42501`; then the grant is restored and the same lookup must
    /// answer. A column the platform grants and no lookup reads is an
    /// over-grant, and withdrawing it refuses nothing, so this arm names it.
    #[compio::test]
    async fn every_column_the_relay_is_granted_is_one_its_lookups_read() {
        let postgres = crate::postgres_fixture::Postgres::start();
        let platform = Platform::apply(postgres.url());
        let admin = admin_pool(&platform, 2).await;
        let relay = relay_pool(&platform, 2).await;
        let app = zeroship_core::typed_id::generate(zeroship_core::typed_id::APP_PREFIX);
        let database = zeroship_core::DatabaseId::mint();
        declare_binding(&admin, &app, &database, "active").await;

        let mut withdrawn = Vec::new();
        for (table, columns) in READS {
            let relation = format!("zeroship.{table}");
            for column in *columns {
                assert!(
                    answer::<bool>(
                        &admin,
                        "SELECT has_column_privilege($1::text, $2::text, $3::text, 'SELECT')",
                        &[&crate::platform_fixture::RELAY_LOGIN, &relation, column],
                    )
                    .await,
                    "{relation}.{column} must be granted before it can be withdrawn"
                );
                admin
                    .batch_execute(&format!(
                        "REVOKE SELECT (\"{column}\") ON {relation} FROM \"{}\"",
                        crate::platform_fixture::RELAY_LOGIN
                    ))
                    .await
                    .expect("withdraw one column");
                denied(
                    lookup(&relay, table, &app, &database).await,
                    &format!(
                        "with {relation}.{column} withdrawn, the lookup reading {table} must be \
                         refused; a grant no lookup needs is an over-grant"
                    ),
                );
                admin
                    .batch_execute(&format!(
                        "GRANT SELECT (\"{column}\") ON {relation} TO \"{}\"",
                        crate::platform_fixture::RELAY_LOGIN
                    ))
                    .await
                    .expect("restore the column");
                lookup(&relay, table, &app, &database)
                    .await
                    .unwrap_or_else(|error| {
                        panic!(
                            "with {relation}.{column} restored the lookup answers again: {}",
                            crate::cause_chain(&*error)
                        )
                    });
                withdrawn.push(format!("{relation}.{column}"));
            }
        }
        assert_eq!(
            withdrawn.len(),
            READS
                .iter()
                .map(|(_, columns)| columns.len())
                .sum::<usize>(),
            "every granted column was withdrawn once: {withdrawn:?}"
        );

        relay.close().await;
        admin.close().await;
    }

    /// **A binding lookup the server refuses stops the capture, says why, and
    /// leaves nothing behind.**
    ///
    /// This is the path production took when the relay's login had no grant on
    /// the binding rows: every subscribe started a capture whose lookup the
    /// server refused. Each outcome is one a subscriber or an operator can
    /// observe: the capture returns; the subscriber's stream ends without ever
    /// being `Ready`; no slot exists under the capture's name; and the warning
    /// carries the server's refusal rather than the bare error kind, which
    /// displays as `db error` and names nothing.
    ///
    /// The control differs in the one variable: with the grant restored, the
    /// same key captures, reaches `Ready` and holds a slot under that name, so
    /// the slot check is one that can see a slot.
    #[compio::test]
    async fn a_refused_binding_lookup_stops_capture_with_its_cause_and_leaves_no_slot() {
        use crate::platform_fixture::{grant_relay, relay_columns, revoke_relay};
        let postgres = crate::postgres_fixture::Postgres::start();
        let platform = Platform::apply(postgres.url());
        let admin = admin_pool(&platform, 4).await;
        let relay = relay_pool(&platform, 4).await;
        let url = platform.relay_url();
        let app = zeroship_core::typed_id::generate(zeroship_core::typed_id::APP_PREFIX);
        let database = zeroship_core::DatabaseId::mint();
        let schema = zeroship_core::database_derivation::schema_name(&database);
        let publication = zeroship_core::replication_names::DATASTORE_PUBLICATION;
        admin
            .batch_execute(&format!(
                "CREATE SCHEMA \"{schema}\";
                 CREATE TABLE \"{schema}\".orders (id int PRIMARY KEY);
                 CREATE PUBLICATION \"{publication}\" FOR TABLES IN SCHEMA \"{schema}\";"
            ))
            .await
            .expect("logical WAL and publication required");
        declare_binding(&admin, &app, &database, "active").await;
        let granted = relay_columns(&admin, "database_bindings").await;
        assert!(
            !granted.is_empty(),
            "the relay's login must hold a grant on the binding rows to lose one"
        );
        revoke_relay(&admin, "database_bindings").await;

        let hub = Rc::new(Hub::default());
        let key = StreamKey::new(&app, database.as_str());
        let slot = slot_name(&key).unwrap();
        let limits = Limits {
            max_bytes: 1024 * 1024,
            max_changes: 100,
            max_relations: 4,
        };
        let (lease, start) = hub.subscribe(&key, 1, 4, 32).unwrap();
        let ((), warnings) = warnings_during(run(
            hub.clone(),
            key.clone(),
            start.expect("the first subscriber starts the capture"),
            relay.clone(),
            url.clone(),
            limits,
        ))
        .await;
        let ended = compio::time::timeout(Duration::from_secs(10), lease.events.recv_async())
            .await
            .expect("the subscriber's stream must end rather than hang");
        assert!(
            ended.is_err(),
            "the subscriber must be disconnected without ever being Ready: {ended:?}"
        );
        let stopped: Vec<&Warning> = warnings
            .iter()
            .filter(|warning| warning.message.starts_with("CDC capture stopped"))
            .collect();
        assert_eq!(
            stopped.len(),
            1,
            "the stopped capture warns once: {warnings:?}"
        );
        let cause = stopped[0]
            .fields
            .get("error")
            .expect("the warning carries its cause");
        assert!(
            cause.contains("permission denied for table database_bindings"),
            "the warning must carry the server's refusal, not the bare error kind: {cause}"
        );
        assert_eq!(
            slots(&admin, &slot).await,
            0,
            "a refused capture leaves no slot"
        );
        drop(lease);

        // THE CONTROL: the same key, the grant restored.
        grant_relay(&admin, "database_bindings", &granted).await;
        let (lease, start) = hub.subscribe(&key, 1, 4, 32).unwrap();
        let task = compio::runtime::spawn(run(
            hub.clone(),
            key.clone(),
            start.expect("the ended stream starts a capture of its own"),
            relay.clone(),
            url,
            limits,
        ));
        assert_eq!(event(&lease).await, Event::Ready);
        assert_eq!(
            slots(&admin, &slot).await,
            1,
            "a capture that resolved holds its slot under that name"
        );
        drop(lease);
        compio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            slots(&admin, &slot).await,
            0,
            "the capture reclaims its slot"
        );

        relay.close().await;
        admin.close().await;
    }

    /// **One app's two databases of ONE datastore capture through two slots.**
    ///
    /// A capture runs per (app, database) and a datastore is reached through a
    /// single `PostgreSQL` database, so both of this app's captures create
    /// their slot in the same place. A slot name composed from the app alone is
    /// therefore one name asked for twice: the server answers the second
    /// `pg_create_logical_replication_slot` with `42710`,
    /// `replication slot "..." already exists`, that capture returns the error,
    /// and the hub tears its stream down under subscribers who then reconnect.
    ///
    /// The arm is behavioural on both sides. Both captures have to reach
    /// `Ready`, which is what a shared name denies, and then each database's
    /// commit has to reach its own subscriber and only its own. The two
    /// databases declare DIFFERENTLY NAMED collections so that half is
    /// race-free: a capture publishes in WAL order, so a capture that leaked
    /// its neighbour's commit delivers the wrong collection here rather than
    /// delivering nothing yet.
    ///
    /// The slot rows are read back as the mechanism check - two rows, both
    /// active, both under the prefix the relay's startup scan reclaims by.
    #[compio::test]
    async fn one_app_s_two_databases_capture_through_two_slots() {
        let postgres = crate::postgres_fixture::Postgres::start();
        let platform = Platform::apply(postgres.url());
        let pool = admin_pool(&platform, 8).await;
        let relay = relay_pool(&platform, 8).await;
        let url = platform.relay_url();
        let app = zeroship_core::typed_id::generate(zeroship_core::typed_id::APP_PREFIX);
        let mine = zeroship_core::DatabaseId::mint();
        let theirs = zeroship_core::DatabaseId::mint();
        assert_ne!(mine, theirs, "the control: two mints are two databases");
        let my_schema = zeroship_core::database_derivation::schema_name(&mine);
        let their_schema = zeroship_core::database_derivation::schema_name(&theirs);
        let publication = zeroship_core::replication_names::DATASTORE_PUBLICATION;
        pool.batch_execute(&format!(
            "CREATE SCHEMA \"{my_schema}\";
             CREATE SCHEMA \"{their_schema}\";
             CREATE TABLE \"{my_schema}\".orders (id int PRIMARY KEY);
             CREATE TABLE \"{their_schema}\".invoices (id int PRIMARY KEY);
             CREATE PUBLICATION \"{publication}\" FOR TABLES IN SCHEMA \"{my_schema}\", \"{their_schema}\";"
        ))
        .await
        .expect("logical WAL and publication required");
        declare_binding(&pool, &app, &mine, "active").await;
        declare_binding(&pool, &app, &theirs, "active").await;

        let hub = Rc::new(Hub::default());
        let my_key = StreamKey::new(&app, mine.as_str());
        let their_key = StreamKey::new(&app, theirs.as_str());
        let (first, my_start) = hub.subscribe(&my_key, 2, 4, 32).unwrap();
        let (second, their_start) = hub.subscribe(&their_key, 2, 4, 32).unwrap();
        let limits = Limits {
            max_bytes: 1024 * 1024,
            max_changes: 100,
            max_relations: 8,
        };
        let my_task = compio::runtime::spawn(run(
            hub.clone(),
            my_key.clone(),
            my_start.unwrap(),
            relay.clone(),
            url.clone(),
            limits,
        ));
        let their_task = compio::runtime::spawn(run(
            hub.clone(),
            their_key.clone(),
            their_start.expect("the app's second database starts a capture of its own"),
            relay.clone(),
            url.clone(),
            limits,
        ));
        assert_eq!(event(&first).await, Event::Ready);
        assert_eq!(
            event(&second).await,
            Event::Ready,
            "the app's second capture must get a slot of its own"
        );

        let my_slot = slot_name(&my_key).unwrap();
        let their_slot = slot_name(&their_key).unwrap();
        assert_ne!(
            my_slot, their_slot,
            "one app's two captures must not request one slot"
        );
        let rows = pool
            .query(
                "SELECT slot_name, active FROM pg_replication_slots \
                 WHERE database = current_database() AND (slot_name = $1 OR slot_name = $2) \
                 ORDER BY slot_name",
                &[&my_slot, &their_slot],
            )
            .await
            .expect("read the relay's slots back");
        assert_eq!(rows.len(), 2, "one capture per database is one slot each");
        for row in &rows {
            let name: String = row.try_get(0).expect("the slot name decodes");
            let active: bool = row.try_get(1).expect("the active flag decodes");
            assert!(active, "{name} must be held by the capture that made it");
            assert!(
                name.starts_with(SLOT_PREFIX),
                "{name} must be reclaimable by the relay's startup prefix scan"
            );
        }
        // The relay's startup scan is over the prefix and not over an app, so
        // it has to see BOTH of this app's slots and nothing else.
        let reclaimable: i64 = pool
            .query(
                "SELECT count(*) FROM pg_replication_slots WHERE database = current_database() \
                 AND left(slot_name, length($1)) = $1",
                &[&SLOT_PREFIX],
            )
            .await
            .expect("count the relay's slots")[0]
            .try_get(0)
            .expect("the count decodes");
        assert_eq!(
            reclaimable, 2,
            "the prefix scan must reach every capture's slot"
        );

        let writer = pool.acquire().await.unwrap();
        let insert = |schema: &str, table: &str, id: i32| {
            format!("INSERT INTO \"{schema}\".{table} VALUES ({id})")
        };
        writer
            .batch_execute(&insert(&my_schema, "orders", 1))
            .await
            .unwrap();
        assert_eq!(
            event(&first).await,
            Event::Change {
                collection: "orders".into(),
                operation: Operation::Insert,
            }
        );
        writer
            .batch_execute(&insert(&their_schema, "invoices", 1))
            .await
            .unwrap();
        assert_eq!(
            event(&second).await,
            Event::Change {
                collection: "invoices".into(),
                operation: Operation::Insert,
            },
            "the second database's own capture delivers its commit"
        );
        // Both captures decode the same publication, so this is where a leak
        // would show: the next event on each stream is the next thing ITS
        // capture published, and a capture that had published its neighbour's
        // commit would deliver that collection here instead.
        writer
            .batch_execute(&insert(&my_schema, "orders", 2))
            .await
            .unwrap();
        writer
            .batch_execute(&insert(&their_schema, "invoices", 2))
            .await
            .unwrap();
        assert_eq!(
            event(&first).await,
            Event::Change {
                collection: "orders".into(),
                operation: Operation::Insert,
            },
            "the other database's commit must not reach this subscriber"
        );
        assert_eq!(
            event(&second).await,
            Event::Change {
                collection: "invoices".into(),
                operation: Operation::Insert,
            },
            "the other database's commit must not reach this subscriber"
        );

        drop(writer);
        drop(first);
        drop(second);
        for task in [my_task, their_task] {
            compio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap();
        }
        let left: i64 = pool
            .query(
                "SELECT count(*) FROM pg_replication_slots WHERE database = current_database() \
                 AND left(slot_name, length($1)) = $1",
                &[&SLOT_PREFIX],
            )
            .await
            .expect("count the relay's slots")[0]
            .try_get(0)
            .expect("the count decodes");
        assert_eq!(left, 0, "each capture reclaims its own slot on exit");

        pool.batch_execute(&format!(
            "DROP PUBLICATION \"{publication}\"; \
             DROP SCHEMA \"{my_schema}\" CASCADE; DROP SCHEMA \"{their_schema}\" CASCADE"
        ))
        .await
        .unwrap();
        relay.close().await;
        pool.close().await;
    }

    /// `PostgreSQL` bounds a slot name, and bounds the characters it may carry.
    ///
    /// This is what
    /// [`zeroship_core::replication_names::POSTGRES_SLOT_NAME_MAX_BYTES`]
    /// claims, asked of a running server rather than of a document. Both
    /// failure shapes are here because they are different shapes: a name bound
    /// as the `name` argument - the relay's own path - is REFUSED with
    /// `42622`, while the same width reaching that argument through a
    /// server-side text conversion is CLIPPED with nothing said, and the
    /// database half is last, so what a clip drops is what tells one app's two
    /// captures apart. Neither is left to the server: the const assertion
    /// beside the constant settles the width before anything runs.
    ///
    /// The controls are beside each arm - a composed name that survives the
    /// same round trip whole, and one that the character rule admits - so an
    /// arm cannot pass because the server had begun refusing everything.
    #[compio::test]
    async fn postgresql_bounds_a_slot_name_and_the_characters_it_may_carry() {
        use compio_postgres::error::SqlState;
        use zeroship_core::replication_names::POSTGRES_SLOT_NAME_MAX_BYTES as MAX;
        let postgres = crate::postgres_fixture::Postgres::start();
        let pool = Pool::connect(&postgres.url(), 2)
            .await
            .expect("required PostgreSQL");
        let create = "SELECT slot_name::text FROM \
                      pg_create_logical_replication_slot($1, 'pgoutput', false, false)";

        // THE RELAY'S PATH: the name is a bound `name` parameter, and the
        // server refuses one past the bound instead of shortening it.
        let over_long = format!("{SLOT_PREFIX}{}", "a".repeat(MAX));
        assert!(over_long.len() > MAX, "the input must exceed the bound");
        let refusal = pool
            .query(create, &[&over_long])
            .await
            .expect_err("an over-long slot name must not be accepted");
        assert_eq!(
            refusal.code(),
            Some(&SqlState::NAME_TOO_LONG),
            "the bound is the server's; it answered {refusal:?}"
        );

        // THE QUIET PATH: the same text converted to `name` by the server
        // keeps the head and drops the tail, with neither error nor notice.
        let clipped: String = pool
            .query("SELECT (($1::text)::name)::text", &[&over_long])
            .await
            .expect("a text-to-name conversion does not refuse")[0]
            .try_get(0)
            .expect("the converted name decodes");
        assert_eq!(
            clipped.len(),
            MAX,
            "`{over_long}` came back as `{clipped}` with nothing said"
        );
        assert_eq!(
            clipped,
            over_long[..MAX],
            "the head survives, so the tail is what a clip costs"
        );

        // THE CHARACTER RULE, which is why the halves are hashed rather than
        // spelled: a slot name carries lower-case letters, digits and the
        // underscore and nothing else.
        let refusal = pool
            .query(create, &[&format!("{SLOT_PREFIX}Upper")])
            .await
            .expect_err("an upper-case slot name must not be accepted");
        assert_eq!(
            refusal.code(),
            Some(&SqlState::INVALID_NAME),
            "the character rule is the server's; it answered {refusal:?}"
        );

        // THE CONTROL FOR BOTH: a name this tree composes is accepted and
        // stored exactly as composed.
        let key = StreamKey::new(
            &zeroship_core::typed_id::generate(zeroship_core::typed_id::APP_PREFIX),
            zeroship_core::DatabaseId::mint().as_str(),
        );
        let composed = slot_name(&key).unwrap();
        assert!(
            !composed.is_empty() && composed.len() <= MAX,
            "`{composed}` must be a name the server can hold"
        );
        let stored: String = pool
            .query(create, &[&composed])
            .await
            .expect("a composed relay slot name is accepted")[0]
            .try_get(0)
            .expect("the created name decodes");
        assert_eq!(
            stored, composed,
            "a relay slot name must reach the server whole"
        );

        pool.query("SELECT pg_drop_replication_slot($1)", &[&stored])
            .await
            .expect("drop the probe slot");
        pool.close().await;
    }

    #[compio::test]
    async fn committed_changes_fan_out_without_values_and_rollback_stays_silent() {
        let postgres = crate::postgres_fixture::Postgres::start();
        let platform = Platform::apply(postgres.url());
        let pool = admin_pool(&platform, 4).await;
        let relay = relay_pool(&platform, 4).await;
        let url = platform.relay_url();
        let app = zeroship_core::typed_id::generate(zeroship_core::typed_id::APP_PREFIX);
        let database = zeroship_core::DatabaseId::mint();
        let sibling = zeroship_core::DatabaseId::mint();
        assert_ne!(
            database, sibling,
            "the control: two mints are two databases"
        );
        let publication = zeroship_core::replication_names::DATASTORE_PUBLICATION;
        // Two DATABASES, one subscriber. The sibling stands in for a co-tenant
        // whose tables are in the same publication, which is the shape the
        // relay-owned per-datastore publication has: membership is not a fence,
        // and the namespace comparison is.
        let schema = zeroship_core::database_derivation::schema_name(&database);
        let sibling_schema = zeroship_core::database_derivation::schema_name(&sibling);
        let ddl = format!(
            "CREATE SCHEMA \"{schema}\";
             CREATE SCHEMA \"{sibling_schema}\";
             CREATE TABLE \"{schema}\".orders (id int PRIMARY KEY, secret text);
             CREATE TABLE \"{schema}\".rolled_back (id int);
             CREATE TABLE \"{schema}\".__zeroship_events (id int PRIMARY KEY);
             CREATE TABLE \"{schema}\".partitioned_events (id int, bucket int) PARTITION BY LIST (bucket);
             CREATE TABLE \"{schema}\".partitioned_events_default PARTITION OF \"{schema}\".partitioned_events DEFAULT;
             CREATE TABLE \"{sibling_schema}\".noise (id int PRIMARY KEY);
             CREATE PUBLICATION \"{publication}\" FOR TABLES IN SCHEMA \"{schema}\" WITH (publish_via_partition_root = true);
             ALTER PUBLICATION \"{publication}\" ADD TABLE \"{sibling_schema}\".noise;"
        );
        pool.batch_execute(&ddl)
            .await
            .expect("logical WAL and publication required");
        declare_binding(&pool, &app, &database, "active").await;
        let hub = Rc::new(Hub::default());
        let stream = StreamKey::new(&app, database.as_str());
        let (first, start) = hub.subscribe(&stream, 1, 4, 32).unwrap();
        let (second, duplicate) = hub.subscribe(&stream, 1, 4, 32).unwrap();
        assert!(duplicate.is_none());
        let task = compio::runtime::spawn(run(
            hub.clone(),
            stream.clone(),
            start.unwrap(),
            relay.clone(),
            url,
            Limits {
                max_bytes: 1024 * 1024,
                max_changes: 100,
                max_relations: 4,
            },
        ));
        assert_eq!(event(&first).await, Event::Ready);
        assert_eq!(event(&second).await, Event::Ready);
        let writer = pool.acquire().await.unwrap();
        writer
            .batch_execute(&format!(
                "INSERT INTO \"{sibling_schema}\".noise VALUES (1); TRUNCATE \"{sibling_schema}\".noise"
            ))
            .await
            .unwrap();
        writer.batch_execute(&format!("BEGIN; INSERT INTO \"{schema}\".rolled_back VALUES (1); ROLLBACK; BEGIN; INSERT INTO \"{schema}\".orders VALUES (1, 'must-never-reach-a-worker')")).await.unwrap();
        assert!(first.events.is_empty(), "uncommitted changes escaped");
        writer.batch_execute("COMMIT").await.unwrap();
        for lease in [&first, &second] {
            let change = event(lease).await;
            assert_eq!(
                change,
                Event::Change {
                    collection: "orders".into(),
                    operation: Operation::Insert
                }
            );
            assert!(!String::from_utf8(change.encode().unwrap())
                .unwrap()
                .contains("must-never"));
        }
        writer
            .batch_execute(&format!(
                "INSERT INTO \"{schema}\".__zeroship_events VALUES (1)"
            ))
            .await
            .unwrap();
        for lease in [&first, &second] {
            assert_eq!(
                event(lease).await,
                Event::Change {
                    collection: "__zeroship_events".into(),
                    operation: Operation::Insert,
                }
            );
        }
        writer
            .batch_execute(&format!(
                "INSERT INTO \"{schema}\".partitioned_events VALUES (1, 7)"
            ))
            .await
            .unwrap();
        for lease in [&first, &second] {
            assert_eq!(
                event(lease).await,
                Event::Change {
                    collection: "partitioned_events".into(),
                    operation: Operation::Insert,
                }
            );
        }
        writer
            .batch_execute(&format!("TRUNCATE \"{schema}\".orders"))
            .await
            .unwrap();
        assert_eq!(event(&first).await, Event::Resync);
        assert_eq!(event(&second).await, Event::Resync);
        drop(writer);
        drop(first);
        drop(second);
        compio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert!(pool
            .query(
                "SELECT 1 FROM pg_replication_slots WHERE slot_name = $1",
                &[&slot_name(&stream).unwrap()]
            )
            .await
            .unwrap()
            .is_empty());
        pool.batch_execute(&format!(
            "DROP PUBLICATION \"{publication}\"; DROP SCHEMA \"{schema}\" CASCADE; DROP SCHEMA \"{sibling_schema}\" CASCADE"
        ))
        .await
        .unwrap();
        relay.close().await;
        pool.close().await;
    }
}
