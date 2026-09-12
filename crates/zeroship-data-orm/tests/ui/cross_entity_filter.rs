use zeroship_data_orm::orm::*;
schema!(pub first = "../fixtures/schema.runtime.json");
schema!(pub second = "../fixtures/schema.runtime.json");
fn main() {
    let filter = first::posts::title.eq("first").unwrap();
    let _ = filter.and(second::posts::title.eq("second").unwrap());
}
