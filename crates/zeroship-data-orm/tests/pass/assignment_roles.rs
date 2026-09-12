use zeroship_data_orm::orm::*;
schema!(pub models = "../fixtures/assignment-roles.runtime.json");
use models::entries;

#[derive(Insertable)]
#[orm(entity = entries)]
struct NewEntry {
    id: String,
    created_at: String,
    version: i64,
}

#[derive(Changeset)]
#[orm(entity = entries)]
struct EditEntry {
    created_at: Change<String>,
    version: Change<i64>,
}

fn main() {
    let record = NewEntry {
        id: "chosen".into(),
        created_at: "ordinary".into(),
        version: 9,
    }
    .into_record()
    .unwrap();
    assert_eq!(record["id"].as_str(), Some("chosen"));
    assert!(!record.contains_key("entry_key"));
    let patch = EditEntry {
        created_at: Change::Keep,
        version: Change::Set(10),
    }
    .into_changes()
    .unwrap();
    assert_eq!(patch["version"].as_i64(), Some(10));
    assert!(!patch.contains_key("id"));
    entries::version.set(10_i64).unwrap();
}
