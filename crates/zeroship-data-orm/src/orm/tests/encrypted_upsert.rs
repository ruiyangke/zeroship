use super::fixtures::CollectionFixture;
use super::*;

fn row(output: Output) -> Value {
    let Output::Rows { mut rows, .. } = output else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 1);
    rows.remove(0)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IdentityMode {
    Typed,
    Database,
    Supplied,
}

async fn concurrent_upsert(identity: IdentityMode, masked: bool, nested: bool) {
    let keys = std::sync::Arc::new(crate::encryption::SuppliedProjectKeys::new());
    keys.insert_hex("upsert_identity", &"3".repeat(64)).unwrap();
    let mut fields = value!({"label":{"type":"string","unique":true}, "secret":{"type":"string","encrypted":true}});
    if masked {
        fields["secret"]["mask"] = value!({"kind":"full","classification":"pii"});
        fields["secret"]["storage"] =
            value!({"valueColumn":"secret","rawColumn":"__zs_raw__secret"});
    }
    let mut owner = CollectionFixture::postgres_with_keys(
        "records",
        fields,
        ProjectKeySource::supplied(keys.clone()),
    )
    .await;
    keys.bind_app(owner.database.binding.app_id(), "upsert_identity")
        .unwrap();
    if identity == IdentityMode::Database {
        let mut migration: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/identity-migration.json"
        ))
        .unwrap();
        if masked {
            migration["ops"][0]["columns"][2]["mask"]["kind"] = serde_json::json!("full");
        }
        owner
            .replace_from_migration("records", &migration.to_string())
            .await;
    }
    if identity == IdentityMode::Supplied {
        let current = &owner.database;
        let mut fields = current.context.with(|| {
            crate::descriptor::collection_schema(&current.binding, "records")
                .unwrap()
                .as_ref()
                .clone()
        });
        fields["id"].as_object_mut().unwrap().shift_remove("assign");
        fields["id"]["writable"] = Value::Bool(true);
        owner.database = Database::from_schema(
            current.binding.clone(),
            current.backend.clone(),
            vec![("records".into(), fields)],
        )
        .unwrap();
    }
    let db = owner.database.clone();
    let fields = db.context.with(|| {
        crate::descriptor::collection_schema(&db.binding, "records")
            .unwrap()
            .as_ref()
            .clone()
    });
    let other = Database::from_schema(
        db.binding.clone(),
        db.backend.clone(),
        vec![("records".into(), fields)],
    )
    .unwrap();
    other
        .install_mask_policy(value!({"support":["pii"]}))
        .unwrap();
    let options = if masked {
        value!({"unmask":["secret"],"actor":{"kind":"support","id":"usr_reader"},"unmaskReason":"verify stored value"})
    } else {
        value!({})
    };
    other
        .collection("records")
        .unwrap()
        .insert(if identity == IdentityMode::Supplied {
            value!({"id":"control","label":"control","secret":"control plaintext"})
        } else {
            value!({"label":"control","secret":"control plaintext"})
        })
        .await
        .unwrap();
    let control = row(other
        .collection("records")
        .unwrap()
        .find(value!({"label":"control"}), options.clone())
        .await
        .unwrap());
    assert_eq!(control["secret"], value!("control plaintext"));
    let subscription = crate::cdc::broker::subscribe(db.binding.app_id(), "records");

    let (inserted_tx, inserted_rx) = futures::channel::oneshot::channel();
    let (commit_tx, commit_rx) = futures::channel::oneshot::channel();
    let first = db.transaction(|tx| async move {
        let row = tx
            .collection("records")?
            .insert(if identity == IdentityMode::Supplied {
                value!({"id":"winner","label":"conflict","secret":"first"})
            } else {
                value!({"label":"conflict","secret":"first"})
            })
            .await?;
        inserted_tx.send(()).unwrap();
        commit_rx.await.unwrap();
        Ok(row)
    });
    let second = async {
        inserted_rx.await.unwrap();
        let upsert = |db: Database| async move {
            db.collection("records")?
                .execute(Operation::Upsert {
                    document: if identity == IdentityMode::Supplied {
                        value!({"id":"loser","label":"conflict","secret":"second"})
                    } else {
                        value!({"label":"conflict","secret":"second"})
                    },
                    conflict_fields: value!(["label"]),
                })
                .await
        };
        if nested {
            other.transaction(upsert).await
        } else {
            upsert(other.clone()).await
        }
    };
    let release = async {
        owner.wait_for_upsert_conflict().await;
        commit_tx.send(()).unwrap();
    };
    let (first, second, ()) = futures::join!(first, second, release);
    let read = other
        .collection("records")
        .unwrap()
        .find(value!({"label":"conflict"}), options)
        .await;
    owner.close().await;
    let first = row(first.unwrap());
    let second = row(second.expect("upsert must succeed after the conflicting insert commits"));
    let stored = row(read.expect("committed ciphertext must authenticate"));
    assert_eq!(stored["secret"], value!("second"));
    assert_eq!(first["id"], second["id"]);
    assert_eq!(stored["id"], first["id"]);
    let mut changes = Vec::new();
    while let Some(message) = subscription.pop() {
        if let crate::cdc::broker::SubscriptionMessage::Change(change) = message {
            changes.push(change.op);
        }
    }
    assert_eq!(changes, vec![ChangeOp::Insert, ChangeOp::Update]);
    subscription.close();
    if identity != IdentityMode::Database {
        assert_eq!(stored["version"], value!(2));
        assert_eq!(stored["created_at"], first["created_at"]);
    }
}

#[compio::test]
async fn postgres_masked_upsert_preserves_winning_identity() {
    concurrent_upsert(IdentityMode::Typed, true, false).await;
}
#[compio::test]
async fn postgres_encrypted_upsert_preserves_winning_identity() {
    concurrent_upsert(IdentityMode::Typed, false, false).await;
}
#[compio::test]
async fn postgres_generated_upsert_preserves_winning_identity() {
    concurrent_upsert(IdentityMode::Database, true, false).await;
}
#[compio::test]
async fn postgres_nested_upsert_preserves_winning_identity() {
    concurrent_upsert(IdentityMode::Database, false, true).await;
}
#[compio::test]
async fn postgres_supplied_upsert_preserves_winning_identity() {
    concurrent_upsert(IdentityMode::Supplied, false, false).await;
}

#[compio::test]
async fn cancelling_a_waiting_protected_upsert_rolls_back_its_internal_frame() {
    let keys = std::sync::Arc::new(crate::encryption::SuppliedProjectKeys::new());
    keys.insert_hex("cancelled_upsert", &"6".repeat(64))
        .unwrap();
    let owner = CollectionFixture::postgres_with_keys(
        "records",
        value!({
            "label":{"type":"string","unique":true},
            "secret":{"type":"string","encrypted":true}
        }),
        ProjectKeySource::supplied(keys.clone()),
    )
    .await;
    keys.bind_app(owner.database.binding.app_id(), "cancelled_upsert")
        .unwrap();
    let db = owner.database.clone();
    let fields = db.context.with(|| {
        crate::descriptor::collection_schema(&db.binding, "records")
            .unwrap()
            .as_ref()
            .clone()
    });
    let other = Database::from_schema(
        db.binding.clone(),
        db.backend.clone(),
        vec![("records".into(), fields)],
    )
    .unwrap();
    let subscription = crate::cdc::broker::subscribe(db.binding.app_id(), "records");
    let (upsert_ready_tx, upsert_ready_rx) = futures::channel::oneshot::channel();
    let (observer_ready_tx, observer_ready_rx) = futures::channel::oneshot::channel();
    let (commit_tx, commit_rx) = futures::channel::oneshot::channel();
    let first = db.transaction(|tx| async move {
        let output = tx
            .collection("records")?
            .insert(value!({"label":"same","secret":"winner"}))
            .await?;
        upsert_ready_tx.send(()).unwrap();
        observer_ready_tx.send(()).unwrap();
        commit_rx.await.unwrap();
        Ok(output)
    });
    let (abort, registration) = futures::future::AbortHandle::new_pair();
    let waiting = futures::future::Abortable::new(
        async {
            upsert_ready_rx.await.unwrap();
            other
                .collection("records")
                .unwrap()
                .execute(Operation::Upsert {
                    document: value!({"label":"same","secret":"cancelled"}),
                    conflict_fields: value!(["label"]),
                })
                .await
        },
        registration,
    );
    let cancel = async {
        observer_ready_rx.await.unwrap();
        owner.wait_for_upsert_conflict().await;
        abort.abort();
        commit_tx.send(()).unwrap();
    };
    let (first, waiting, ()) = futures::join!(first, waiting, cancel);
    first.unwrap();
    assert!(waiting.is_err());

    let row = row(other
        .collection("records")
        .unwrap()
        .execute(Operation::Upsert {
            document: value!({"label":"same","secret":"committed"}),
            conflict_fields: value!(["label"]),
        })
        .await
        .unwrap());
    assert_eq!(row["secret"], value!("committed"));
    let mut changes = Vec::new();
    while let Some(message) = subscription.pop() {
        if let crate::cdc::broker::SubscriptionMessage::Change(change) = message {
            changes.push(change.op);
        }
    }
    assert_eq!(changes, vec![ChangeOp::Insert, ChangeOp::Update]);
    subscription.close();
    owner.close().await;
}
