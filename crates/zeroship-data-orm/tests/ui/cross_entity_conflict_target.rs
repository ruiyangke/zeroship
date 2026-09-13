use zeroship_data_orm::orm::*;
schema!(pub first = "../fixtures/schema.runtime.json");
schema!(pub second = "../fixtures/schema.runtime.json");
fn main() {
    let _ = ConflictTarget::new(first::posts::title).and(second::posts::title);
}
