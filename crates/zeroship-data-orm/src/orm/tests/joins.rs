use super::*;
use zeroship_data_sql::{AggregateFunc, AggregateRef, CompareOp, Operand, Predicate};

#[derive(Debug, FromRow)]
#[orm(entity = posts)]
struct NullableSummary {
    score: Option<f64>,
}

pub(super) async fn exercise(db: &Database) {
    let collection = db.collection("posts").unwrap();
    for document in [
        value!({"title":"join_parent", "nickname":"join_child"}),
        value!({"title":"join_child", "nickname":null, "score":null}),
        value!({"title":"join_orphan", "nickname":"absent"}),
    ] {
        collection.insert(document).await.unwrap();
    }
    let o = db.entity::<posts::Entity>().unwrap().alias("o").unwrap();
    let c = db.entity::<posts::Entity>().unwrap().alias("c").unwrap();
    let on = || {
        o.column(posts::nickname)
            .eq_column(c.column(posts::title))
            .unwrap()
    };
    let read = || {
        db.from(&o)
            .left_join(&c, on())
            .unwrap()
            .filter(o.column(posts::title).eq("join_parent").unwrap())
            .select((o.row::<Summary>(), c.optional_row::<NullableSummary>()))
            .unwrap()
    };
    let rows = read().all().await.unwrap();
    assert_eq!(rows[0].0.name, "join_parent");
    assert!(
        rows[0].1.as_ref().unwrap().score.is_none(),
        "a matched all-null projection is present"
    );
    let unmatched = db
        .from(&o)
        .left_join(&c, on())
        .unwrap()
        .filter(o.column(posts::title).eq("join_orphan").unwrap())
        .select((o.row::<Summary>(), c.optional_row::<NullableSummary>()))
        .unwrap()
        .all()
        .await
        .unwrap();
    assert!(unmatched[0].1.is_none());
    assert!(
        db.from(&o)
            .left_join(&c, on())
            .unwrap()
            .select(c.row::<Summary>())
            .unwrap()
            .all()
            .await
            .is_err()
    );

    collection
        .delete(value!({"title":"join_child"}))
        .await
        .unwrap();
    let deleted = read().all().await.unwrap();
    assert_eq!(deleted[0].0.name, "join_parent");
    assert!(
        deleted[0].1.is_none(),
        "right visibility must preserve the parent"
    );
    let inner = db
        .from(&o)
        .inner_join(&c, on())
        .unwrap()
        .filter(o.column(posts::title).eq("join_parent").unwrap())
        .select(o.row::<Summary>())
        .unwrap()
        .all()
        .await
        .unwrap();
    assert!(inner.is_empty());

    let source = ReadSource::new("posts", "o");
    let child = ReadSource::new("posts", "c");
    let mut grouped = ReadQuery::new(source.clone());
    grouped.joins.push(ReadJoin {
        kind: zeroship_data_sql::JoinKind::Left,
        source: child.clone(),
        on: on(),
    });
    grouped.filter = o.column(posts::title).eq("join_parent").unwrap();
    let count =
        AggregateRef::over_path(AggregateFunc::Count, child.column("id").unwrap(), false).unwrap();
    grouped.projection = vec![ReadProjection::Scalar {
        output: "matches".into(),
        expression: Operand::Aggregate(count.clone()),
    }];
    grouped.having = Predicate::compare(
        Operand::Aggregate(count),
        CompareOp::Eq,
        Operand::Lit(zeroship_data_sql::Literal::Int(0)),
    );
    let Output::Rows { rows, .. } = db.read(grouped.clone()).await.unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows, vec![value!({"matches":0})]);

    grouped.projection = [
        ("average", AggregateFunc::Avg, "counter"),
        ("total", AggregateFunc::Sum, "counter"),
        ("earliest", AggregateFunc::Min, "created_at"),
    ]
    .into_iter()
    .map(|(output, function, field)| ReadProjection::Scalar {
        output: output.into(),
        expression: Operand::Aggregate(
            AggregateRef::over_path(function, source.column(field).unwrap(), false).unwrap(),
        ),
    })
    .collect();
    let Output::Rows { rows, .. } = db.read(grouped).await.unwrap() else {
        panic!("expected aggregate rows")
    };
    assert_eq!(rows[0]["average"].as_f64(), Some(7.0));
    assert_eq!(rows[0]["total"], value!(7));
    assert!(matches!(rows[0]["earliest"], Value::Timestamp(_)));

    let tx_result = db
        .transaction(|tx| async move {
            tx.collection("posts")?
                .insert(value!({"title":"join_tx", "nickname":"join_tx"}))
                .await?;
            let a = tx.entity::<posts::Entity>()?.alias("a")?;
            let b = tx.entity::<posts::Entity>()?.alias("b")?;
            let rows = tx
                .from(&a)
                .inner_join(
                    &b,
                    a.column(posts::nickname)
                        .eq_column(b.column(posts::title))?,
                )?
                .filter(a.column(posts::title).eq("join_tx")?)
                .select((a.row::<Summary>(), b.row::<Summary>()))?
                .all()
                .await?;
            assert_eq!(rows[0].0.name, rows[0].1.name);
            Ok(tx.from(&a).select(a.row::<Summary>())?)
        })
        .await
        .unwrap();
    assert!(
        tx_result.all().await.is_err(),
        "escaped queries must retain the expired transaction scope"
    );
}
