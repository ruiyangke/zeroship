use zeroship_data_orm::orm::*;
schema!(pub schema = "../fixtures/schema.runtime.json");
#[derive(Changeset)]
#[orm(entity = schema::posts)]
struct Post { id: Change<String> }
fn main() {}
