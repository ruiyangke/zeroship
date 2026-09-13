use zeroship_data_orm::orm::*;
include!("../fixtures/posts_schema.rs");
posts_schema!(pub schema);
fn main() {
    let _ = schema::posts::payload.lt(Some(vec![1_u8]));
}
