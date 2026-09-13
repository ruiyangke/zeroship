use zeroship_data_orm::orm::*;
schema!(pub schema = "../fixtures/typed-relations.runtime.json");
fn invalid(db: &Database) {
    db.entity::<schema::posts::Entity>()
        .unwrap()
        .query()
        .with_related(schema::posts::title);
}
fn main() {}
