//! Rules the type system and the attribute enforce before anything links.
//!
//! Every file under `tests/ui` must FAIL to compile. The suite asserts a
//! non-empty case list first, so a mis-globbed directory cannot report a
//! vacuous pass.

use std::fs;
use std::path::Path;

#[test]
fn declaration_and_consumer_misuse_does_not_compile() {
    // Does not cover: anything that is legal Rust but wrong policy, such as an
    // identity with no ConfigSpec. That is a linked-registry check, not a type
    // error, and lives in contract_fixtures.rs.
    let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ui");
    let cases = fs::read_dir(&ui)
        .expect("ui fixture directory")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "rs"))
        .count();
    assert!(
        cases >= 9,
        "expected the nine compile-fail fixtures, found {cases}; a moved or \
         renamed directory would otherwise make this test pass at zero"
    );

    let harness = trybuild::TestCases::new();
    harness.compile_fail(ui.join("*.rs"));
}
