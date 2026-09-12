use super::fixtures::CollectionFixture;
use super::*;

fn fields() -> Value {
    value!({
        "email":{"type":"string", "unique":true},
        "name":{"type":"string"}
    })
}

#[compio::test]
async fn postgres_orm_owns_created_row_identity() {
    let fixture = CollectionFixture::postgres("contacts", fields()).await;
    exercise_identity(&fixture.database).await;
    fixture.close().await;
}

#[compio::test]
async fn sqlite_orm_owns_created_row_identity() {
    let fixture = CollectionFixture::sqlite("contacts", fields()).await;
    exercise_identity(&fixture.database).await;
    fixture.close().await;
}

async fn exercise_identity(db: &Database) {
    let contacts = db.collection("contacts").unwrap();
    let supplied =
        value!({"id":"usr_caller_chosen", "email":"blocked@example.com", "name":"blocked"});
    for operation in [
        Operation::Insert {
            document: supplied.clone(),
        },
        Operation::InsertMany {
            documents: value!([
                {"email":"valid@example.com", "name":"valid"}, supplied.clone()
            ]),
        },
        Operation::Upsert {
            document: supplied,
            conflict_fields: value!(["email"]),
        },
    ] {
        let error = contacts.execute(operation).await.unwrap_err();
        assert!(
            matches!(error, DbError::ValidationFailed { code, .. } if code == "platform_assigned_field"),
            "{error}"
        );
        assert_eq!(
            count(contacts.count(value!({}), value!({})).await.unwrap()),
            0
        );
    }
    for conflict_fields in [
        value!(["id"]),
        value!(["created_at"]),
        value!(["created_by"]),
        value!(["updated_at"]),
        value!(["updated_by"]),
        value!(["version"]),
        value!(["deleted_at"]),
        value!([]),
        value!(["email", 1]),
        value!(["email", null]),
        value!(["email", "email"]),
        value!(["missing"]),
        value!(["name"]),
        value!("email"),
    ] {
        let error = contacts
            .execute(Operation::Upsert {
                document: value!({"email":"blocked@example.com"}),
                conflict_fields,
            })
            .await
            .unwrap_err();
        assert!(matches!(error, DbError::ValidationFailed { .. }), "{error}");
        assert_eq!(
            count(contacts.count(value!({}), value!({})).await.unwrap()),
            0
        );
    }

    let Output::Rows { rows, .. } = contacts
        .execute(Operation::Upsert {
            document: value!({"email":"alice@example.com", "name":"Alice"}),
            conflict_fields: value!(["email"]),
        })
        .await
        .unwrap()
    else {
        panic!("upsert must return rows")
    };
    let original = rows[0].clone();
    assert!(original["id"].as_str().unwrap().starts_with("cont_"));
    let original_id = original["id"].clone();
    let output = db.transaction(|tx| async move {
        let contacts = tx.collection("contacts")?;
        let error = contacts.execute(Operation::Upsert {
            document: value!({"id":original_id, "email":"alice@example.com", "name":"forged"}),
            conflict_fields: value!(["email"]),
        }).await.unwrap_err();
        assert!(matches!(error, DbError::ValidationFailed { code, .. } if code == "platform_assigned_field"), "{error}");
        contacts.execute(Operation::Upsert {
            document: value!({"email":"alice@example.com", "name":"Alicia"}),
            conflict_fields: value!(["email"]),
        }).await
    }).await.unwrap();
    let Output::Rows { rows, .. } = output else {
        panic!("upsert must return rows")
    };
    assert_eq!(rows[0]["id"], original["id"]);
    assert_eq!(rows[0]["created_at"], original["created_at"]);
    assert_eq!(rows[0]["name"], value!("Alicia"));
    assert_eq!(rows[0]["version"], value!(2));
    assert_eq!(
        count(contacts.count(value!({}), value!({})).await.unwrap()),
        1
    );

    contacts
        .update(
            value!({"id":original["id"].clone()}),
            value!({"name":"Alice"}),
        )
        .await
        .unwrap();
    let Output::Rows { rows, .. } = contacts.find(value!({}), value!({})).await.unwrap() else {
        panic!("find must return rows")
    };
    assert_eq!(rows[0]["id"], original["id"]);
    assert_eq!(rows[0]["name"], value!("Alice"));
}
