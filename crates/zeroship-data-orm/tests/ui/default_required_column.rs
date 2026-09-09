use zeroship_data_orm::orm::*;
schema!(pub schema = "../fixtures/schema.runtime.json");
#[derive(Insertable)]
#[orm(entity = schema::posts)]
struct Post {
    #[orm(default)]
    title: Defaulted<String>,
}
fn main() {}
