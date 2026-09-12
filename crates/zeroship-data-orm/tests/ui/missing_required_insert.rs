use zeroship_data_orm::orm::*;
schema!(pub schema = "../fixtures/schema.runtime.json");
#[derive(Insertable)]
#[orm(entity = schema::posts)]
struct Post { payload: Option<Vec<u8>> }
fn main() {}
