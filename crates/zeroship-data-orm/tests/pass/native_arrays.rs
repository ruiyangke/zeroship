#![allow(dead_code)]

use zeroship_data_orm::{orm::*, Database};

include!("../fixtures/native_arrays_schema.rs");
native_arrays_schema!(pub schema);
use schema::grants;

#[derive(FromRow)]
#[orm(entity = grants)]
struct Grant {
    scopes: Vec<String>,
    amr: Option<Vec<String>>,
    tags: Vec<String>,
}

#[derive(Insertable)]
#[orm(entity = grants)]
struct NewGrant<'a> {
    id: &'a str,
    scopes: &'a [String],
    amr: Option<Vec<&'a str>>,
    tags: Vec<String>,
}

#[derive(Changeset)]
#[orm(entity = grants)]
struct Narrow<'a> {
    scopes: Change<&'a [String]>,
}

async fn queries(db: &Database, scopes: &[String]) -> Result<(), DbError> {
    let entity = db.entity::<grants::Entity>()?;
    let _: Vec<Grant> = entity
        .query()
        .filter(
            grants::scopes
                .eq(scopes)?
                .or(grants::scopes.in_values([vec!["a"], vec!["b", "c"]])?)
                .and(grants::amr.ne(Some(vec!["pwd".to_owned()]))?),
        )
        .all()
        .await?;
    let _: Option<Grant> = entity
        .update(
            grants::id.eq("g")?,
            grants::scopes
                .push("openid")?
                .and(grants::amr.add_to_set("otp")?)?
                .and(grants::tags.pull("old")?)?,
        )
        .await?;
    let _: Option<Grant> = entity
        .update(
            grants::id.eq("g")?,
            Narrow {
                scopes: Change::Set(scopes),
            },
        )
        .await?;
    let g = entity.alias("g")?;
    let _: Vec<Vec<String>> = db
        .from(&g)
        .filter(g.column(grants::scopes).eq(vec!["a"])?)
        .select(g.column(grants::scopes).select::<Vec<String>>())?
        .all()
        .await?;
    Ok(())
}

fn storage() -> zeroship_data_orm::schema::ArrayStorage {
    grants::Entity::schema()["scopes"].storage.array
}

fn main() {}
