use zeroship_data_orm::orm::*;
schema!(pub schema = "../fixtures/typed-relations.runtime.json");
fn invalid(db: &Database) {
    db.entity::<schema::authors::Entity>()
        .unwrap()
        .query()
        .with_related(schema::posts::relations::author);
}
fn main() {}
