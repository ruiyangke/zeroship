#[test]
fn statements_do_not_implement_serialize() {
    trybuild::TestCases::new().compile_fail("tests/sql/ui/statement_is_not_serializable.rs");
}

#[test]
fn raw_execution_helpers_are_not_public() {
    trybuild::TestCases::new().compile_fail("tests/sql/ui/raw_execution_is_private.rs");
}
