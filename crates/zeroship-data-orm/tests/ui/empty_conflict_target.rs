use zeroship_data_orm::orm::*;
schema!(pub schema = "../fixtures/schema.runtime.json");
fn main() {
    let _ = ConflictTarget::<schema::posts::Entity>::new();
}
