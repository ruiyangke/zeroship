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
        .query(
            &format!(
                "SELECT b.database_id {} AND b.database_id = $2 LIMIT 1",
                zeroship_core::live_binding::LIVE_BINDINGS_FROM_WHERE
            ),
            &[&app, &database.as_str().to_owned()],
        )
        .await?;
    if rows.is_empty() {
        return Err("app holds no live binding to the database the subscribe request named".into());
    }
    Ok(zeroship_core::database_derivation::schema_name(&database))
}

pub(crate) fn slot_name(app: &str) -> Result<String, Error> {
    Ok(zeroship_core::replication_names::relay_slot_name(app)?)
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
    let slot = match slot_name(&key.app) {
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
    if result.is_err() {
        tracing::warn!(app_id = %key.app, database_id = %key.database, "CDC capture stopped; subscribers must reconnect and resnapshot");
    }
    // Only our prefix and this app's exact name are ever deleted. An active
    // slot is never terminated; a failed cleanup is retried at relay startup.
    if pool.query("SELECT pg_drop_replication_slot(slot_name) FROM pg_replication_slots WHERE slot_name = $1 AND NOT active AND database = current_database()", &[&slot]).await.is_err() {
        tracing::warn!(app_id = %key.app, "CDC slot cleanup failed");
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
    use std::time::Duration;

    async fn event(lease: &crate::hub::Lease) -> Event {
        compio::time::timeout(Duration::from_secs(10), lease.events.recv_async())
            .await
            .expect("CDC delivery timed out")
            .expect("CDC source stopped")
    }

    /// One database per app, derived the way the reconciler derives it.
    ///
    /// Keyed on the app id so the fixture and the Control rows it declares
    /// below cannot drift: both call this.
    fn test_database(app: &str) -> zeroship_core::DatabaseId {
        use std::collections::HashMap;
        use std::sync::{Mutex, OnceLock};
        static DATABASES: OnceLock<Mutex<HashMap<String, zeroship_core::DatabaseId>>> =
            OnceLock::new();
        DATABASES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("database map")
            .entry(app.to_owned())
            .or_insert_with(zeroship_core::DatabaseId::mint)
            .clone()
    }

    fn test_schema(app: &str) -> String {
        zeroship_core::database_derivation::schema_name(&test_database(app))
    }

    /// Declare the Control rows the relay reads to resolve a subscriber's
    /// schema. Without them capture refuses, which is the production behaviour
    /// for an app whose binding is not live.
    async fn declare_binding(pool: &Pool, app: &str) {
        let database = test_database(app);
        declare_binding_to(pool, app, &database, "active").await;
    }

    /// One `(app, database)` edge in Control's rows, at the given binding
    /// status. The database itself is always `active`, so a non-`active`
    /// status here varies exactly one conjunct of the liveness predicate.
    async fn declare_binding_to(
        pool: &Pool,
        app: &str,
        database: &zeroship_core::DatabaseId,
        status: &str,
    ) {
        let database = database.as_str().to_owned();
        pool.batch_execute(
            "CREATE SCHEMA IF NOT EXISTS zeroship;
             CREATE TABLE IF NOT EXISTS zeroship.databases (
               id text PRIMARY KEY, status text NOT NULL
             );
             CREATE TABLE IF NOT EXISTS zeroship.database_bindings (
               id text PRIMARY KEY, app_id text NOT NULL, database_id text NOT NULL,
               status text NOT NULL, generation bigint NOT NULL DEFAULT 1,
               observed_generation bigint NOT NULL DEFAULT 1
             );",
        )
        .await
        .expect("declare the control stand-ins");
        pool.execute(
            "INSERT INTO zeroship.databases (id, status) VALUES ($1, 'active') \
             ON CONFLICT (id) DO NOTHING",
            &[&database],
        )
        .await
        .expect("declare the database");
        pool.execute(
            "INSERT INTO zeroship.database_bindings (id, app_id, database_id, status) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (id) DO NOTHING",
            &[
                &zeroship_core::BindingId::mint().as_str().to_owned(),
                &app.to_owned(),
                &database,
                &status.to_owned(),
            ],
        )
        .await
        .expect("declare the binding");
    }

    /// **An app that holds several live bindings resolves the named one.**
    ///
    /// The subscribe request carries the database, so holding more than one
    /// live binding is an ordinary state here rather than an ambiguity: the
    /// request selects, and nothing has to pick. A resolver that answered from
    /// the app alone could only refuse this row set or guess at it, and the
    /// guess is the silent half.
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
        let pool = Pool::connect(&postgres.url(), 2)
            .await
            .expect("required PostgreSQL");
        let app = zeroship_core::typed_id::generate(zeroship_core::typed_id::APP_PREFIX);
        let neighbour = zeroship_core::typed_id::generate(zeroship_core::typed_id::APP_PREFIX);

        let mine = zeroship_core::DatabaseId::mint();
        let theirs = zeroship_core::DatabaseId::mint();
        let unbound = zeroship_core::DatabaseId::mint();
        let not_yet = zeroship_core::DatabaseId::mint();
        declare_binding_to(&pool, &app, &mine, "active").await;
        declare_binding_to(&pool, &app, &theirs, "active").await;
        declare_binding_to(&pool, &app, &not_yet, "pending").await;
        // `unbound` is a real, active database - the neighbour's - so the
        // refusal below is about THIS app's binding topology and not about a
        // row that is simply absent.
        declare_binding_to(&pool, &neighbour, &unbound, "active").await;

        // PRECONDITION: the app really holds TWO live bindings, compared to
        // each other. Two separately-minted ids that happened to be equal
        // would make the whole arm vacuous.
        assert_ne!(mine, theirs, "the app's two databases must be different");
        let live: i64 = pool
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

        let my_schema = bound_database_schema(&pool, &app, mine.as_str())
            .await
            .expect("a named live binding resolves rather than being refused");
        let their_schema = bound_database_schema(&pool, &app, theirs.as_str())
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

        assert!(
            bound_database_schema(&pool, &app, unbound.as_str())
                .await
                .is_err(),
            "a database this app holds no binding to must be refused"
        );
        assert!(
            bound_database_schema(&pool, &app, not_yet.as_str())
                .await
                .is_err(),
            "a binding that is not yet live must be refused"
        );

        pool.close().await;
    }

    #[compio::test]
    async fn committed_changes_fan_out_without_values_and_rollback_stays_silent() {
        let postgres = crate::postgres_fixture::Postgres::start();
        let url = postgres.url();
        let pool = Pool::connect(&url, 4).await.expect("required PostgreSQL");
        let app = zeroship_core::typed_id::generate(zeroship_core::typed_id::APP_PREFIX);
        let sibling = zeroship_core::typed_id::generate(zeroship_core::typed_id::APP_PREFIX);
        let publication = zeroship_core::replication_names::DATASTORE_PUBLICATION;
        // Two DATABASES, one subscriber. The sibling stands in for a co-tenant
        // whose tables are in the same publication, which is the shape the
        // relay-owned per-datastore publication has: membership is not a fence,
        // and the namespace comparison is.
        let schema = test_schema(&app);
        let sibling_schema = test_schema(&sibling);
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
        declare_binding(&pool, &app).await;
        let hub = Rc::new(Hub::default());
        let stream = StreamKey::new(&app, test_database(&app).as_str());
        let (first, start) = hub.subscribe(&stream, 1, 4, 32).unwrap();
        let (second, duplicate) = hub.subscribe(&stream, 1, 4, 32).unwrap();
        assert!(duplicate.is_none());
        let task = compio::runtime::spawn(run(
            hub.clone(),
            stream.clone(),
            start.unwrap(),
            pool.clone(),
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
                &[&slot_name(&app).unwrap()]
            )
            .await
            .unwrap()
            .is_empty());
        pool.batch_execute(&format!(
            "DROP PUBLICATION \"{publication}\"; DROP SCHEMA \"{schema}\" CASCADE; DROP SCHEMA \"{sibling_schema}\" CASCADE"
        ))
        .await
        .unwrap();
        pool.close().await;
    }
}
