use zeroship_data_orm::orm::*;
schema!(pub schema = "../fixtures/schema.runtime.json");
fn main() {
    let _ = schema::posts::payload.lt(Some(vec![1_u8]));
}
