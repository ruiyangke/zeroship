use zeroship_data_orm::orm::*;
schema!(pub schema = "../fixtures/schema.runtime.json");
use schema::posts;

#[derive(FromRow)]
#[orm(entity = posts)]
struct Summary {
    #[orm(column = "title")]
    name: String,
}

#[derive(Insertable)]
#[orm(entity = posts)]
struct NewPost<'a, T> {
    title: &'a str,
    payload: Option<T>,
    #[orm(default)]
    counter: Defaulted<i64>,
}

type OptionalName = Change<Option<String>>;
#[derive(Changeset)]
#[orm(entity = posts)]
struct Edit {
    nickname: OptionalName,
}

fn main() {
    let bytes = vec![0, 255];
    let address = bytes.as_ptr();
    let record = NewPost {
        title: "owned bytes",
        payload: Some(bytes),
        counter: Defaulted::Default,
    }
    .into_record()
    .unwrap();
    assert_eq!(record["payload"].as_bytes().unwrap().as_ptr(), address);
    assert!(!record.contains_key("counter"));
    assert_eq!(Summary::COLUMNS, &["title"]);
    assert!(
        Edit {
            nickname: Change::Keep
        }
        .into_changes()
        .unwrap()
        .is_empty()
    );
    assert!(
        Edit {
            nickname: Change::Set(None)
        }
        .into_changes()
        .unwrap()["nickname"]
            .is_null()
    );
}
