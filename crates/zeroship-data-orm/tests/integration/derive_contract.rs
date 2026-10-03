//! The compile-pass and compile-fail contracts for the ORM derive macros and
//! the SQL surface, driven through `trybuild`'s shared generated project.

fn compile_failure_fixtures(directory: &str) -> usize {
    std::fs::read_dir(directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "rs")
        })
        .count()
}

#[test]
fn schema_mapping_and_sql_contracts_are_checked_by_rust() {
    assert!(
        compile_failure_fixtures("tests/ui") >= 10,
        "compile-failure fixtures must be present"
    );
    assert!(
        compile_failure_fixtures("tests/sql/ui") >= 1,
        "the SQL compile-failure fixtures must be present"
    );

    // A project that contains any pass fixture links every fixture, and
    // trybuild keys its generated project directory by crate name. Keeping the
    // pass and compile-fail fixtures in separate projects lets the compile-fail
    // project `cargo check` instead of `cargo build`, and running both projects
    // from one test keeps the shared project directory single-writer.
    let compile_pass = trybuild::TestCases::new();
    compile_pass.pass("tests/pass/*.rs");

    let compile_fail = trybuild::TestCases::new();
    compile_fail.compile_fail("tests/ui/*.rs");
    compile_fail.compile_fail("tests/sql/ui/*.rs");
}
