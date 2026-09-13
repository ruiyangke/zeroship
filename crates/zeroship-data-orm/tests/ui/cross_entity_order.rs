use zeroship_data_orm::orm::*;
include!("../fixtures/posts_schema.rs");
posts_schema!(pub schema);
posts_schema!(pub other, readings);

fn invalid(db: &Database) {
    db.entity::<schema::posts::Entity>().unwrap().query()
        .order_by(other::readings::title.asc());
}

fn main() {}
