use super::fixtures::*;
use zeroship_data_orm::value;

#[test]
fn native_transaction_isolation_refusals_keep_the_parent_usable() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let alias = crate::tests::fixtures::harness_alias(LOCAL_DEV_APP_ID);
        apply_schema_ahead_of_runtime(
            &dir,
            &format!(
                "CREATE TABLE \"{alias}\".notes ({SYSTEM_COLUMNS_SQLITE}, title TEXT NOT NULL);"
            ),
        );
        let source = sqlite_runtime_source(
            "notes",
            &value!({"title":{"type":"string", "required":true}}),
            r#"
const _procedures = {
  async transactionIsolation() {
    let entered = 0;
    const unsupported = [];
    for (const isolationLevel of ["read uncommitted", "read committed", "repeatable read"]) {
      const result = await env.db.transaction(async () => { entered++; }, { isolationLevel });
      unsupported.push(result.error?.code ?? "accepted");
    }
    let nestedCode;
    await env.db.transaction(async outer => {
      const nested = await env.db.transaction(async () => { entered++; }, { isolationLevel: "serializable" });
      nestedCode = nested.error?.code ?? "accepted";
      await env.db.transaction(async inner => {
        await inner.collection(COLLECTION).insert({ title: "inner" });
      });
      await outer.collection(COLLECTION).insert({ title: "outer" });
    }, { isolationLevel: "serializable" });
    const rows = await env.db.collection(COLLECTION).find({}, { orderBy: { title: 1 } });
    return { unsupported, nestedCode, entered, titles: rows.map(row => row.title) };
  },
};
"#,
        );
        assert_eq!(
            dispatch_sqlite_runtime(&dir, &source, "transactionIsolation"),
            value!({"json":{
                "unsupported":["unsupported_isolation_level", "unsupported_isolation_level", "unsupported_isolation_level"],
                "nestedCode":"nested_isolation_level",
                "entered":0,
                "titles":["inner", "outer"],
            }})
        );
    });
}

#[test]
fn native_transaction_collections_expire_with_their_own_frame() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let alias = crate::tests::fixtures::harness_alias(LOCAL_DEV_APP_ID);
        apply_schema_ahead_of_runtime(
            &dir,
            &format!(
                "CREATE TABLE \"{alias}\".notes ({SYSTEM_COLUMNS_SQLITE}, title TEXT NOT NULL);"
            ),
        );
        let source = sqlite_runtime_source(
            "notes",
            &value!({"title":{"type":"string", "required":true}}),
            r#"
const nativeTransaction = Object.getPrototypeOf(env.db).transaction.bind(env.db);

async function expectExpired(operation) {
  try {
    await operation();
    return "accepted";
  } catch (error) {
    return error?.code ?? "missing_code";
  }
}

const _procedures = {
  async transactionScope() {
    let outerView;
    let outerNotes;
    let innerView;
    let innerNotes;
    let innerViewAfterSettle;
    let innerCollectionAfterSettle;

    await nativeTransaction(async outer => {
      outerView = outer;
      outerNotes = outer.collection(COLLECTION);
      await outerNotes.insert({ title: "outer-before" });

      await nativeTransaction(async inner => {
        innerView = inner;
        innerNotes = inner.collection(COLLECTION);
        await innerNotes.insert({ title: "inner" });
      });

      innerViewAfterSettle = await expectExpired(
        () => innerView.collection(COLLECTION).insert({ title: "escaped-inner-view" }),
      );
      innerCollectionAfterSettle = await expectExpired(
        () => innerNotes.insert({ title: "escaped-inner-collection" }),
      );
      await outerNotes.insert({ title: "outer-after" });
    });

    const outerViewAfterSettle = await expectExpired(
      () => outerView.collection(COLLECTION).insert({ title: "escaped-outer-view" }),
    );
    const outerCollectionAfterSettle = await expectExpired(
      () => outerNotes.insert({ title: "escaped-outer-collection" }),
    );
    const rows = await env.db.collection(COLLECTION).find({}, { orderBy: { title: 1 } });

    return {
      innerViewAfterSettle,
      innerCollectionAfterSettle,
      outerViewAfterSettle,
      outerCollectionAfterSettle,
      titles: rows.map(row => row.title),
    };
  },
};
"#,
        );

        let response = dispatch_sqlite_runtime(&dir, &source, "transactionScope");
        assert_eq!(
            response,
            value!({"json":{
                "innerViewAfterSettle":"transaction_scope_expired",
                "innerCollectionAfterSettle":"transaction_scope_expired",
                "outerViewAfterSettle":"transaction_scope_expired",
                "outerCollectionAfterSettle":"transaction_scope_expired",
                "titles":["inner", "outer-after", "outer-before"],
            }})
        );
    });
}

/// The table the overlap arms write, in the dev database file the runtime
/// attaches for [`LOCAL_DEV_APP_ID`].
fn notes_table(dir: &tempfile::TempDir) {
    let alias = crate::tests::fixtures::harness_alias(LOCAL_DEV_APP_ID);
    apply_schema_ahead_of_runtime(
        dir,
        &format!("CREATE TABLE \"{alias}\".notes ({SYSTEM_COLUMNS_SQLITE}, title TEXT NOT NULL);"),
    );
}

/// The dispatch-level overlap arms, which drive each queued operation by hand
/// so the schedule is the test's rather than the pump's.
mod dispatch {
    use super::*;
    use std::{cell::RefCell, collections::HashMap, rc::Rc};
    use zeroship_data_orm::binding::DbBinding;
    use zeroship_data_orm::transaction::scope::TransactionScope;
    use zeroship_runtime::state::{OpErrorKind, OpResult, ResolveValue};
    use zeroship_runtime::{init_v8, RuntimeState, SharedState};

    /// A dev-tier transaction opened through the ORM protocol, exactly as the
    /// native `db.transaction` begin does, with the scope its callback runs in.
    struct DevTransaction {
        runtime: compio::runtime::Runtime,
        binding: DbBinding,
        callback: TransactionScope,
    }

    fn open(dir: &tempfile::TempDir) -> DevTransaction {
        notes_table(dir);
        crate::tests::fixtures::reset_context();
        crate::tests::fixtures::set_database_url(&format!(
            "sqlite:{}",
            dir.path().join("control.sqlite").display()
        ));
        let binding = crate::tests::fixtures::binding(LOCAL_DEV_APP_ID);
        crate::tests::fixtures::install_schema(
            &binding,
            "notes",
            value!({"title":{"type":"string", "required":true}}),
        );
        let route = binding.route();
        let runtime = compio::runtime::Runtime::new().expect("compio runtime build");
        runtime.block_on(async {
            let backend = crate::tx_scope::ensure_backend()
                .await
                .expect("open the sqlite backend");
            let admission = crate::transaction::TxAdmission::acquire(route.clone())
                .await
                .expect("claim the free lane");
            let frame = crate::transaction::exec_begin_or_savepoint(false, None, &binding, backend)
                .await
                .expect("BEGIN");
            assert!(frame.is_none(), "a top-level begin opens no savepoint");
            admission.handed_to_reducer();
        });
        let callback = TransactionScope::current(&route).expect("the transaction is open");
        DevTransaction {
            runtime,
            binding,
            callback,
        }
    }

    type Queued = std::pin::Pin<Box<dyn std::future::Future<Output = OpResult>>>;

    /// What a queued operation settled to: `Ok` when it resolved, its code and
    /// hint when it was refused.
    fn outcome(result: OpResult) -> Result<(), (String, Option<String>)> {
        match result {
            OpResult::JsValue {
                value: ResolveValue::RejectError(error),
                ..
            } => match error.kind {
                OpErrorKind::CodedError { code, hint, .. } => Err((code, hint)),
                other => panic!("expected a coded refusal, got {other:?}"),
            },
            OpResult::JsValue { .. } => Ok(()),
            _ => panic!("a db dispatch settles through OpResult::JsValue"),
        }
    }

    impl DevTransaction {
        /// Drive one queued operation to completion.
        fn settle(&self, operation: Queued) -> Result<(), (String, Option<String>)> {
            outcome(self.runtime.block_on(operation))
        }

        /// Open a nested frame, as a nested `db.transaction()` does, and return
        /// it with the scope its callback runs in.
        fn nested(
            &self,
        ) -> (
            zeroship_data_orm::transaction::reducer::frames::FrameId,
            TransactionScope,
        ) {
            let frame = self.runtime.block_on(async {
                let backend = crate::tx_scope::ensure_backend()
                    .await
                    .expect("the backend is open");
                crate::transaction::exec_begin_or_savepoint(true, None, &self.binding, backend)
                    .await
                    .expect("SAVEPOINT")
                    .expect("a nested begin opens a frame")
            });
            let scope =
                TransactionScope::current(&self.binding.route()).expect("the child frame is open");
            assert_eq!(scope.frame(), frame.get(), "the child frame is the top");
            (frame, scope)
        }

        /// Run a settlement the moment its callback settled, beside the
        /// operation the callback left outstanding. The settlement is polled
        /// FIRST, so it is the settlement's own wait - not the order the
        /// executor happens to pick - that lets the operation finish.
        fn settle_beside(
            &self,
            settle: impl std::future::Future<Output = crate::transaction::SettleOutcome>,
            operation: Queued,
        ) -> (
            crate::transaction::SettleOutcome,
            Result<(), (String, Option<String>)>,
        ) {
            let (settled, operation) = self
                .runtime
                .block_on(async { futures::join!(settle, operation) });
            (settled, outcome(operation))
        }

        /// Commit, then read back what the transaction left behind on a
        /// connection of the fixture's own.
        fn commit_and_read_titles(&self, dir: &tempfile::TempDir) -> Vec<String> {
            match self.runtime.block_on(crate::transaction::exec_settle(
                &self.binding.route(),
                true,
                None,
            )) {
                crate::transaction::SettleOutcome::Ok => {}
                other => panic!("the transaction must commit, got {other:?}"),
            }
            self.titles(dir)
        }

        /// The rows committed so far, read on a connection of the fixture's own.
        fn titles(&self, dir: &tempfile::TempDir) -> Vec<String> {
            let alias = crate::tests::fixtures::harness_alias(LOCAL_DEV_APP_ID);
            self.runtime
                .block_on(
                    crate::tests::fixtures::sqlite::Inspector::open(dir.path()).query(
                        &format!("SELECT title FROM \"{alias}\".notes ORDER BY title"),
                        &[],
                    ),
                )
                .expect("read the committed rows")
                .into_iter()
                .map(|row| row[0].clone().expect("a title"))
                .collect()
        }
    }

    fn assert_busy(refusal: Result<(), (String, Option<String>)>, what: &str) {
        let (code, hint) = refusal.expect_err(what);
        assert_eq!(code, "transaction_connection_busy", "{what}");
        assert!(
            hint.as_deref().is_some_and(|hint| hint.contains("Await")),
            "{what}: the refusal must carry the hint that names the remedy; got {hint:?}"
        );
    }

    fn method<'s>(
        scope: &v8::PinScope<'s, '_>,
        receiver: v8::Local<'s, v8::Object>,
        name: &str,
    ) -> v8::Local<'s, v8::Function> {
        let key = v8::String::new(scope, name).unwrap();
        receiver
            .get(scope, key.into())
            .unwrap_or_else(|| panic!("`{name}` is present"))
            .try_into()
            .unwrap_or_else(|_| panic!("`{name}` is a function"))
    }

    /// Issue `notes.insert({ title })`, as a creator's call does.
    fn insert<'s>(scope: &v8::PinScope<'s, '_>, notes: v8::Local<'s, v8::Object>, title: &str) {
        let insert = method(scope, notes, "insert");
        let source = v8::String::new(scope, &format!(r#"{{"title":"{title}"}}"#)).unwrap();
        let document = v8::json::parse(scope, source).expect("parse the document");
        assert!(
            insert.call(scope, notes.into(), &[document]).is_some(),
            "insert threw synchronously; the dispatch was never reached"
        );
    }

    macro_rules! isolate {
        (let $scope:ident, let $state:ident) => {
            init_v8();
            let mut isolate = v8::Isolate::new(v8::CreateParams::default());
            v8::scope!(let handle_scope, &mut isolate);
            let context = v8::Context::new(handle_scope, Default::default());
            let $scope = &mut v8::ContextScope::new(handle_scope, context);
            let $state: SharedState =
                Rc::new(RefCell::new(RuntimeState::new(HashMap::new(), None, None)));
            $scope.set_slot($state.clone());
        };
    }

    fn queued(
        state: &SharedState,
    ) -> Vec<std::pin::Pin<Box<dyn std::future::Future<Output = OpResult>>>> {
        state.borrow_mut().spawned_ops.drain(..).collect()
    }

    /// Two inserts issued in one synchronous turn inside a transaction, with
    /// the schedule a first-poll check admits: the first operation runs to
    /// completion - its statement answered, the connection handed back -
    /// before the second is polled at all.
    ///
    /// The dev tier answers from a dedicated thread, so this is the schedule a
    /// busy dev machine produces on its own. The second insert was issued while
    /// the first was outstanding, so it is refused however quickly the first
    /// finished.
    #[test]
    fn an_operation_issued_beside_an_outstanding_one_is_refused_even_after_it_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let transaction = open(&dir);
        isolate!(let scope, let state);
        let notes = crate::v8_classes::collection::mint_collection(
            scope,
            "notes".to_owned(),
            transaction.binding.clone(),
            Some(transaction.callback.clone()),
        )
        .expect("mint the transaction's collection");
        // Inside the callback's async scope, as `Promise.all` over two
        // `tx.notes.insert(...)` calls issues them.
        let entered = crate::tx_scope::enter(scope, &transaction.callback);
        insert(scope, notes, "first");
        insert(scope, notes, "second");
        crate::tx_scope::leave(scope, entered);

        let mut operations = queued(&state);
        assert_eq!(
            operations.len(),
            2,
            "each insert queues exactly one operation"
        );
        let second = operations.pop().unwrap();
        let first = operations.pop().unwrap();
        transaction
            .settle(first)
            .expect("the first insert owns the frame and runs");
        assert_busy(
            transaction.settle(second),
            "the second insert was issued while the first was outstanding",
        );
        assert_eq!(
            transaction.commit_and_read_titles(&dir),
            ["first"],
            "the refused insert must never reach the connection"
        );
        crate::tests::fixtures::reset_context();
    }

    /// A nested `db.transaction()` is a call of its parent callback. Issued
    /// while another call is outstanding it is refused where it is issued, and
    /// once issued it holds the parent's frame against a sibling call until it
    /// settles or is abandoned.
    #[test]
    fn a_nested_transaction_is_a_call_of_its_parent_callback() {
        let dir = tempfile::tempdir().unwrap();
        let transaction = open(&dir);
        isolate!(let scope, let state);
        let notes = crate::v8_classes::collection::mint_collection(
            scope,
            "notes".to_owned(),
            transaction.binding.clone(),
            Some(transaction.callback.clone()),
        )
        .expect("mint the transaction's collection");
        let source = v8::String::new(scope, "(async () => {})").unwrap();
        let script = v8::Script::compile(scope, source, None).expect("compile the callback");
        let callback: v8::Local<v8::Function> = script
            .run(scope)
            .expect("evaluate the callback")
            .try_into()
            .expect("the callback is a function");

        // Beside an outstanding insert, the nested transaction is refused at
        // once: its promise is already rejected and it queued nothing.
        let entered = crate::tx_scope::enter(scope, &transaction.callback);
        insert(scope, notes, "first");
        let nested = crate::v8_classes::transaction::transaction_dispatch(
            scope,
            callback,
            None,
            transaction.binding.clone(),
        );
        crate::tx_scope::leave(scope, entered);
        assert_eq!(
            nested.state(),
            v8::PromiseState::Rejected,
            "a nested transaction issued beside an outstanding call is refused where it is issued"
        );
        let reason: v8::Local<v8::Object> = nested
            .result(scope)
            .try_into()
            .expect("the refusal is an Error object");
        let code_key = v8::String::new(scope, "code").unwrap();
        let code = reason
            .get(scope, code_key.into())
            .expect("the refusal carries a code")
            .to_rust_string_lossy(scope);
        assert_eq!(code, "transaction_connection_busy");
        let mut operations = queued(&state);
        assert_eq!(operations.len(), 1, "only the insert queued an operation");
        transaction
            .settle(operations.pop().unwrap())
            .expect("the insert that owned the frame runs");

        // Issued first, the nested transaction holds the frame against a
        // sibling insert issued beside it.
        let entered = crate::tx_scope::enter(scope, &transaction.callback);
        let nested = crate::v8_classes::transaction::transaction_dispatch(
            scope,
            callback,
            None,
            transaction.binding.clone(),
        );
        insert(scope, notes, "sibling");
        crate::tx_scope::leave(scope, entered);
        assert_eq!(nested.state(), v8::PromiseState::Pending);
        let mut operations = queued(&state);
        assert_eq!(
            operations.len(),
            2,
            "the nested begin and the sibling insert each queued one"
        );
        let sibling = operations.pop().unwrap();
        let begin = operations.pop().unwrap();
        assert_busy(
            transaction.settle(sibling),
            "a sibling insert issued beside a nested transaction",
        );
        // Abandoned before it sent its SAVEPOINT: dropping it hands the frame
        // back.
        drop(begin);

        let entered = crate::tx_scope::enter(scope, &transaction.callback);
        insert(scope, notes, "after");
        crate::tx_scope::leave(scope, entered);
        let mut operations = queued(&state);
        transaction
            .settle(operations.pop().unwrap())
            .expect("the frame is free again once the nested transaction was abandoned");

        assert_eq!(transaction.commit_and_read_titles(&dir), ["after", "first"]);
        crate::tests::fixtures::reset_context();
    }

    /// The collection a callback running in `frame` is handed.
    fn notes_in<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        transaction: &DevTransaction,
        frame: &TransactionScope,
    ) -> v8::Local<'s, v8::Object> {
        crate::v8_classes::collection::mint_collection(
            scope,
            "notes".to_owned(),
            transaction.binding.clone(),
            Some(frame.clone()),
        )
        .expect("mint the frame's collection")
    }

    fn assert_unfinished(settled: crate::transaction::SettleOutcome, what: &str) {
        match settled {
            crate::transaction::SettleOutcome::SettleErr(error) => {
                assert_eq!(
                    error.code(),
                    "transaction_work_unfinished",
                    "{what}: {error:?}"
                );
            }
            other => panic!("{what}: the settle must refuse unfinished work, got {other:?}"),
        }
    }

    /// A nested callback rejects - its `Promise.all` lost a branch to the
    /// overlap refusal - while the branch that won is still outstanding. The
    /// nested rollback must wait for that call, then roll the frame back.
    ///
    /// The winning call is claimed and has not yet run when the rollback is
    /// requested, so it cannot finish first however quickly the dev tier's
    /// thread answers: without the wait the rollback closes the frame and the
    /// call then runs against a frame that is gone. The same shape with the
    /// call already on the wire is
    /// `a_failed_nested_callback_waits_for_its_running_operation_then_rolls_back`
    /// in the ORM, which can hold a statement at the backend; this crate has no
    /// such gate.
    #[test]
    fn a_nested_rollback_waits_for_the_child_call_still_outstanding() {
        let dir = tempfile::tempdir().unwrap();
        let transaction = open(&dir);
        let (frame, child) = transaction.nested();
        isolate!(let scope, let state);
        let notes = notes_in(scope, &transaction, &child);
        let entered = crate::tx_scope::enter(scope, &child);
        insert(scope, notes, "a");
        insert(scope, notes, "b");
        crate::tx_scope::leave(scope, entered);
        let mut operations = queued(&state);
        let second = operations.pop().unwrap();
        let first = operations.pop().unwrap();
        assert_busy(transaction.settle(second), "the second child call");

        let rollback =
            crate::transaction::exec_settle(&transaction.binding.route(), false, Some(frame));
        let (settled, first) = transaction.settle_beside(rollback, first);
        assert!(
            matches!(settled, crate::transaction::SettleOutcome::Ok),
            "the nested rollback must succeed once the child call finished: {settled:?}"
        );
        first.expect("the child call that owned the frame finished before the rollback");
        assert_eq!(
            TransactionScope::current(&transaction.binding.route())
                .expect("the root is open")
                .frame(),
            transaction.callback.frame(),
            "the rollback closed the child frame"
        );
        assert_eq!(
            transaction.commit_and_read_titles(&dir),
            Vec::<String>::new(),
            "the failed child's row must not commit with the root"
        );
        crate::tests::fixtures::reset_context();
    }

    /// A call made after its callback settled belongs to a transaction that is
    /// ending. It is refused as expired where it is made, and the transaction
    /// commits without it, rather than waiting for it and committing its row.
    #[test]
    fn a_call_made_after_its_callback_settled_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let transaction = open(&dir);
        isolate!(let scope, let state);
        let notes = notes_in(scope, &transaction, &transaction.callback);
        // The callback resolved, and nothing it started was outstanding.
        let commit = crate::transaction::exec_settle(&transaction.binding.route(), true, None);
        // A continuation of the callback then makes a call before the settle
        // has run at all.
        let entered = crate::tx_scope::enter(scope, &transaction.callback);
        insert(scope, notes, "late");
        crate::tx_scope::leave(scope, entered);
        let mut operations = queued(&state);
        let late = operations.pop().unwrap();

        let (settled, late) = transaction.settle_beside(commit, late);
        assert!(
            matches!(settled, crate::transaction::SettleOutcome::Ok),
            "the transaction commits its own work: {settled:?}"
        );
        let (code, hint) = late.expect_err("a call made after the callback settled is refused");
        assert_eq!(code, "transaction_scope_expired");
        assert!(hint.is_some_and(|hint| hint.contains("Await")));
        assert_eq!(
            transaction.titles(&dir),
            Vec::<String>::new(),
            "the late call's row must never be committed"
        );
        crate::tests::fixtures::reset_context();
    }

    /// A callback rejects while a call it made has not yet run at all. The
    /// rollback waits for that call to run and finish inside the transaction,
    /// then rolls it back with everything else.
    ///
    /// Without the wait the rollback went first and the call then ran against
    /// a transaction that was already ending.
    #[test]
    fn a_rejected_callback_waits_for_its_call_then_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let transaction = open(&dir);
        isolate!(let scope, let state);
        let notes = notes_in(scope, &transaction, &transaction.callback);
        let entered = crate::tx_scope::enter(scope, &transaction.callback);
        insert(scope, notes, "a");
        crate::tx_scope::leave(scope, entered);
        let mut operations = queued(&state);
        let call = operations.pop().unwrap();

        let rollback = crate::transaction::exec_settle(&transaction.binding.route(), false, None);
        let (settled, call) = transaction.settle_beside(rollback, call);
        assert!(
            matches!(settled, crate::transaction::SettleOutcome::Ok),
            "a rejected callback rolls back: {settled:?}"
        );
        call.expect("the call ran to completion inside the transaction before the rollback");
        assert_eq!(transaction.titles(&dir), Vec::<String>::new());
        crate::tests::fixtures::reset_context();
    }

    /// A callback resolves while a call it started is still outstanding. The
    /// settle waits for the call, then rolls the transaction back and fails with
    /// `transaction_work_unfinished` rather than committing work the callback
    /// never waited for.
    #[test]
    fn a_callback_that_returns_with_a_call_outstanding_rolls_back_as_unfinished() {
        let dir = tempfile::tempdir().unwrap();
        let transaction = open(&dir);
        isolate!(let scope, let state);
        let notes = notes_in(scope, &transaction, &transaction.callback);
        let entered = crate::tx_scope::enter(scope, &transaction.callback);
        insert(scope, notes, "a");
        crate::tx_scope::leave(scope, entered);
        let mut operations = queued(&state);
        let call = operations.pop().unwrap();

        let commit = crate::transaction::exec_settle(&transaction.binding.route(), true, None);
        let (settled, call) = transaction.settle_beside(commit, call);
        assert_unfinished(settled, "a root callback that left a call running");
        call.expect("the outstanding call ran to completion before the rollback");
        assert_eq!(
            transaction.titles(&dir),
            Vec::<String>::new(),
            "the unfinished call's row must be rolled back, not committed"
        );
        crate::tests::fixtures::reset_context();
    }

    /// The same for a nested callback: its frame is rolled back and the nested
    /// transaction fails as unfinished, while the parent carries on and commits
    /// its own work.
    #[test]
    fn a_nested_callback_that_returns_with_a_call_outstanding_rolls_back_as_unfinished() {
        let dir = tempfile::tempdir().unwrap();
        let transaction = open(&dir);
        let (frame, child) = transaction.nested();
        isolate!(let scope, let state);
        let child_notes = notes_in(scope, &transaction, &child);
        let entered = crate::tx_scope::enter(scope, &child);
        insert(scope, child_notes, "child");
        crate::tx_scope::leave(scope, entered);
        let mut operations = queued(&state);
        let call = operations.pop().unwrap();

        let release =
            crate::transaction::exec_settle(&transaction.binding.route(), true, Some(frame));
        let (settled, call) = transaction.settle_beside(release, call);
        assert_unfinished(settled, "a nested callback that left a call running");
        call.expect("the outstanding child call ran to completion before the rollback");

        let root_notes = notes_in(scope, &transaction, &transaction.callback);
        let entered = crate::tx_scope::enter(scope, &transaction.callback);
        insert(scope, root_notes, "outer");
        crate::tx_scope::leave(scope, entered);
        let mut operations = queued(&state);
        transaction
            .settle(operations.pop().unwrap())
            .expect("the parent frame is usable once the nested one rolled back");
        assert_eq!(transaction.commit_and_read_titles(&dir), ["outer"]);
        crate::tests::fixtures::reset_context();
    }

    /// A parent callback that returns while a nested transaction it started is
    /// still outstanding: the nested transaction holds the parent's frame from
    /// the moment it is issued, so the parent's settle waits for it and then
    /// rolls back as unfinished instead of committing around it. The hold the
    /// nested transaction keeps after its SAVEPOINT is bound by the SDK-level
    /// `unawaited_calls_fail_their_transaction_as_unfinished`.
    #[test]
    fn a_callback_that_returns_with_a_nested_transaction_outstanding_rolls_back_as_unfinished() {
        let dir = tempfile::tempdir().unwrap();
        let transaction = open(&dir);
        isolate!(let scope, let state);
        let source = v8::String::new(scope, "(async () => {})").unwrap();
        let script = v8::Script::compile(scope, source, None).expect("compile the callback");
        let callback: v8::Local<v8::Function> = script
            .run(scope)
            .expect("evaluate the callback")
            .try_into()
            .expect("the callback is a function");
        let entered = crate::tx_scope::enter(scope, &transaction.callback);
        let nested = crate::v8_classes::transaction::transaction_dispatch(
            scope,
            callback,
            None,
            transaction.binding.clone(),
        );
        crate::tx_scope::leave(scope, entered);
        assert_eq!(nested.state(), v8::PromiseState::Pending);
        let mut operations = queued(&state);
        let begin = operations.pop().unwrap();

        let commit = crate::transaction::exec_settle(&transaction.binding.route(), true, None);
        let mut commit = Box::pin(commit);
        // The parent's settle is waiting on the nested transaction's claim.
        assert!(
            transaction
                .runtime
                .block_on(async { futures::poll!(commit.as_mut()) })
                .is_pending(),
            "the parent must not settle while its nested transaction holds the frame"
        );
        // Abandoning the nested transaction before its SAVEPOINT releases the
        // frame, and the parent settles as unfinished.
        drop(begin);
        assert_unfinished(
            transaction.runtime.block_on(commit),
            "a callback that left a nested transaction open",
        );
        assert_eq!(transaction.titles(&dir), Vec::<String>::new());
        crate::tests::fixtures::reset_context();
    }
}

/// The same overlap written the way a creator writes it, through the SDK: the
/// error the callback's `Promise.all` rejects with carries the documented
/// canonical code and the hint that names the remedy, and the transaction
/// rolls back.
#[test]
fn overlapping_transaction_operations_reach_the_creator_with_their_code_and_hint() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        notes_table(&dir);
        let source = sqlite_runtime_source(
            "notes",
            &value!({"title":{"type":"string", "required":true}}),
            r#"
const _procedures = {
  async overlap() {
    const result = await env.db.transaction(async tx => {
      const notes = tx.collection(COLLECTION);
      await Promise.all([notes.insert({ title: "first" }), notes.insert({ title: "second" })]);
    });
    const rows = await env.db.collection(COLLECTION).find({});
    return {
      code: result.error?.code ?? null,
      hinted: typeof result.error?.hint === "string" && result.error.hint.includes("Await"),
      rows: rows.length,
    };
  },
};
"#,
        );
        assert_eq!(
            dispatch_sqlite_runtime(&dir, &source, "overlap"),
            value!({"json":{"code":"TRANSACTION_CONNECTION_BUSY", "hinted":true, "rows":0}})
        );
    });
}

/// Work a callback started and did not wait for fails its transaction, at the
/// root and in a nested callback, and a nested transaction left running is such
/// work too. Each unfinished frame is rolled back; the parent of a nested one
/// carries on.
#[test]
fn unawaited_calls_fail_their_transaction_as_unfinished() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        notes_table(&dir);
        let source = sqlite_runtime_source(
            "notes",
            &value!({"title":{"type":"string", "required":true}}),
            r#"
const _procedures = {
  async unfinished() {
    const root = await env.db.transaction(async tx => {
      tx.collection(COLLECTION).insert({ title: "root" });
    });
    let nested;
    const parent = await env.db.transaction(async tx => {
      nested = await env.db.transaction(async inner => {
        inner.collection(COLLECTION).insert({ title: "child" });
      });
      await tx.collection(COLLECTION).insert({ title: "parent" });
    });
    let child;
    const abandoning = await env.db.transaction(async () => {
      // Returns once the nested callback is running - its SAVEPOINT is open -
      // without waiting for the nested transaction to finish.
      let started;
      const running = new Promise(resolve => { started = resolve; });
      child = env.db.transaction(async inner => {
        started();
        await inner.collection(COLLECTION).insert({ title: "left open" });
      });
      await running;
    });
    const leftOpen = await child;
    const rows = await env.db.collection(COLLECTION).find({}, { orderBy: { title: 1 } });
    return {
      root: root.error?.code ?? null,
      hinted: typeof root.error?.hint === "string" && root.error.hint.includes("Await"),
      nested: nested.error?.code ?? null,
      parent: parent.error?.code ?? null,
      abandoning: abandoning.error?.code ?? null,
      leftOpen: leftOpen.error?.code ?? null,
      titles: rows.map(row => row.title),
    };
  },
};
"#,
        );
        assert_eq!(
            dispatch_sqlite_runtime(&dir, &source, "unfinished"),
            value!({"json":{
                "root":"transaction_work_unfinished",
                "hinted":true,
                "nested":"transaction_work_unfinished",
                "parent":null,
                "abandoning":"transaction_work_unfinished",
                "leftOpen":null,
                "titles":["parent"],
            }})
        );
    });
}

/// A call made after the callback returned, but before the settle first ran,
/// belongs to a transaction that is ending: it is refused as expired rather
/// than waited for and committed. A call made before the callback settled is
/// unfinished work instead, and rolls the transaction back.
///
/// The late call is scheduled behind `depth` chained reactions, so which side
/// of the settlement it lands on is the chain's length, not timing.
#[test]
fn a_call_made_after_the_callback_returned_is_refused_not_committed() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        notes_table(&dir);
        let source = sqlite_runtime_source(
            "notes",
            &value!({"title":{"type":"string", "required":true}}),
            r#"
const _procedures = {
  async late() {
    const outcomes = {};
    for (const depth of [1, 40]) {
      let late;
      const tx = await env.db.transaction(async t => {
        await t.collection(COLLECTION).insert({ title: "own-" + depth });
        let chain = Promise.resolve();
        for (let i = 0; i < depth; i++) chain = chain.then(() => {});
        late = chain.then(() => env.db.collection(COLLECTION).insert({ title: "late-" + depth }));
      });
      // The root handle is the native collection, which throws on refusal.
      const refused = await late.then(() => null, error => error.code ?? "uncoded");
      outcomes[depth] = { tx: tx.error?.code ?? null, late: refused };
    }
    const rows = await env.db.collection(COLLECTION).find({}, { orderBy: { title: 1 } });
    return { outcomes, titles: rows.map(row => row.title) };
  },
};
"#,
        );
        assert_eq!(
            dispatch_sqlite_runtime(&dir, &source, "late"),
            value!({"json":{
                "outcomes":{
                    "1":{"tx":"transaction_work_unfinished", "late":null},
                    "40":{"tx":null, "late":"transaction_scope_expired"},
                },
                "titles":["own-40"],
            }})
        );
    });
}

/// Every call a callback makes is issued when it is made - a nested
/// `db.transaction()` and a root-handle `get(id)` included - so of the calls
/// one turn makes, the first runs and each later one is refused.
#[test]
fn every_call_a_callback_makes_is_issued_when_it_is_made() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        notes_table(&dir);
        let source = sqlite_runtime_source(
            "notes",
            &value!({"title":{"type":"string", "required":true}}),
            r#"
const _procedures = {
  async issued() {
    const seed = await env.db[COLLECTION].insert({ title: "seed" });
    const nested = title => env.db.transaction(async inner => {
      await inner.collection(COLLECTION).insert({ title });
    });
    const outcome = settled => settled.status === "rejected"
      ? settled.reason.code
      : settled.value.error?.code ?? "ok";
    const outcomes = {};
    const result = await env.db.transaction(async tx => {
      const notes = tx.collection(COLLECTION);
      const turn = async calls => (await Promise.allSettled(calls)).map(outcome);
      const get = () => env.db[COLLECTION].get(seed.data.id);
      outcomes.nestedFirst = await turn([nested("nested 1"), notes.insert({ title: "sibling 1" })]);
      outcomes.siblingFirst = await turn([notes.insert({ title: "sibling 2" }), nested("nested 2")]);
      outcomes.getFirst = await turn([get(), notes.insert({ title: "sibling 3" })]);
      outcomes.getThenNested = await turn([get(), nested("nested 3"), notes.insert({ title: "sibling 4" })]);
    });
    const rows = await env.db.collection(COLLECTION).find({}, { orderBy: { title: 1 } });
    return { error: result.error?.code ?? null, outcomes, titles: rows.map(row => row.title) };
  },
};
"#,
        );
        assert_eq!(
            dispatch_sqlite_runtime(&dir, &source, "issued"),
            value!({"json":{
                "error":null,
                "outcomes":{
                    "nestedFirst":["ok", "TRANSACTION_CONNECTION_BUSY"],
                    "siblingFirst":["ok", "transaction_connection_busy"],
                    "getFirst":["ok", "TRANSACTION_CONNECTION_BUSY"],
                    "getThenNested":["ok", "transaction_connection_busy", "TRANSACTION_CONNECTION_BUSY"],
                },
                "titles":["nested 1", "seed", "sibling 2"],
            }})
        );
    });
}
