use zeroship_data_engine::orm::*;
schema!(pub schema = "../fixtures/schema.runtime.json");
#[derive(FromRow)]
#[orm(entity = schema::posts)]
struct Post { nickname: String }
fn main() {}
