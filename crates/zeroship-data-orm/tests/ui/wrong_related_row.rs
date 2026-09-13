use zeroship_data_orm::orm::*;
schema!(pub schema = "../fixtures/typed-relations.runtime.json");
#[derive(FromRow)]
#[orm(entity = schema::posts)]
struct Post {
    title: String,
}

fn invalid(db: &Database) {
    db.entity::<schema::posts::Entity>()
        .unwrap()
        .query()
        .with_related(schema::posts::relations::author)
        .all::<Post, Post>();
}
fn main() {}
