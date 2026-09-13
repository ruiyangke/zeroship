use zeroship_data_orm::orm::*;
include!("../fixtures/posts_schema.rs");
posts_schema!(pub schema);
fn invalid(source: &EntityAlias<schema::posts::Entity>) {
    let _ = source.column(schema::posts::title).eq(source.column(schema::posts::counter));
}
fn main() {}
