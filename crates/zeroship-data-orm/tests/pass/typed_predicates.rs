use zeroship_data_orm::orm::*;
schema!(pub schema = "../fixtures/typed-predicates.runtime.json");
use schema::predicate_rows as rows;
fn predicates(source: &EntityAlias<rows::Entity>) -> Result<(), DbError> {
    let _ = rows::rank.gte(1_i64)?.and(rows::rank.lte(3_i64)?).negate();
    let _ = !rows::optional.in_values([Some("one"), None])?;
    let _ = rows::document.ne(Value::Null)?;
    let _ = rows::payload.is_not_null();
    let _ = source.column(rows::rank).gte(1_i64)?;
    let _ = source.column(rows::optional).not_in_values([None::<&str>])?;
    let _ = source.column(rows::document).eq(Value::Null)?;
    let _ = source.column(rows::payload).is_null();
    Ok(())
}
fn main() {}
