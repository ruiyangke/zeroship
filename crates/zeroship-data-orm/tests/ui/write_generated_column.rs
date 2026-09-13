use zeroship_data_orm::orm::*;
include!("../fixtures/posts_schema.rs");
posts_schema!(pub schema);
#[derive(Changeset)]
#[orm(entity = schema::posts)]
struct Post { id: Change<String> }
fn main() {}
