use zeroship_data_orm::orm::{schema, Insertable};

schema!(pub models = "../fixtures/manual-id.runtime.json");
use models::records;

#[derive(Insertable)]
#[orm(entity = records)]
struct Record {
    id: String,
    label: String,
}

fn main() {}
