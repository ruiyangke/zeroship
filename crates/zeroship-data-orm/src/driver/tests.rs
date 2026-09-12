use super::*;
use std::cell::Cell;

/// An instrumented connection source needs no ORM or host service traits.
#[derive(Debug)]
struct ObservedDriver<D> {
    inner: D,
    acquisitions: Cell<usize>,
}
#[async_trait(?Send)]
impl<D: Driver> Driver for ObservedDriver<D> {
    async fn acquire(&self, kind: LeaseKind) -> Result<Session, DbError> {
        self.acquisitions.set(self.acquisitions.get() + 1);
        self.inner.acquire(kind).await
    }
}

pub(crate) async fn native_commands(driver: impl Driver, blob_type: &str) {
    let driver = ObservedDriver {
        inner: driver,
        acquisitions: Cell::new(0),
    };
    let session = driver.acquire(LeaseKind::Transaction).await.unwrap();
    assert_eq!(driver.acquisitions.get(), 1);
    session.exec("BEGIN", &[]).await.unwrap();
    session.exec(&format!("CREATE TEMP TABLE driver_values (id BIGINT PRIMARY KEY, payload {blob_type}, label TEXT)"), &[]).await.unwrap();
    let bytes = Value::Bytes(vec![0, 255, 128, 39]);
    assert_eq!(
        session
            .exec(
                "INSERT INTO driver_values VALUES ($1, $2, $3)",
                &[1.into(), bytes.clone(), Value::Null]
            )
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        session
            .exec(
                "INSERT INTO driver_values VALUES ($1, $2, $3), ($4, $5, $6)",
                &[
                    2.into(),
                    bytes.clone(),
                    "two".into(),
                    3.into(),
                    Value::Null,
                    "three".into()
                ]
            )
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        session
            .exec(
                "UPDATE driver_values SET label = $1 WHERE id > $2",
                &["updated".into(), 1.into()]
            )
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        session
            .exec("DELETE FROM driver_values WHERE id = $1", &[3.into()])
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        session
            .exec("DELETE FROM driver_values WHERE id = $1", &[3.into()])
            .await
            .unwrap(),
        0
    );
    let rows = session
        .query(
            "SELECT id, payload, label FROM driver_values ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["payload"], bytes);
    assert_eq!(rows[0]["label"], Value::Null);
    assert_eq!(rows[1]["label"], Value::from("updated"));
    session
        .exec("SAVEPOINT driver_savepoint", &[])
        .await
        .unwrap();
    session
        .exec("UPDATE driver_values SET label = $1", &["discarded".into()])
        .await
        .unwrap();
    session
        .exec("ROLLBACK TO SAVEPOINT driver_savepoint", &[])
        .await
        .unwrap();
    let rows = session
        .query("SELECT label FROM driver_values WHERE id = $1", &[2.into()])
        .await
        .unwrap();
    assert_eq!(rows[0]["label"], Value::from("updated"));
    session.exec("DROP TABLE driver_values", &[]).await.unwrap();
    let (terminal, error) = session.settle(SettleIntent::Commit).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(terminal, TerminalResult::Committed);
}
