use zeroship_data_orm::orm::*;
include!("../fixtures/predicates_schema.rs");
predicates_schema!(pub schema);
fn invalid(source: &EntityAlias<schema::predicate_rows::Entity>) {
    let _ = source.column(schema::predicate_rows::document).gt(Value::Null);
}
fn main() {}
