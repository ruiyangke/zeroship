use zeroship_data_orm::orm::schema;

schema!(pub models = "../fixtures/manual-id.runtime.json");

fn main() {
    let _ = models::records::id.set("replacement");
}
