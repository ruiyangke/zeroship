use super::fixtures::CollectionFixture;
use super::*;

fn rows(output: Output) -> Vec<Value> {
    let Output::Rows { rows, .. } = output else {
        panic!("expected rows")
    };
    assert!(!rows.is_empty());
    for row in &rows {
        assert!(row.get("id").is_none(), "internal identity leaked: {row:?}");
    }
    rows
}

async fn hidden_id(postgres: bool) {
    let keys = std::sync::Arc::new(crate::encryption::SuppliedProjectKeys::new());
    keys.insert_hex("hidden_identity", &"3".repeat(64)).unwrap();
    let fields = value!({
        "label":{"type":"string","unique":true},
        "secret":{"type":"string","encrypted":true},
        "masked":{"type":"string","mask":{"kind":"full","classification":"pii"},
            "storage":{"valueColumn":"masked","rawColumn":"__zs_raw__masked"}}
    });
    let source = ProjectKeySource::supplied(keys.clone());
    let owner = if postgres {
        CollectionFixture::postgres_with_keys("records", fields, source).await
    } else {
        CollectionFixture::sqlite_with_keys("records", fields, source).await
    };
    keys.bind_app(owner.database.binding.app_id(), "hidden_identity")
        .unwrap();
    for flag in ["readable", "projectable"] {
        let mut fields = owner.database.context.with(|| {
            crate::descriptor::collection_schema(&owner.database.binding, "records")
                .unwrap()
                .as_ref()
                .clone()
        });
        fields["id"][flag] = value!(false);
        let db = Database::from_schema(
            owner.database.binding.clone(),
            owner.database.backend.clone(),
            vec![("records".into(), fields)],
        )
        .unwrap();
        db.install_mask_policy(value!({"support":["pii"]})).unwrap();
        let records = db.collection("records").unwrap();
        let inserted = rows(
            records
                .insert(value!({"label":flag,"secret":"private","masked":"raw"}))
                .await
                .unwrap(),
        );
        assert_eq!(inserted[0]["secret"], value!("private"));
        assert!(
            !inserted[0]["masked"]["_meta"]["row_pk"]
                .as_str()
                .unwrap()
                .is_empty()
        );
        let source = ReadSource::new("records", "r");
        let mut read = ReadQuery::new(source.clone());
        read.filter = zeroship_data_sql::Predicate::compare(
            zeroship_data_sql::Operand::Path(source.column("label").unwrap()),
            zeroship_data_sql::CompareOp::Eq,
            zeroship_data_sql::Operand::Lit(zeroship_data_sql::Literal::Text(flag.into())),
        );
        for fields in [None, Some(vec!["secret".into()])] {
            read.projection = vec![ReadProjection::Row {
                output: "record".into(),
                source: "r".into(),
                fields,
                optional: false,
            }];
            let projected = rows(db.read(read.clone()).await.unwrap());
            assert!(projected[0]["record"].get("id").is_none());
            assert_eq!(projected[0]["record"]["secret"], value!("private"));
        }
        let batch = rows(
            records
                .execute(Operation::InsertMany {
                    documents: value!([
                        {"label":format!("{flag}_batch"),"secret":"batch"}
                    ]),
                })
                .await
                .unwrap(),
        );
        assert_eq!(batch[0]["secret"], value!("batch"));
        let projection = rows(
            records
                .find(value!({"label":flag}), value!({"select":["secret"]}))
                .await
                .unwrap(),
        );
        assert_eq!(projection, vec![value!({"secret":"private"})]);
        let unmasked = rows(records.find(value!({"label":flag}), value!({
            "select":["masked"],"unmask":["masked"],"actor":{"kind":"support","id":"usr_reader"},"unmaskReason":"verify projection"
        })).await.unwrap());
        assert_eq!(unmasked, vec![value!({"masked":"raw"})]);
        assert!(
            records
                .find(value!({}), value!({"select":["id"]}))
                .await
                .is_err()
        );
        let updated = rows(
            records
                .execute(Operation::Update {
                    filter: value!({"label":flag}),
                    patch: value!({"secret":"updated"}),
                    many: false,
                })
                .await
                .unwrap(),
        );
        assert_eq!(updated[0]["secret"], value!("updated"));
        let upserted = rows(
            records
                .execute(Operation::Upsert {
                    document: value!({"label":flag,"secret":"upserted"}),
                    conflict_fields: value!(["label"]),
                })
                .await
                .unwrap(),
        );
        assert_eq!(upserted[0]["secret"], value!("upserted"));
        let stored = rows(
            records
                .find(value!({"label":flag}), value!({}))
                .await
                .unwrap(),
        );
        assert_eq!(stored[0]["secret"], value!("upserted"));
        let purged = rows(
            records
                .execute(Operation::Purge {
                    filter: value!({"label":flag}),
                    many: false,
                })
                .await
                .unwrap(),
        );
        assert_eq!(purged[0]["secret"], value!("upserted"));
    }
    owner.close().await;
}

#[compio::test]
async fn sqlite_hidden_identity_stays_internal() {
    hidden_id(false).await;
}
#[compio::test]
async fn postgres_hidden_identity_stays_internal() {
    hidden_id(true).await;
}
