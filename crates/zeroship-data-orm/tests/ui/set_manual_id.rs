include!("../fixtures/manual_id_schema.rs");
manual_id_schema!(pub models);

fn main() {
    let _ = models::records::id.set("replacement");
}
