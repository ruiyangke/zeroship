use zeroship_data_orm::orm::{Change, Changeset};

include!("../fixtures/manual_id_schema.rs");
manual_id_schema!(pub models);
use models::records;

#[derive(Changeset)]
#[orm(entity = records)]
struct ChangeId {
    id: Change<String>,
}

fn main() {}
