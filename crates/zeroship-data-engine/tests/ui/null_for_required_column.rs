use zeroship_data_engine::orm::*;
schema!(pub schema = "../fixtures/schema.runtime.json");
fn main() { let _ = schema::posts::title.set(None::<String>); }
