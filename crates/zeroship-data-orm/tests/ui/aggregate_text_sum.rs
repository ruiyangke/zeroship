use zeroship_data_orm::orm::*;
include!("../fixtures/posts_schema.rs");
posts_schema!(pub schema);

fn invalid(db: &Database) {
    let p = db.entity::<schema::posts::Entity>().unwrap().alias("p").unwrap();
    p.column(schema::posts::title).sum();
}

fn main() {}
