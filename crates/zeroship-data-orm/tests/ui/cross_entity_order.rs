use zeroship_data_orm::orm::*;
schema!(pub schema = "../fixtures/schema.runtime.json");
schema!(pub other = "../fixtures/typed-reads.runtime.json");

fn invalid(db: &Database) {
    db.entity::<schema::posts::Entity>().unwrap().query()
        .order_by(other::readings::title.asc());
}

fn main() {}
