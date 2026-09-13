use zeroship_data_orm::orm::*;
include!("../fixtures/posts_schema.rs");
posts_schema!(pub schema);
#[derive(Insertable)]
#[orm(entity = schema::posts)]
struct Post { payload: Option<Vec<u8>> }
fn main() {}
