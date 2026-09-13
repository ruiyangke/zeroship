use zeroship_data_orm::orm::*;
include!("../fixtures/mutation_expressions_schema.rs");
mutation_expressions_schema!(pub schema);
mutation_expressions_schema!(pub other);
fn main() { let _ = schema::counters::quantity.increment(1_i64).unwrap().and(other::counters::quantity.set(1_i64).unwrap()); }
