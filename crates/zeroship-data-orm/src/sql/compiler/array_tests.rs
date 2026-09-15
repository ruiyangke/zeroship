use super::*;
use crate::sql::{
    statement::{
        ArrayElement, ArrayOperator, Assignment, Expression, Insert, InsertParts, MutationScope,
        ResolvedPredicate, Statement, StorageType, Table, Update, UpdateParts,
    },
    Ident, IdentRole, SchemaName,
};
use crate::{value, Value};

const TEXT_ARRAY: StorageType = StorageType::Array(ArrayElement::Text);

fn table() -> Table {
    Table::new(
        SchemaName::new("app").unwrap(),
        Ident::parse_as("grants", IdentRole::Collection).unwrap(),
        [
            ("id", StorageType::Text),
            ("scopes", TEXT_ARRAY),
            ("tags", StorageType::Json),
        ]
        .map(|(name, storage)| {
            (
                Ident::parse_as(name, IdentRole::StoredColumn).unwrap(),
                storage,
            )
        }),
    )
    .unwrap()
}

fn insert(column: &str, value: Value) -> Result<Statement, CompileError> {
    let table = table();
    Ok(Statement::Insert(Insert::new(InsertParts {
        columns: vec![table.column(column).unwrap()],
        table,
        rows: vec![vec![Expression::Bind(value)]],
        returning: Vec::new(),
        insert_generated_identity: false,
    })?))
}

fn mutation(column: &str, operator: ArrayOperator, operand: Value) -> Result<Update, CompileError> {
    let table = table();
    let column = table.column(column).unwrap();
    Update::new(UpdateParts {
        assignments: vec![Assignment {
            column: column.clone(),
            value: Expression::ArrayMutation {
                column,
                operator,
                operand,
            },
        }],
        table,
        predicate: ResolvedPredicate::Const(true),
        scope: MutationScope::Matching,
        returning: Vec::new(),
    })
}

#[test]
fn native_text_array_binds_carry_an_explicit_postgres_cast() {
    let query = PostgresCompiler
        .compile(
            insert("scopes", value!(["a", "b"])).unwrap(),
            &PostgresCompiler.support(),
        )
        .unwrap();
    assert_eq!(
        query.sql(),
        "INSERT INTO \"app\".\"grants\" (\"scopes\") VALUES ($1::text[])"
    );
    assert_eq!(query.params(), &[value!(["a", "b"])]);
    assert_eq!(
        SqliteCompiler
            .compile(
                insert("scopes", value!(["a"])).unwrap(),
                &SqliteCompiler.support(),
            )
            .unwrap_err(),
        CompileError::Unsupported("native array storage")
    );
    for compiler in [&PostgresCompiler as &dyn SqlCompiler, &SqliteCompiler] {
        let query = compiler
            .compile(
                insert("tags", Value::Json("[\"a\"]".into())).unwrap(),
                &compiler.support(),
            )
            .unwrap();
        assert!(query.sql().ends_with("VALUES ($1)"), "{}", query.sql());
    }
    for invalid in [
        value!([1]),
        value!(["a", null]),
        value!("a"),
        Value::Json("[\"a\"]".into()),
    ] {
        assert!(insert("scopes", invalid).is_err());
    }
}

#[test]
fn native_text_array_mutations_use_postgres_array_functions() {
    for (operator, expected) in [
        (
            ArrayOperator::Push,
            "CASE WHEN \"scopes\" IS NULL THEN \"scopes\" ELSE array_append(\"scopes\", $1::text) END",
        ),
        (
            ArrayOperator::Pull,
            "CASE WHEN \"scopes\" IS NULL THEN \"scopes\" ELSE array_remove(\"scopes\", $1::text) END",
        ),
        (
            ArrayOperator::AddToSet,
            "CASE WHEN \"scopes\" IS NULL THEN \"scopes\" WHEN array_position(\"scopes\", $1::text) \
             IS NOT NULL THEN \"scopes\" ELSE array_append(\"scopes\", $1::text) END",
        ),
    ] {
        let query = PostgresCompiler
            .compile(
                Statement::Update(mutation("scopes", operator, value!("x")).unwrap()),
                &PostgresCompiler.support(),
            )
            .unwrap();
        assert_eq!(
            query.sql(),
            format!("UPDATE \"app\".\"grants\" SET \"scopes\" = {expected}")
        );
        assert_eq!(query.params(), &[value!("x")]);
        assert_eq!(
            SqliteCompiler
                .compile(
                    Statement::Update(mutation("scopes", operator, value!("x")).unwrap()),
                    &SqliteCompiler.support(),
                )
                .unwrap_err(),
            CompileError::Unsupported("native array storage")
        );
    }
    for operand in [
        value!(["x"]),
        Value::Null,
        Value::Json("\"x\"".into()),
        value!(1),
    ] {
        assert!(mutation("scopes", ArrayOperator::Push, operand).is_err());
    }
    assert!(mutation("tags", ArrayOperator::Push, Value::Json("\"x\"".into())).is_ok());
    assert!(mutation("id", ArrayOperator::Push, value!("x")).is_err());
}

#[test]
fn storage_codecs_keep_native_arrays_out_of_json() {
    use crate::sql::registration::SqlRegistration;
    let postgres = SqlRegistration::postgres();
    for (input, expected) in [
        (value!(["b", "a", "a"]), value!(["b", "a", "a"])),
        (value!([]), value!([])),
        (Value::Json("[\"NULL\"]".into()), value!(["NULL"])),
        (Value::Null, Value::Null),
    ] {
        assert_eq!(postgres.encode(TEXT_ARRAY, input).unwrap(), expected);
    }
    for invalid in [
        value!([1]),
        value!(["a", null]),
        value!(["nul\u{0}"]),
        value!("a"),
        Value::Json("null".into()),
        Value::Json("not json".into()),
    ] {
        assert!(postgres.encode(TEXT_ARRAY, invalid).is_err());
    }
    assert_eq!(
        postgres.encode(StorageType::Json, value!(["a"])).unwrap(),
        Value::Json("[\"a\"]".into())
    );
    assert_eq!(
        postgres.decode(TEXT_ARRAY, value!(["a", null])).unwrap(),
        value!(["a", null])
    );
    assert!(postgres.decode(TEXT_ARRAY, value!("a")).is_err());

    let sqlite = SqlRegistration::sqlite();
    assert!(sqlite.encode(TEXT_ARRAY, value!(["a"])).is_err());
    assert!(sqlite.decode(TEXT_ARRAY, value!(["a"])).is_err());
    let mut native = crate::schema::ColumnSchema::new(crate::schema::LogicalType::Array);
    native.items = Some(crate::schema::LogicalType::Text);
    native.storage.array = crate::schema::ArrayStorage::Native;
    assert_eq!(postgres.storage_type(&native).unwrap(), TEXT_ARRAY);
    assert_eq!(sqlite.storage_type(&native).unwrap(), StorageType::Json);
    native.storage.array = crate::schema::ArrayStorage::Json;
    assert_eq!(postgres.storage_type(&native).unwrap(), StorageType::Json);
}
