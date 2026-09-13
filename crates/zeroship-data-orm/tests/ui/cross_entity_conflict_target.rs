use zeroship_data_orm::orm::*;
include!("../fixtures/posts_schema.rs");
posts_schema!(pub first);
posts_schema!(pub second);
fn main() {
    let _ = ConflictTarget::new(first::posts::title).and(second::posts::title);
}
