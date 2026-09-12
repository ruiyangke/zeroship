use super::fixtures::CollectionFixture;
use super::*;

fn fields() -> Value {
    value!({
        "slug":{"type":"string","required":true,"unique":true},
        "nickname":{"type":"string","default":"anonymous"}
    })
}

#[compio::test]
async fn sqlite_insert_many_applies_defaults_across_row_shapes() {
    let fixture = CollectionFixture::sqlite("entries", fields()).await;
    let Output::Rows { rows, .. } = fixture
        .database
        .collection("entries")
        .unwrap()
        .execute(Operation::InsertMany {
            documents: value!([
                {"slug":"with-null","nickname":null},
                {"slug":"with-default"}
            ]),
        })
        .await
        .unwrap()
    else {
        panic!("insertMany must return rows")
    };

    assert_eq!(rows[0]["nickname"], Value::Null);
    assert_eq!(rows[1]["nickname"], value!("anonymous"));
    fixture.close().await;
}

#[compio::test]
async fn sqlite_insert_many_rolls_back_all_row_shapes_on_failure() {
    let fixture = CollectionFixture::sqlite("entries", fields()).await;
    let collection = fixture.database.collection("entries").unwrap();
    collection
        .execute(Operation::InsertMany {
            documents: value!([
                {"slug":"duplicate","nickname":null},
                {"slug":"duplicate"}
            ]),
        })
        .await
        .unwrap_err();

    let Output::Rows { rows, .. } = collection.find(value!({}), value!({})).await.unwrap() else {
        panic!("find must return rows")
    };
    assert!(rows.is_empty());
    fixture.close().await;
}
