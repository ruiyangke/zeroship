//! One transaction frame admits one outstanding operation at a time, and a
//! frame is not ended while one of its operations is still running.
//!
//! A native operation is issued by its future's first poll, which is where it
//! claims its frame; a future prepared and never polled has issued nothing.
//! Settlement reads the same claims: it waits until none is held on the frame
//! it ends, and a callback that returned while one was held has its frame
//! rolled back as unfinished work.
//!
//! Every arm drives its schedule explicitly. [`Hold`] keeps an operation's
//! statement outstanding until the arm releases it, so the verdict never
//! depends on how quickly a backend answers.

#![expect(
    clippy::future_not_send,
    reason = "ORM fixtures use local compio sessions"
)]

use super::fixtures::CollectionFixture;
use super::*;

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

fn record(id: &str) -> Value {
    value!({"id": id, "label": id})
}

/// Keeps the next statement the handle's backend runs from finishing until
/// [`Self::release`], so the operation that sent it is still outstanding when
/// the arm polls the next one.
///
/// SQLite answers from an actor thread, which could reply before the future is
/// polled again, so a test gate stalls the actor. PostgreSQL needs none: a
/// first poll leaves the statement on the wire, and its answer can only arrive
/// on a later turn of the runtime.
struct Hold(Option<crate::backend::sqlite::session::NextCommandGate>);

impl Hold {
    fn arm(database: &Database) -> Self {
        Self(
            database
                .backend
                .get_rc::<crate::backend::sqlite::SqliteBackend>()
                .map(|backend| backend.arm_next_command_gate_for_tests()),
        )
    }

    async fn release(self) {
        if let Some(gate) = self.0 {
            gate.wait_until_blocked()
                .await
                .expect("the held statement reached the actor");
            gate.release();
        }
    }
}

/// The committed ids, sorted, read on a fresh autocommit call.
async fn committed_ids(database: &Database) -> Vec<String> {
    let Output::Rows { rows, .. } = database
        .collection("records")
        .unwrap()
        .find(value!({}), value!({"orderBy": {"id": 1}}))
        .await
        .unwrap()
    else {
        panic!("find must return rows")
    };
    rows.iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect()
}

fn hint_of(error: &DbError) -> Option<&str> {
    match error {
        DbError::ValidationFailed { hint, .. } | DbError::Coded { hint, .. } => hint.as_deref(),
        _ => None,
    }
}

/// Assert `result` is the documented refusal `code`, carrying a hint that names
/// the remedy.
fn assert_refused<T: std::fmt::Debug>(result: Result<T, DbError>, code: &str, what: &str) {
    let error = result.expect_err(what);
    assert_eq!(error.code(), code, "{what}: {error:?}");
    assert!(
        hint_of(&error).is_some_and(|hint| hint.contains("Await")),
        "{what}: the refusal must carry the hint that names the remedy; got {error:?}"
    );
}

fn assert_connection_busy<T: std::fmt::Debug>(result: Result<T, DbError>, what: &str) {
    assert_refused(result, "transaction_connection_busy", what);
}

async fn refuses_an_operation_polled_while_another_is_outstanding(postgres: bool) {
    let fixture = fixture(postgres).await;
    let (first, second, third) = fixture
        .database
        .transaction(|tx| async move {
            let records = tx.collection("records")?;
            let hold = Hold::arm(&tx);
            // `join!` polls `first`, which claims the frame and is held on the
            // wire, then `second`, which is refused, and only then the release.
            let (first, second, ()) = futures::join!(
                records.insert(record("first")),
                records.insert(record("second")),
                hold.release(),
            );
            // Both claims are gone once both have settled - the finished one and
            // the refused one - so the next operation is admitted.
            let third = records.insert(record("third")).await;
            Ok::<_, DbError>((first, second, third))
        })
        .await
        .unwrap();
    first.expect("the first operation owns the frame and runs");
    assert_connection_busy(second, "an operation polled while another was outstanding");
    third.expect("an operation polled after both settled runs");
    assert_eq!(
        committed_ids(&fixture.database).await,
        ["first", "third"],
        "the refused operation must never reach the connection"
    );
    fixture.close().await;
}

#[compio::test]
async fn sqlite_refuses_an_operation_polled_while_another_is_outstanding() {
    refuses_an_operation_polled_while_another_is_outstanding(false).await;
}

#[compio::test]
async fn postgres_refuses_an_operation_polled_while_another_is_outstanding() {
    refuses_an_operation_polled_while_another_is_outstanding(true).await;
}

/// A native future issues nothing until it is polled, so futures prepared
/// together and awaited one after another never overlap, and one prepared and
/// dropped unpolled never claimed anything.
async fn futures_prepared_together_and_awaited_in_turn_all_run(postgres: bool) {
    let fixture = fixture(postgres).await;
    fixture
        .database
        .transaction(|tx| async move {
            let records = tx.collection("records")?;
            let a = records.insert(record("a"));
            let b = records.insert(record("b"));
            drop(records.insert(record("abandoned")));
            a.await?;
            b.await?;
            records.insert(record("c")).await?;
            let Output::Count(count) = records.count(value!({}), value!({})).await? else {
                panic!("count must return a count")
            };
            assert_eq!(
                count, 3,
                "every awaited operation ran inside the transaction"
            );
            Ok::<_, DbError>(())
        })
        .await
        .unwrap();
    assert_eq!(committed_ids(&fixture.database).await, ["a", "b", "c"]);
    fixture.close().await;
}

#[compio::test]
async fn sqlite_futures_prepared_together_and_awaited_in_turn_all_run() {
    futures_prepared_together_and_awaited_in_turn_all_run(false).await;
}

#[compio::test]
async fn postgres_futures_prepared_together_and_awaited_in_turn_all_run() {
    futures_prepared_together_and_awaited_in_turn_all_run(true).await;
}

/// A nested transaction is an operation of its parent frame: first polled
/// while a parent operation is outstanding, it is refused, and the parent frame
/// is free again once it is gone.
async fn a_nested_transaction_is_an_operation_of_its_parent_frame(postgres: bool) {
    let fixture = fixture(postgres).await;
    let (parent_before, nested, parent_after) = fixture
        .database
        .transaction(|tx| async move {
            let records = tx.collection("records")?;
            let hold = Hold::arm(&tx);
            let (parent_before, nested, ()) = futures::join!(
                records.insert(record("parent-before")),
                tx.transaction(|nested| async move {
                    nested
                        .collection("records")?
                        .insert(record("refused-child"))
                        .await?;
                    Ok::<_, DbError>(())
                }),
                hold.release(),
            );
            let parent_after = records.insert(record("parent-after")).await;
            Ok::<_, DbError>((parent_before, nested, parent_after))
        })
        .await
        .unwrap();
    parent_before.expect("the parent operation owns the frame and runs");
    assert_connection_busy(
        nested,
        "a nested transaction polled while a parent operation was outstanding",
    );
    parent_after.expect("the parent frame is free again once both have settled");
    assert_eq!(
        committed_ids(&fixture.database).await,
        ["parent-after", "parent-before"]
    );
    fixture.close().await;
}

#[compio::test]
async fn sqlite_a_nested_transaction_is_an_operation_of_its_parent_frame() {
    a_nested_transaction_is_an_operation_of_its_parent_frame(false).await;
}

#[compio::test]
async fn postgres_a_nested_transaction_is_an_operation_of_its_parent_frame() {
    a_nested_transaction_is_an_operation_of_its_parent_frame(true).await;
}

/// `insertMany` opens a savepoint of its own and runs several statements in
/// it. Those are the operation's own work, so its claim must not refuse them,
/// while a separate operation polled beside it is refused.
async fn an_atomic_write_frame_runs_under_its_operations_claim(postgres: bool) {
    let fixture = fixture(postgres).await;
    let (many, beside) = fixture
        .database
        .transaction(|tx| async move {
            let records = tx.collection("records")?;
            let hold = Hold::arm(&tx);
            let (many, beside, ()) = futures::join!(
                records.execute(Operation::InsertMany {
                    documents: value!([record("many-a"), record("many-b")]),
                }),
                records.insert(record("beside")),
                hold.release(),
            );
            Ok::<_, DbError>((many, beside))
        })
        .await
        .unwrap();
    let Output::Rows { rows, .. } = many.expect("insertMany owns the frame and runs") else {
        panic!("insertMany must return rows")
    };
    assert_eq!(rows.len(), 2);
    assert_connection_busy(
        beside,
        "an operation polled beside an outstanding insertMany",
    );
    assert_eq!(committed_ids(&fixture.database).await, ["many-a", "many-b"]);
    fixture.close().await;
}

#[compio::test]
async fn sqlite_an_atomic_write_frame_runs_under_its_operations_claim() {
    an_atomic_write_frame_runs_under_its_operations_claim(false).await;
}

#[compio::test]
async fn postgres_an_atomic_write_frame_runs_under_its_operations_claim() {
    an_atomic_write_frame_runs_under_its_operations_claim(true).await;
}

/// Start `id`'s insert on `database`, leave it running on a task of its own,
/// and return the channel its result arrives on. The callback that calls this
/// then returns with the operation still outstanding.
async fn leave_running(
    database: &Database,
    id: &str,
) -> Result<futures::channel::oneshot::Receiver<Result<Output, DbError>>, DbError> {
    let hold = Hold::arm(database);
    let mut operation = Box::pin(database.collection("records")?.insert(record(id)));
    assert!(
        futures::poll!(operation.as_mut()).is_pending(),
        "the held operation is outstanding after its first poll"
    );
    let (sender, receiver) = futures::channel::oneshot::channel();
    compio::runtime::spawn(async move {
        hold.release().await;
        let _ = sender.send(operation.await);
    })
    .detach();
    Ok(receiver)
}

/// A callback that returns while an operation it started is still running:
/// the commit waits for that operation, then rolls the transaction back and
/// fails with `transaction_work_unfinished` rather than committing it.
async fn a_callback_that_returns_with_an_operation_outstanding_rolls_back(postgres: bool) {
    let fixture = fixture(postgres).await;
    let mut running = None;
    let settled = fixture
        .database
        .transaction(|tx| {
            let running = &mut running;
            async move {
                *running = Some(leave_running(&tx, "unfinished").await?);
                Ok::<_, DbError>(())
            }
        })
        .await;
    assert_refused(
        settled,
        "transaction_work_unfinished",
        "a callback that returned with an operation outstanding",
    );
    running
        .expect("the callback started the operation")
        .await
        .expect("the operation reported its result")
        .expect("the operation ran to completion before the rollback");
    assert_eq!(
        committed_ids(&fixture.database).await,
        Vec::<String>::new(),
        "the unfinished operation's row must be rolled back, not committed"
    );
    fixture.close().await;
}

#[compio::test]
async fn sqlite_a_callback_that_returns_with_an_operation_outstanding_rolls_back() {
    a_callback_that_returns_with_an_operation_outstanding_rolls_back(false).await;
}

#[compio::test]
async fn postgres_a_callback_that_returns_with_an_operation_outstanding_rolls_back() {
    a_callback_that_returns_with_an_operation_outstanding_rolls_back(true).await;
}

/// The same inside a nested transaction: its frame is rolled back and it fails
/// as unfinished, while the parent carries on and commits its own work.
async fn a_nested_callback_that_returns_with_an_operation_outstanding_rolls_back(postgres: bool) {
    let fixture = fixture(postgres).await;
    let (nested, running) = fixture
        .database
        .transaction(|tx| async move {
            let mut running = None;
            let nested = tx
                .transaction(|nested| {
                    let running = &mut running;
                    async move {
                        *running = Some(leave_running(&nested, "child").await?);
                        Ok::<_, DbError>(())
                    }
                })
                .await;
            tx.collection("records")?.insert(record("parent")).await?;
            Ok::<_, DbError>((nested, running))
        })
        .await
        .unwrap();
    assert_refused(
        nested,
        "transaction_work_unfinished",
        "a nested callback that returned with an operation outstanding",
    );
    running
        .expect("the nested callback started the operation")
        .await
        .expect("the operation reported its result")
        .expect("the operation ran to completion before the nested rollback");
    assert_eq!(committed_ids(&fixture.database).await, ["parent"]);
    fixture.close().await;
}

#[compio::test]
async fn sqlite_a_nested_callback_that_returns_with_an_operation_outstanding_rolls_back() {
    a_nested_callback_that_returns_with_an_operation_outstanding_rolls_back(false).await;
}

#[compio::test]
async fn postgres_a_nested_callback_that_returns_with_an_operation_outstanding_rolls_back() {
    a_nested_callback_that_returns_with_an_operation_outstanding_rolls_back(true).await;
}

/// A callback that fails while an operation it started is still running: the
/// rollback waits for the operation, then rolls it back with everything else,
/// and the callback's own error is what the caller sees.
async fn a_failed_callback_waits_for_its_operation_then_rolls_back(postgres: bool) {
    let fixture = fixture(postgres).await;
    let mut running = None;
    let settled = fixture
        .database
        .transaction(|tx| {
            let running = &mut running;
            async move {
                *running = Some(leave_running(&tx, "in-flight").await?);
                Err::<(), _>(DbError::validation(
                    "callback_failed",
                    "the callback failed",
                ))
            }
        })
        .await;
    assert_eq!(
        settled.expect_err("the callback failed").code(),
        "callback_failed",
        "a failed callback reports its own error"
    );
    running
        .expect("the callback started the operation")
        .await
        .expect("the operation reported its result")
        .expect("the operation ran to completion before the rollback");
    assert_eq!(committed_ids(&fixture.database).await, Vec::<String>::new());
    fixture.close().await;
}

#[compio::test]
async fn sqlite_a_failed_callback_waits_for_its_operation_then_rolls_back() {
    a_failed_callback_waits_for_its_operation_then_rolls_back(false).await;
}

#[compio::test]
async fn postgres_a_failed_callback_waits_for_its_operation_then_rolls_back() {
    a_failed_callback_waits_for_its_operation_then_rolls_back(true).await;
}

/// A nested callback that fails while an operation it started is still on the
/// wire: the nested rollback waits for the operation, then rolls the frame
/// back, and the parent carries on.
///
/// [`Hold`] keeps the operation's statement at the backend when the callback
/// returns, so the rollback deterministically meets a frame whose connection
/// is in use. Without the wait the rollback was refused there, reported
/// `rollback_failed_indeterminate` with the child frame still open, and the
/// parent could not use its own connection again.
async fn a_failed_nested_callback_waits_for_its_running_operation_then_rolls_back(postgres: bool) {
    let fixture = fixture(postgres).await;
    let (nested, running) = fixture
        .database
        .transaction(|tx| async move {
            let mut running = None;
            let nested = tx
                .transaction(|nested| {
                    let running = &mut running;
                    async move {
                        *running = Some(leave_running(&nested, "child").await?);
                        Err::<(), _>(DbError::validation(
                            "callback_failed",
                            "the nested callback failed",
                        ))
                    }
                })
                .await;
            tx.collection("records")?.insert(record("parent")).await?;
            Ok::<_, DbError>((nested, running))
        })
        .await
        .unwrap();
    assert_eq!(
        nested.expect_err("the nested callback failed").code(),
        "callback_failed",
        "the nested rollback succeeded and reports the callback's own error"
    );
    running
        .expect("the nested callback started the operation")
        .await
        .expect("the operation reported its result")
        .expect("the operation ran to completion before the nested rollback");
    assert_eq!(committed_ids(&fixture.database).await, ["parent"]);
    fixture.close().await;
}

#[compio::test]
async fn sqlite_a_failed_nested_callback_waits_for_its_running_operation_then_rolls_back() {
    a_failed_nested_callback_waits_for_its_running_operation_then_rolls_back(false).await;
}

#[compio::test]
async fn postgres_a_failed_nested_callback_waits_for_its_running_operation_then_rolls_back() {
    a_failed_nested_callback_waits_for_its_running_operation_then_rolls_back(true).await;
}

/// An operation first polled after its callback returned belongs to a
/// transaction that is ending: it is refused as expired, and the transaction
/// commits its own work without it.
async fn an_operation_first_polled_after_its_callback_returned_is_refused(postgres: bool) {
    let fixture = fixture(postgres).await;
    let mut late = None;
    fixture
        .database
        .transaction(|tx| {
            let late = &mut late;
            async move {
                let records = tx.collection("records")?;
                records.insert(record("own")).await?;
                // Prepared here, first polled by its own task after the
                // callback has returned and its settle has begun.
                let operation = records.insert(record("late"));
                let (sender, receiver) = futures::channel::oneshot::channel();
                compio::runtime::spawn(async move {
                    let _ = sender.send(operation.await);
                })
                .detach();
                *late = Some(receiver);
                Ok::<_, DbError>(())
            }
        })
        .await
        .expect("the transaction commits its own work");
    let refused = late
        .expect("the callback prepared the operation")
        .await
        .expect("the operation reported its result");
    assert_eq!(
        refused.expect_err("the late operation is refused").code(),
        "transaction_scope_expired"
    );
    assert_eq!(committed_ids(&fixture.database).await, ["own"]);
    fixture.close().await;
}

#[compio::test]
async fn sqlite_an_operation_first_polled_after_its_callback_returned_is_refused() {
    an_operation_first_polled_after_its_callback_returned_is_refused(false).await;
}

#[compio::test]
async fn postgres_an_operation_first_polled_after_its_callback_returned_is_refused() {
    an_operation_first_polled_after_its_callback_returned_is_refused(true).await;
}

/// A future the callback polled and then parked, neither driving nor dropping
/// it, still holds its frame's claim, so settlement waits for it. The
/// transaction's execution deadline ends that wait, and nothing the
/// transaction wrote is committed.
async fn a_parked_operation_holds_settlement_until_the_execution_deadline(postgres: bool) {
    type Parked = std::pin::Pin<Box<dyn Future<Output = Result<Output, DbError>>>>;
    let fixture = fixture(postgres).await;
    // The pinned transaction borrows the fixture until this block ends.
    {
        let database = &fixture.database;
        let parked = std::cell::RefCell::new(None::<Parked>);
        let hold = std::cell::RefCell::new(None::<Hold>);
        let (callback_returned, mut returned) = futures::channel::oneshot::channel::<()>();
        let mut transaction = std::pin::pin!(database.transaction(|tx| {
            let (parked, hold) = (&parked, &hold);
            async move {
                let records = tx.collection("records")?;
                records.insert(record("own")).await?;
                *hold.borrow_mut() = Some(Hold::arm(&tx));
                let mut operation: Parked = Box::pin(records.insert(record("parked")));
                assert!(
                    futures::poll!(operation.as_mut()).is_pending(),
                    "the parked operation is outstanding after its first poll"
                );
                *parked.borrow_mut() = Some(operation);
                let _ = callback_returned.send(());
                Ok::<_, DbError>(())
            }
        }));
        match futures::future::select(transaction.as_mut(), &mut returned).await {
            futures::future::Either::Right(_) => {}
            futures::future::Either::Left((settled, _)) => {
                panic!("the transaction settled before its callback returned: {settled:?}")
            }
        }
        assert!(
            futures::poll!(transaction.as_mut()).is_pending(),
            "settlement waits for the parked operation"
        );
        let route = database.binding.route();
        let gate = hold.borrow_mut().take();
        let (fired, (), settled) = futures::join!(
            database
                .context
                .scope(crate::transaction::probe::fire_execution_deadline(&route)),
            async {
                if let Some(gate) = gate {
                    gate.release().await;
                }
            },
            transaction,
        );
        assert!(
            fired.refused.is_none(),
            "the deadline was delivered: {fired:?}"
        );
        // SQLite rolls back once the held statement is let through. On
        // PostgreSQL the parked statement is still on the wire, so the session
        // is withdrawn instead and the outcome is reported as indeterminate.
        // The settle is a ROLLBACK (the callback left work unfinished), so the
        // unknown is a failed rollback, not a commit that may have happened.
        let expected = if postgres {
            "rollback_failed_indeterminate"
        } else {
            "transaction_deadline_expired"
        };
        let error = settled.expect_err("the deadline ended the transaction");
        assert_eq!(error.code(), expected, "{error:?}");
        assert!(
            error.message_str().contains("transaction_deadline_expired"),
            "{error:?}"
        );
        let parked = parked.borrow_mut().take().expect("the callback parked it");
        parked
            .await
            .expect_err("the parked operation belongs to a transaction that ended");
        assert_eq!(committed_ids(database).await, Vec::<String>::new());
    }
    fixture.close().await;
}

#[compio::test]
async fn sqlite_a_parked_operation_holds_settlement_until_the_execution_deadline() {
    a_parked_operation_holds_settlement_until_the_execution_deadline(false).await;
}

#[compio::test]
async fn postgres_a_parked_operation_holds_settlement_until_the_execution_deadline() {
    a_parked_operation_holds_settlement_until_the_execution_deadline(true).await;
}
