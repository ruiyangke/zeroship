//! PostgreSQL capture belongs exclusively to the relay process.

use crate::hub::{Hub, Start};
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

pub(crate) const SLOT_PREFIX: &str = "__zs_relay_";

/// The physical schema of the one database this app is bound to.
///
/// **Read from Control's rows, never composed here.** The database id is a
/// control-plane fact and `db_<dbs>` is derived from it, so a relay that
/// composed a schema from the app id would name something no reconciler
/// created. This is the same read Control's binding endpoint performs, taken
/// against the pool this process already reads `zeroship.worker_instances`
/// from when it verifies a worker.
///
/// **An app holds exactly one database today, so "the app.s binding" is
/// unambiguous.** When an app can hold several, the subscribe request has to
/// name WHICH database, and that is a wire-contract change: every producer,
/// consumer, fixture and doc in one patch. Until then this REFUSES a second
/// live binding rather than ordering and taking the first: picking silently
/// would stream one database to a subscriber expecting another, and the day
/// that becomes possible is the day nobody is looking at this function.
async fn bound_database_schema(pool: &Pool, app: &str) -> Result<String, Error> {
    let rows = pool
        .query(
            "SELECT b.database_id \
               FROM zeroship.database_bindings b \
               JOIN zeroship.databases d ON d.id = b.database_id \
              WHERE b.app_id = $1 \
                AND b.status = 'active' \
                AND b.observed_generation >= b.generation \
                AND d.status = 'active' \
              ORDER BY b.id \
              LIMIT 2",
            &[&app],
        )
        .await?;
    if rows.len() > 1 {
        return Err("app holds more than one live database binding; the subscribe request must \
                    name which database"
            .into());
    }
    let row = rows.first().ok_or("app has no live database binding")?;
    let database: String = row.try_get(0)?;
    let database = zeroship_core::DatabaseId::parse(&database)?;
    Ok(zeroship_core::database_derivation::schema_name(&database))
}

pub(crate) fn slot_name(app: &str) -> Result<String, Error> {
    let publication = zeroship_core::replication_names::publication_name(app)?;
    let token = publication
        .strip_prefix("__zs_pub_")
        .ok_or("unexpected publication name")?;
    Ok(format!("{SLOT_PREFIX}{token}"))
}

/// The caller holds the relay's database advisory lock for this task's life.
/// Every exit drops the replication socket before attempting slot cleanup.
pub(crate) async fn run(
    hub: Rc<Hub>,
    app: String,
    start: Start,
    pool: Pool,
    url: String,
    limits: Limits,
) {
    let slot = match slot_name(&app) {
        Ok(slot) => slot,
        Err(_) => {
            hub.end(&app, start.generation);
            return;
        }
    };
    let result = {
        let capture = capture(&hub, &app, start.generation, &pool, &url, &slot, limits).fuse();
        let stop = start.shutdown.recv_async().fuse();
        futures::pin_mut!(capture, stop);
        futures::select! { result = capture => result, _ = stop => Ok(()) }
    };
    if result.is_err() {
        tracing::warn!(app_id = %app, "CDC capture stopped; subscribers must reconnect and resnapshot");
    }
    // Only our prefix and this app's exact name are ever deleted. An active
    // slot is never terminated; a failed cleanup is retried at relay startup.
    if pool.query("SELECT pg_drop_replication_slot(slot_name) FROM pg_replication_slots WHERE slot_name = $1 AND NOT active AND database = current_database()", &[&slot]).await.is_err() {
        tracing::warn!(app_id = %app, "CDC slot cleanup failed");
    }
    hub.end(&app, start.generation);
}

async fn capture(
    hub: &Hub,
    app: &str,
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
    let publication = zeroship_core::replication_names::publication_name(app)?;
    if pool
        .query(
            "SELECT 1 FROM pg_publication WHERE pubname = $1",
            &[&publication],
        )
        .await?
        .is_empty()
    {
        return Err("app publication is absent".into());
    }
    // THE TENANT BOUNDARY IN THIS STREAM. The publication is relay-owned and
    // spans every database on the datastore, so its membership is NOT a filter:
    // reading namespaces out of it would admit every co-tenant's relations to
    // this subscriber. What separates them is this comparison, against the
    // schema of the ONE database this app is bound to.
    let schema = bound_database_schema(pool, app).await?;
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
            publication_names: &[&publication],
            ..Default::default()
        })
        .await?;
    let mut transactions = TransactionBuffer::new(max_bytes, max_changes);
    let mut relations: HashMap<u32, String> = HashMap::new();
    hub.ready(app, generation);
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
                            hub.publish(app, generation, Event::Resync);
                        } else {
                            for message in batch.changes {
                                let (rel_id, operation) = match message {
                                    M::Insert { rel_id, .. } => (rel_id, Operation::Insert),
                                    M::Update { rel_id, .. } => (rel_id, Operation::Update),
                                    M::Delete { rel_id, .. } => (rel_id, Operation::Delete),
                                    M::Truncate { .. } => {
                                        hub.publish(app, generation, Event::Resync);
                                        continue;
                                    }
                                    _ => return Err("unexpected buffered message".into()),
                                };
                                match relations.get(&rel_id) {
                                    Some(collection) => hub.publish(
                                        app,
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
        let database = test_database(app).as_str().to_owned();
        pool.batch_execute(
            "CREATE SCHEMA IF NOT EXISTS zeroship;
             CREATE TABLE IF NOT EXISTS zeroship.databases (
               id text PRIMARY KEY, status text NOT NULL, schema_epoch int NOT NULL DEFAULT 1
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
             VALUES ($1, $2, $3, 'active') ON CONFLICT (id) DO NOTHING",
            &[
                &zeroship_core::BindingId::mint().as_str().to_owned(),
                &app.to_owned(),
                &database,
            ],
        )
        .await
        .expect("declare the binding");
    }

    #[compio::test]
    async fn committed_changes_fan_out_without_values_and_rollback_stays_silent() {
        let postgres = crate::postgres_fixture::Postgres::start();
        let url = postgres.url();
        let pool = Pool::connect(&url, 4).await.expect("required PostgreSQL");
        let app = zeroship_core::typed_id::generate(zeroship_core::typed_id::APP_PREFIX);
        let sibling = zeroship_core::typed_id::generate(zeroship_core::typed_id::APP_PREFIX);
        let publication = zeroship_core::replication_names::publication_name(&app).unwrap();
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
        let (first, start) = hub.subscribe(&app, 1, 4, 32).unwrap();
        let (second, duplicate) = hub.subscribe(&app, 1, 4, 32).unwrap();
        assert!(duplicate.is_none());
        let task = compio::runtime::spawn(run(
            hub.clone(),
            app.clone(),
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
