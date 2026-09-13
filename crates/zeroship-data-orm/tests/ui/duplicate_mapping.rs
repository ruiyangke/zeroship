use zeroship_data_orm::orm::*;
include!("../fixtures/posts_schema.rs");
posts_schema!(pub schema);
#[derive(FromRow)]
#[orm(entity = schema::posts)]
struct Post {
    title: String,
    #[orm(column = "title")]
    name: String,
}
fn main() {}
