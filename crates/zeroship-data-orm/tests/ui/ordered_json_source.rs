use zeroship_data_orm::orm::*;
schema!(pub schema = "../fixtures/typed-predicates.runtime.json");
fn invalid(source: &EntityAlias<schema::predicate_rows::Entity>) {
    let _ = source.column(schema::predicate_rows::document).gt(Value::Null);
}
fn main() {}
