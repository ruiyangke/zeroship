use zeroship_data_orm::orm::*;
include!("../fixtures/posts_schema.rs");
posts_schema!(pub schema);
fn invalid(source: &EntityAlias<schema::posts::Entity>) {
    let _ = count_rows().eq(source.column(schema::posts::title));
}
fn main() {}
