//! Usage a database reports through the sink its host attached.
use super::*;
use fixtures::CollectionFixture;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Totals what the ORM reports, per metric.
#[derive(Debug, Default)]
struct Totals(Mutex<BTreeMap<String, u64>>);

impl crate::metrics::UsageSink for Totals {
    fn record(&self, metric: &str, amount: u64) {
        *self
            .0
            .lock()
            .expect("usage totals")
            .entry(metric.to_owned())
            .or_default() += amount;
    }
}

impl Totals {
    /// Everything reported since the last call.
    fn take(&self) -> BTreeMap<String, u64> {
        std::mem::take(&mut *self.0.lock().expect("usage totals"))
    }
}

fn totals(entries: &[(&str, u64)]) -> BTreeMap<String, u64> {
    entries
        .iter()
        .map(|(metric, amount)| ((*metric).to_owned(), *amount))
        .collect()
}

fn fields() -> Value {
    value!({
        "id": {"type":"string", "primaryKey":true, "required":true},
        "label": {"type":"string", "required":true}
    })
}

async fn fixture(postgres: bool) -> CollectionFixture {
    let columns = "id TEXT PRIMARY KEY, label TEXT NOT NULL";
    if postgres {
        CollectionFixture::postgres_from_table_definition("records", fields(), columns).await
    } else {
        CollectionFixture::sqlite_from_table_definition("records", fields(), columns).await
    }
}

async fn usage_follows_the_attached_sink(postgres: bool) {
    let fixture = fixture(postgres).await;
    let sink = Arc::new(Totals::default());
    let metered = fixture
        .database
        .clone()
        .with_usage_sink(Arc::clone(&sink) as _);
    let records = metered.collection("records").unwrap();

    records
        .insert(value!({"id":"a", "label":"first"}))
        .await
        .unwrap();
    records.find(value!({}), value!({})).await.unwrap();
    assert_eq!(
        sink.take(),
        totals(&[("db_reads", 1), ("db_rows_written", 1), ("db_writes", 1)]),
        "each operation reports once"
    );

    metered
        .transaction(|tx| async move {
            tx.collection("records")?
                .insert(value!({"id":"b", "label":"second"}))
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        sink.take(),
        totals(&[("db_rows_written", 1), ("db_writes", 1)]),
        "a transaction reports to the sink of the database that opened it"
    );

    records
        .insert(value!({"id":"a", "label":"duplicate"}))
        .await
        .expect_err("a duplicate key must be refused");
    assert_eq!(
        sink.take(),
        totals(&[]),
        "a failed operation reports nothing"
    );

    // The same binding without a sink serves the same work and reports nothing.
    let unmetered = fixture.database.collection("records").unwrap();
    unmetered
        .insert(value!({"id":"c", "label":"third"}))
        .await
        .unwrap();
    let count = unmetered.count(value!({}), value!({})).await.unwrap();
    assert!(matches!(count, Output::Count(3)), "{count:?}");
    assert_eq!(
        sink.take(),
        totals(&[]),
        "a database without a sink reports nothing"
    );
}

#[compio::test]
async fn sqlite_usage_follows_the_attached_sink() {
    usage_follows_the_attached_sink(false).await;
}

#[compio::test]
async fn postgres_usage_follows_the_attached_sink() {
    usage_follows_the_attached_sink(true).await;
}
