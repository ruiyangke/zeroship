use zeroship_data_orm::orm::*;
include!("../fixtures/mutation_expressions_schema.rs");
mutation_expressions_schema!(pub schema);
fn main() { let _ = schema::counters::optional.increment(None::<i64>); }
