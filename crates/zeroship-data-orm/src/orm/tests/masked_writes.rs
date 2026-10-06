//! A masked cell from a read result never reaches storage.
//!
//! The cell's serialized form is its display, so a write that serialized it
//! would replace the column's data with a mask. Every write refuses one, in any
//! position of a JSON value, before anything is stored.
use super::fixtures::CollectionFixture;
use super::*;

fn fields() -> Value {
    value!({
        "ssn": {"type": "string", "mask": {"kind": "last4", "classification": "spi"},
            "storage": {"valueColumn": "ssn", "rawColumn": "__zs_raw__ssn"}},
        "prefs": {"type": "json", "nullable": true}
    })
}

fn object(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

/// The JSON shapes a cell can take: on its own, in an array, inside an object.
/// Built by hand: `value!` serializes its operands, which turns a cell into its
/// display before a write could see it.
fn shapes(leaf: &Value) -> [(&'static str, Value); 3] {
    [
        ("scalar", leaf.clone()),
        ("array", Value::Array(vec![Value::from("kept"), leaf.clone()])),
        ("object", object([("nested", object([("leaf", leaf.clone())]))])),
    ]
}

fn assert_refused(result: Result<Output, DbError>, label: &str) {
    match result {
        Err(DbError::ValidationFailed {
            code: "masked_cell_refused",
            ..
        }) => {}
        other => panic!("{label}: a masked cell must be refused, got {other:?}"),
    }
}

async fn masked_cells_never_reach_a_json_column(postgres: bool) {
    let owner = if postgres {
        CollectionFixture::postgres("people", fields()).await
    } else {
        CollectionFixture::sqlite("people", fields()).await
    };
    let people = owner.database.collection("people").unwrap();
    let Output::Rows(rows) = people
        .insert(value!({"ssn": "123-45-6789", "prefs": {"seeded": true}}))
        .await
        .unwrap()
    else {
        panic!("insert returns rows")
    };
    let id = rows[0]["id"].clone();
    let cell = rows[0]["ssn"].clone();
    assert!(cell.as_masked().is_some(), "a read returns a masked cell: {cell:?}");

    for (shape, value) in shapes(&cell) {
        assert_refused(
            people.insert(object([("prefs", value.clone())])).await,
            &format!("insert {shape}"),
        );
        assert_refused(
            people
                .update(
                    value!({"id": id.clone()}),
                    object([("$set", object([("prefs", value)]))]),
                )
                .await,
            &format!("update {shape}"),
        );
    }
    let Output::Rows(rows) = people.find(value!({}), value!({})).await.unwrap() else {
        panic!("find returns rows")
    };
    assert_eq!(rows.len(), 1, "no refused insert may store a row: {rows:?}");
    assert_eq!(
        rows[0]["prefs"],
        value!({"seeded": true}),
        "no refused update may change the stored value"
    );

    // Control: the same shapes holding the display as plain text are written
    // and read back, so the refusals above are about the cell and nothing else.
    let display = Value::from(cell.as_masked().unwrap().display());
    for (shape, value) in shapes(&display) {
        people
            .update(
                value!({"id": id.clone()}),
                object([("$set", object([("prefs", value.clone())]))]),
            )
            .await
            .unwrap_or_else(|error| panic!("{shape}: plain data must be written: {error}"));
        let Output::Rows(rows) = people
            .find(value!({"id": id.clone()}), value!({}))
            .await
            .unwrap()
        else {
            panic!("find returns rows")
        };
        assert_eq!(rows[0]["prefs"], value, "{shape}");
    }
    owner.close().await;
}

#[compio::test]
async fn sqlite_json_writes_refuse_masked_cells() {
    masked_cells_never_reach_a_json_column(false).await;
}

#[compio::test]
async fn postgres_json_writes_refuse_masked_cells() {
    masked_cells_never_reach_a_json_column(true).await;
}
