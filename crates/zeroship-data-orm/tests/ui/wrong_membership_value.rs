use zeroship_data_orm::orm::*;
schema!(pub schema = "../fixtures/schema.runtime.json");
fn main() {
    let _ = schema::posts::title.in_values([1_i64]);
}
