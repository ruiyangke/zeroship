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
