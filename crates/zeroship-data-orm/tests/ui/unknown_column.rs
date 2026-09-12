use zeroship_data_orm::orm::*;
schema!(pub schema = "../fixtures/schema.runtime.json");
#[derive(FromRow)]
#[orm(entity = schema::posts)]
struct Post { typo: String }
fn main() {}
