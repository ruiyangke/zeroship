use zeroship_data_orm::orm::*;
include!("../fixtures/native_arrays_schema.rs");
native_arrays_schema!(pub schema);
fn main() { let _ = schema::grants::scopes.push(vec!["nested"]); }
