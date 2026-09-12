#[test]
fn statements_do_not_implement_serialize() {
    trybuild::TestCases::new().compile_fail("tests/sql/ui/statement_is_not_serializable.rs");
}
