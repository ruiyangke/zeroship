use zeroship_data_orm::orm::*;
include!("../fixtures/relations_schema.rs");
relations_schema!(pub schema);
fn invalid(db: &Database) {
    db.entity::<schema::posts::Entity>()
        .unwrap()
        .query()
        .with_related(schema::posts::title);
}
fn main() {}
