use zeroship_data_orm::orm::*;
schema!(pub schema = "../fixtures/schema.runtime.json");

fn invalid(db: &Database) {
    let p = db.entity::<schema::posts::Entity>().unwrap().alias("p").unwrap();
    p.column(schema::posts::title).select::<i64>();
}

fn main() {}
