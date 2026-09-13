use zeroship_data_orm::orm::*;
include!("../fixtures/relations_schema.rs");
relations_schema!(pub schema);
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
