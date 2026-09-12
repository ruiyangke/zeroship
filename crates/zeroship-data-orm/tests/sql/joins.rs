use zeroship_data_orm::sql::*;
use crate::NumberedParameters;
use zeroship_data_orm::value;

#[test]
fn runtime_rendering_uses_each_sources_declared_value_type() {
    let root_schema = value!({"id":{"type":"string"}, "customer_id":{"type":"string"}, "created_at":{"type":"string"}});
    let child_schema = value!({"id":{"type":"string"}, "event_time":{"type":"date"}});
    let o = ident("o");
    let c = ident("c");
    let sources = [
        compile::ReadSource {
            alias: Some(&o),
            schema: &root_schema,
        },
        compile::ReadSource {
            alias: Some(&c),
            schema: &child_schema,
        },
    ];
    let query = Select::builder(ident("orders"), projection())
        .alias(o.clone())
        .join(join())
        .filter(Predicate::And(vec![
            Predicate::Compare {
                lhs: Operand::Path(path("o", "created_at")),
                op: CompareOp::Eq,
                rhs: Operand::Lit(Literal::Text("ordinary text".into())),
            },
            Predicate::Compare {
                lhs: Operand::Path(path("c", "event_time")),
                op: CompareOp::Eq,
                rhs: Operand::Lit(Literal::Int(0)),
            },
        ]))
        .build()
        .unwrap();
    for dialect in [compile::SqlDialect::Postgres, compile::SqlDialect::Sqlite] {
        let query =
            compile::build_select(&query, &SchemaName::new("app").unwrap(), &sources, dialect)
                .unwrap();
        assert!(query.params().contains(&value!("ordinary text")));
        assert!(!query.sql().contains("ordinary text"));
        assert!(query.sql().contains("\"c\".\"event_time\""));
        match dialect {
            compile::SqlDialect::Postgres => {
                assert!(query.params().contains(&value::Value::Timestamp(0)))
            }
            compile::SqlDialect::Sqlite => {
                assert!(query.params().contains(&value!("1970-01-01T00:00:00.000Z")))
            }
        }
    }
}

fn ident(value: &str) -> Ident {
    Ident::parse_as(value, IdentRole::Alias).unwrap()
}
fn path(source: &str, column: &str) -> FieldPath {
    FieldPath::column(ident(column)).in_source(ident(source))
}
fn equal(a: &str, b: &str) -> Predicate {
    Predicate::compare(
        Operand::Path(path(a, "customer_id")),
        CompareOp::Eq,
        Operand::Path(path(b, "id")),
    )
}
fn projection() -> Projection {
    Projection::rows(vec![
        ProjectedField::path(path("o", "id"), ident("order_id")),
        ProjectedField::path(path("c", "id"), ident("customer_id")),
    ])
    .unwrap()
}
fn join() -> Join {
    Join {
        kind: JoinKind::Left,
        collection: ident("customers"),
        alias: ident("c"),
        on: equal("o", "c"),
    }
}

#[test]
fn qualified_left_join_keeps_on_filters_and_bound_values() {
    let mut joined = join();
    joined.on = Predicate::And(vec![
        joined.on,
        Predicate::is_null(Operand::Path(path("c", "deleted_at"))),
    ]);
    let plan = Select::builder(ident("orders"), projection())
        .namespace(ident("app"))
        .alias(ident("o"))
        .join(joined)
        .filter(Predicate::compare(
            Operand::Path(path("o", "status")),
            CompareOp::Eq,
            Operand::Lit(Literal::Text("paid' OR TRUE".into())),
        ))
        .build()
        .unwrap();
    let rendered = render::postgres::render_select(&plan).unwrap();
    assert!(
        rendered
            .sql()
            .contains("LEFT JOIN \"app\".\"customers\" AS \"c\" ON")
    );
    let (on, filter) = rendered.sql().split_once(" WHERE ").unwrap();
    assert!(on.contains("\"c\".\"deleted_at\" IS NULL"));
    assert!(filter.contains("\"o\".\"status\""));
    assert!(!rendered.sql().contains("paid'"));
    assert_eq!(
        rendered.placeholder_slots(),
        (1..=rendered.params().len()).collect::<Vec<_>>()
    );
}

#[test]
fn ambiguous_and_forward_sources_are_refused() {
    for on in [
        equal("future", "c"),
        Predicate::Or(vec![equal("o", "c"), Predicate::always()]),
        Predicate::always(),
    ] {
        let mut j = join();
        j.on = on;
        assert!(
            Select::builder(ident("orders"), projection())
                .alias(ident("o"))
                .join(j)
                .build()
                .is_err()
        );
    }
    assert!(
        Select::builder(ident("orders"), projection())
            .alias(ident("c"))
            .join(join())
            .build()
            .is_err()
    );
    let unqualified = Projection::rows(vec![ProjectedField::column(ident("id")).unwrap()]).unwrap();
    assert!(
        Select::builder(ident("orders"), unqualified)
            .alias(ident("o"))
            .join(join())
            .build()
            .is_err()
    );
}

#[test]
fn grouped_join_qualifies_aggregate_arguments() {
    let count = AggregateRef::over_path(AggregateFunc::Count, path("c", "id"), true).unwrap();
    let projection = Projection::aggregate(vec![
        ProjectedField::aggregate(count.clone(), ident("matches")),
        ProjectedField::path(path("o", "id"), ident("order_id")),
    ])
    .unwrap();
    let plan = Select::builder(ident("orders"), projection)
        .alias(ident("o"))
        .join(join())
        .group_by(vec![path("o", "id")])
        .having(Predicate::compare(
            Operand::Aggregate(count),
            CompareOp::Gt,
            Operand::Lit(Literal::Int(0)),
        ))
        .build()
        .unwrap();
    let sql = render::postgres::render_select(&plan).unwrap();
    assert!(sql.sql().contains("COUNT(DISTINCT \"c\".\"id\")"));
    assert!(sql.sql().contains("GROUP BY \"o\".\"id\" HAVING"));
}

#[test]
fn self_and_composite_joins_preserve_order() {
    let first = Join {
        kind: JoinKind::Left,
        collection: ident("orders"),
        alias: ident("c"),
        on: Predicate::And(vec![
            equal("o", "c"),
            Predicate::compare(
                Operand::Path(path("o", "tenant")),
                CompareOp::Eq,
                Operand::Path(path("c", "tenant")),
            ),
        ]),
    };
    let second = Join {
        kind: JoinKind::Inner,
        collection: ident("orders"),
        alias: ident("z"),
        on: equal("c", "z"),
    };
    let plan = Select::builder(ident("orders"), projection())
        .alias(ident("o"))
        .join(first)
        .join(second)
        .build()
        .unwrap();
    let sql = render::postgres::render_select(&plan).unwrap();
    assert!(sql.sql().find(" AS \"c\" ON").unwrap() < sql.sql().find(" AS \"z\" ON").unwrap());
    assert!(sql.sql().contains("\"o\".\"tenant\""));
}
