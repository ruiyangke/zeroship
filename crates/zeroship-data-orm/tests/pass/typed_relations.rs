use zeroship_data_orm::orm::*;
schema!(pub schema = "../fixtures/typed-relations.runtime.json");
use schema::{authors, posts};

fn target<C: Relation<Source = posts::Entity, Target = authors::Entity>>(_: C) -> &'static str {
    C::TARGET_COLUMN
}

#[derive(FromRow)]
#[orm(entity = posts)]
struct Post {
    title: String,
}

#[derive(FromRow)]
#[orm(entity = authors)]
struct Author {
    name: String,
}

async fn read(db: &Database) -> Result<(), DbError> {
    let collection = db.entity::<posts::Entity>()?;
    let _: Vec<(Post, Option<Author>)> = collection
        .query()
        .filter(posts::title.eq("hello")?)
        .with_related(posts::relations::author)
        .all()
        .await?;
    let _: Option<(Post, Option<Author>)> = collection
        .query()
        .with_related(posts::relations::authorByHandle)
        .first()
        .await?;
    Ok(())
}

fn main() {
    assert_eq!(target(posts::relations::author), "id");
    assert_eq!(target(posts::relations::authorByHandle), "handle");
    assert_eq!(target(posts::relations::authorBySerial), "serial");
    let _ = read;
}
