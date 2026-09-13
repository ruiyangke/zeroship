#![allow(dead_code)]

use zeroship_data_orm::{Database, orm::*};

schema!(pub schema = "../fixtures/schema.runtime.json");
use schema::posts;

#[derive(FromRow)]
#[orm(entity = posts)]
struct Post {
    id: String,
    title: String,
    score: Option<f64>,
}

#[derive(Insertable)]
#[orm(entity = posts)]
struct NewPost<'a> {
    title: &'a str,
}

async fn queries(db: &Database) -> Result<(), DbError> {
    let posts = db.entity::<posts::Entity>()?;
    let _: Vec<Post> = posts
        .query()
        .filter(
            posts::score
                .gte(Some(1.0))?
                .and(posts::title.in_values(["first", "second"])?),
        )
        .order_by(posts::score.desc().nulls_last())
        .order_by(posts::id.asc())
        .offset(1)?
        .limit(20)?
        .all()
        .await?;
    let _: Option<Post> = posts.query().first().await?;
    let _: i64 = posts.query().include_deleted().count().await?;
    let _: i64 = posts.count(posts::score.is_not_null()).await?;
    let _: Vec<Post> = posts.find(Filter::all(), FindOptions::default()).await?;

    let p = posts.alias("p")?;
    let _: Vec<(Option<String>, i64, Option<f64>)> = db
        .from(&p)
        .group_by(p.column(posts::nickname))
        .having(count_rows().gte(2_i64)?)
        .select((
            p.column(posts::nickname).select(),
            count_rows(),
            p.column(posts::score).avg(),
        ))?
        .all()
        .await?;
    let _: Vec<(i64, i64, Option<i64>, Option<f64>, Option<i64>, Option<i64>)> = db
        .from(&p)
        .select((
            p.column(posts::score).count(),
            p.column(posts::nickname).count_distinct(),
            p.column(posts::counter).sum(),
            p.column(posts::score).sum(),
            p.column(posts::counter).min(),
            p.column(posts::counter).max(),
        ))?
        .all()
        .await?;
    let child = posts.alias("child")?;
    let _: Vec<(String, Option<String>)> = db
        .from(&p)
        .left_join(
            &child,
            p.column(posts::nickname)
                .eq_column(child.column(posts::title))?,
        )?
        .select((
            p.column(posts::title).select(),
            child.column(posts::title).select_optional(),
        ))?
        .all()
        .await?;
    Ok(())
}

async fn mutations(db: &Database) -> Result<(), DbError> {
    let posts = db.entity::<posts::Entity>()?;
    let inserted: Post = posts.insert(NewPost { title: "first" }).await?;
    let _: Vec<Post> = posts
        .insert_many([NewPost { title: "second" }, NewPost { title: "third" }])
        .await?;
    let _: Post = posts
        .upsert(
            NewPost { title: "upsert" },
            ConflictTarget::new(posts::title),
        )
        .await?;
    let _ = ConflictTarget::new(posts::title).and(posts::nickname);
    let _: Option<Post> = posts
        .update(posts::id.eq(inserted.id)?, posts::title.set("edited")?)
        .await?;
    let _: i64 = posts
        .update_many(Filter::all(), posts::score.set(Some(2.0))?)
        .await?;
    let _: Option<Post> = posts.delete(posts::title.eq("edited")?).await?;
    let _: i64 = posts.delete_many(Filter::all()).await?;
    let _: Option<Post> = posts.restore(posts::title.eq("edited")?).await?;
    let _: i64 = posts.restore_many(Filter::all()).await?;
    let _: Option<Post> = posts.purge(posts::title.eq("edited")?).await?;
    let _: i64 = posts.purge_many(Filter::all()).await?;
    Ok(())
}

async fn transactions(db: &Database) -> Result<(), DbError> {
    db.transaction_with_options(
        TransactionOptions::default().isolation_level(IsolationLevel::Serializable),
        |tx| async move {
            let _: Post = tx
                .entity::<posts::Entity>()?
                .insert(NewPost {
                    title: "transaction",
                })
                .await?;
            tx.transaction(|nested| async move {
                let _: i64 = nested
                    .entity::<posts::Entity>()?
                    .count(Filter::all())
                    .await?;
                Ok(())
            })
            .await
        },
    )
    .await
}

fn main() {}
