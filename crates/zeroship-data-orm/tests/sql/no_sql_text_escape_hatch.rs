#[test]
fn plans_do_not_implement_serialize() {
    trybuild::TestCases::new().compile_fail("tests/sql/ui/plan_is_not_serializable.rs");
}
