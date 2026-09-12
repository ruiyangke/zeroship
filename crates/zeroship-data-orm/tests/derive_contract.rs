#[test]
fn schema_and_mapping_contracts_are_checked_by_rust() {
    let cases = std::fs::read_dir("tests/ui")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "rs")
        })
        .count();
    assert!(cases >= 10, "compile-failure fixtures must be present");
    let tests = trybuild::TestCases::new();
    tests.pass("tests/pass/*.rs");
    tests.compile_fail("tests/ui/*.rs");
}
