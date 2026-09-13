use zeroship_data_orm::orm::Insertable;

include!("../fixtures/manual_id_schema.rs");
manual_id_schema!(pub models);
use models::records;

#[derive(Insertable)]
#[orm(entity = records)]
struct Record {
    id: String,
    label: String,
}

fn main() {}
