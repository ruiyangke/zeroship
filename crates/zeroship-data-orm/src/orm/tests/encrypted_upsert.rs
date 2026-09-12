use super::fixtures::CollectionFixture;
use super::*;

fn row(output: Output) -> Value {
    let Output::Rows { mut rows, .. } = output else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 1);
    rows.remove(0)
}

async fn concurrent_upsert(generated: bool, masked: bool, nested: bool) {
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
    if generated {
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
        .insert(value!({"label":"control","secret":"control plaintext"}))
        .await
        .unwrap();
    let control = row(other
        .collection("records")
        .unwrap()
        .find(value!({"label":"control"}), options.clone())
        .await
        .unwrap());
    assert_eq!(control["secret"], value!("control plaintext"));

    let (inserted_tx, inserted_rx) = futures::channel::oneshot::channel();
    let (commit_tx, commit_rx) = futures::channel::oneshot::channel();
    let first = db.transaction(|tx| async move {
        let row = tx
            .collection("records")?
            .insert(value!({"label":"conflict","secret":"first"}))
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
                    document: value!({"label":"conflict","secret":"second"}),
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
    if !generated {
        assert_eq!(stored["version"], value!(2));
        assert_eq!(stored["created_at"], first["created_at"]);
    }
}

#[compio::test]
async fn postgres_masked_upsert_preserves_winning_identity() {
    concurrent_upsert(false, true, false).await;
}
#[compio::test]
async fn postgres_encrypted_upsert_preserves_winning_identity() {
    concurrent_upsert(false, false, false).await;
}
#[compio::test]
async fn postgres_generated_upsert_preserves_winning_identity() {
    concurrent_upsert(true, true, false).await;
}
#[compio::test]
async fn postgres_nested_upsert_preserves_winning_identity() {
    concurrent_upsert(true, false, true).await;
}
