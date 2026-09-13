use zeroship_data_orm::orm::*;
include!("../fixtures/posts_schema.rs");
posts_schema!(pub schema);
posts_schema!(pub other);
fn main() { let _ = schema::posts::title.eq(other::posts::title); }
