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
    let mut relations: HashMap<u32, Option<String>> = HashMap::new();
    hub.ready(app, generation);
    while let Some(message) = stream.next().await? {
        match message {
            ReplicationMessage::XLogData { body, .. } => {
                let message = pgoutput::decode(&body)?;
                match transactions
                    .push(message)
                    .map_err(|_| "invalid transaction sequence")?
                {
                    Action::Pending => {}
                    Action::Metadata(pgoutput::PgOutputMessage::Relation {
                        rel_id,
                        namespace,
                        name,
                        ..
                    }) => {
                        if !relations.contains_key(&rel_id) && relations.len() >= max_relations {
                            return Err("relation cache capacity exhausted".into());
                        }
                        let name = (namespace == app && !name.starts_with("__")).then_some(name);
                        relations.insert(rel_id, name);
                    }
                    Action::Metadata(_) => return Err("unexpected transaction metadata".into()),
                    Action::Commit(batch) => {
                        if batch.needs_resync {
                            hub.publish(app, generation, Event::Resync);
                        } else {
                            for message in batch.changes {
                                use pgoutput::PgOutputMessage as M;
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
                                    Some(Some(collection)) => hub.publish(
                                        app,
                                        generation,
                                        Event::Change {
                                            collection: collection.clone(),
                                            operation,
                                        },
                                    ),
                                    Some(None) => {}
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

    #[compio::test]
    async fn committed_changes_fan_out_without_values_and_rollback_stays_silent() {
        let postgres = crate::postgres_fixture::Postgres::start();
        let url = postgres.url();
        let pool = Pool::connect(&url, 4).await.expect("required PostgreSQL");
        let app = zeroship_core::typed_id::generate(zeroship_core::typed_id::APP_PREFIX);
        let publication = zeroship_core::replication_names::publication_name(&app).unwrap();
        let ddl = format!("CREATE SCHEMA \"{app}\"; CREATE TABLE \"{app}\".orders (id int PRIMARY KEY, secret text); CREATE TABLE \"{app}\".rolled_back (id int); CREATE PUBLICATION \"{publication}\" FOR TABLES IN SCHEMA \"{app}\"");
        pool.batch_execute(&ddl)
            .await
            .expect("logical WAL and publication required");
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
                max_relations: 100,
            },
        ));
        assert_eq!(event(&first).await, Event::Ready);
        assert_eq!(event(&second).await, Event::Ready);
        let writer = pool.acquire().await.unwrap();
        writer.batch_execute(&format!("BEGIN; INSERT INTO \"{app}\".rolled_back VALUES (1); ROLLBACK; BEGIN; INSERT INTO \"{app}\".orders VALUES (1, 'must-never-reach-a-worker')")).await.unwrap();
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
            .batch_execute(&format!("TRUNCATE \"{app}\".orders"))
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
            "DROP PUBLICATION \"{publication}\"; DROP SCHEMA \"{app}\" CASCADE"
        ))
        .await
        .unwrap();
        pool.close().await;
    }
}
