use zeroship_data_orm::orm::{schema, Change, Changeset};

schema!(pub models = "../fixtures/manual-id.runtime.json");
use models::records;

#[derive(Changeset)]
#[orm(entity = records)]
struct ChangeId {
    id: Change<String>,
}

fn main() {}
