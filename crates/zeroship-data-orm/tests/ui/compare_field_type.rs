use zeroship_data_orm::orm::*;
include!("../fixtures/posts_schema.rs");
posts_schema!(pub schema);
fn main() { let _ = schema::posts::title.eq(schema::posts::counter); }
