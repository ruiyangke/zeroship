//! Benchmark typed statement compilation and SDK filter decoding.
//! Run with `cargo bench -p zeroship-data-orm --bench bench_query_build`.

use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use zeroship_data_orm::{
    sql::{
        registration::SqlRegistration,
        statement::{
            Expression, Insert, InsertParts, ResolvedOperand, ResolvedPredicate,
            ResolvedPredicateValue, ReturnedColumn, RowLock, SelectParts, SelectStatement,
            SelectedExpression, Statement, StorageType, Table,
        },
        CompareOp, Ident, IdentRole, MembershipOp, SchemaName,
    },
    value, Value,
};

#[derive(Clone, Copy)]
enum ReadWorkload {
    Empty,
    Small,
    Complex,
}

fn ident(name: &str, role: IdentRole) -> Ident {
    Ident::parse_as(name, role).expect("benchmark identifier")
}

fn users_table(alias: Option<&str>) -> Table {
    let namespace =
        SchemaName::new("app_01HJQK2A8R000000000000000").expect("benchmark schema name");
    let collection = ident("users", IdentRole::Collection);
    let columns = [
        ("id", StorageType::Text),
        ("status", StorageType::Text),
        ("role", StorageType::Text),
        ("score", StorageType::Integer),
        ("email", StorageType::Text),
        ("name", StorageType::Text),
        ("created_at", StorageType::Timestamp),
        ("updated_at", StorageType::Timestamp),
    ]
    .map(|(name, storage)| (ident(name, IdentRole::StoredColumn), storage));
    match alias {
        Some(alias) => Table::aliased(
            namespace,
            collection,
            ident(alias, IdentRole::Alias),
            columns,
        ),
        None => Table::new(namespace, collection, columns),
    }
    .expect("benchmark table")
}

fn comparison(table: &Table, field: &str, op: CompareOp, value: Value) -> ResolvedPredicate {
    let column = table.column(field).expect("benchmark column");
    ResolvedPredicate::Compare {
        lhs: ResolvedOperand::Column(column.clone()),
        op,
        rhs: ResolvedPredicateValue::Bind {
            storage: column.storage(),
            value,
        },
    }
}

fn read_predicate(workload: ReadWorkload, table: &Table) -> ResolvedPredicate {
    match workload {
        ReadWorkload::Empty => ResolvedPredicate::Const(true),
        ReadWorkload::Small => ResolvedPredicate::and(vec![
            comparison(table, "status", CompareOp::Eq, Value::from("active")),
            comparison(table, "role", CompareOp::Eq, Value::from("admin")),
        ]),
        ReadWorkload::Complex => ResolvedPredicate::and(vec![
            ResolvedPredicate::Membership {
                lhs: ResolvedOperand::Column(table.column("status").expect("status column")),
                op: MembershipOp::In,
                values: vec![
                    Value::from("active"),
                    Value::from("pending"),
                    Value::from("trial"),
                ],
            },
            ResolvedPredicate::or(vec![
                comparison(table, "role", CompareOp::Eq, Value::from("admin")),
                comparison(
                    table,
                    "created_at",
                    CompareOp::Gte,
                    Value::Timestamp(1_767_225_600_000),
                ),
            ]),
            comparison(table, "score", CompareOp::Gte, Value::from(50)),
            comparison(table, "score", CompareOp::Lte, Value::from(100)),
        ]),
    }
}

fn read_statement(workload: ReadWorkload) -> Statement {
    let table = users_table(Some("source"));
    let projection = ["id", "status", "role", "score", "email", "name"]
        .into_iter()
        .map(|field| SelectedExpression {
            expression: ResolvedOperand::Column(table.column(field).expect("projection column")),
            alias: ident(field, IdentRole::Alias),
        })
        .collect();
    Statement::Select(
        SelectStatement::new(SelectParts {
            predicate: read_predicate(workload, &table),
            table,
            joins: Vec::new(),
            projection,
            group_by: Vec::new(),
            having: ResolvedPredicate::Const(true),
            order_by: Vec::new(),
            limit: Some(50),
            offset: Some(0),
            distinct: false,
            lock: RowLock::None,
        })
        .expect("benchmark select"),
    )
}

fn sdk_filter(workload: ReadWorkload) -> Value {
    match workload {
        ReadWorkload::Empty => value!({}),
        ReadWorkload::Small => value!({"status":"active", "role":"admin"}),
        ReadWorkload::Complex => value!({
            "$and": [
                {"status":{"$in":["active", "pending", "trial"]}},
                {"$or":[
                    {"role":"admin"},
                    {"created_at":{"$gte":1_767_225_600_000_i64}}
                ]},
                {"score":{"$gte":50, "$lte":100}}
            ]
        }),
    }
}

fn insert_statement() -> Statement {
    let table = users_table(None);
    let columns = ["id", "email", "name", "role", "created_at", "updated_at"]
        .iter()
        .map(|field| table.column(field).expect("insert column"))
        .collect();
    let rows = vec![vec![
        Expression::Bind(Value::from("usr_01HJQK2A8R000000000000000")),
        Expression::Bind(Value::from("alice@example.com")),
        Expression::Bind(Value::from("Alice Example")),
        Expression::Bind(Value::from("admin")),
        Expression::Bind(Value::Timestamp(1_769_040_000_000)),
        Expression::Bind(Value::Timestamp(1_769_040_000_000)),
    ]];
    Statement::Insert(
        Insert::new(InsertParts {
            returning: vec![ReturnedColumn {
                column: table.column("id").expect("identity column"),
                alias: None,
            }],
            table,
            columns,
            rows,
            insert_generated_identity: false,
        })
        .expect("benchmark insert"),
    )
}

fn bench_compile_read(c: &mut Criterion) {
    let registration = SqlRegistration::postgres();
    let mut group = c.benchmark_group("compile_read");
    group.measurement_time(Duration::from_secs(3));
    group.warm_up_time(Duration::from_secs(1));
    for (name, workload) in [
        ("empty", ReadWorkload::Empty),
        ("small", ReadWorkload::Small),
        ("complex", ReadWorkload::Complex),
    ] {
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &workload,
            |b, workload| {
                b.iter(|| {
                    black_box(
                        registration
                            .compile(read_statement(*workload))
                            .expect("compile benchmark select"),
                    );
                });
            },
        );
    }
    group.finish();
}

fn bench_decode_sdk_filter(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode_sdk_filter");
    group.measurement_time(Duration::from_secs(3));
    group.warm_up_time(Duration::from_secs(1));
    for (name, workload) in [
        ("empty", ReadWorkload::Empty),
        ("small", ReadWorkload::Small),
        ("complex", ReadWorkload::Complex),
    ] {
        let input = sdk_filter(workload);
        group.bench_with_input(BenchmarkId::from_parameter(name), &input, |b, input| {
            b.iter_batched_ref(
                || input.clone(),
                |input| {
                    black_box(
                        zeroship_data_orm::sql::filter::decode(input)
                            .expect("decode benchmark filter"),
                    );
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_compile_insert(c: &mut Criterion) {
    let registration = SqlRegistration::postgres();
    let mut group = c.benchmark_group("compile_insert");
    group.measurement_time(Duration::from_secs(3));
    group.warm_up_time(Duration::from_secs(1));
    group.bench_function("typed_record", |b| {
        b.iter(|| {
            black_box(
                registration
                    .compile(insert_statement())
                    .expect("compile benchmark insert"),
            );
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_compile_read,
    bench_decode_sdk_filter,
    bench_compile_insert
);
criterion_main!(benches);
