use std::path::Path;

use zeroship_config_contract::metadata::{
    BinaryTarget, MetadataContractError, TargetClass, TargetClassification, check_workspace,
    validate_target_classifications,
};

#[test]
fn live_workspace_binary_inventory_is_complete() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("config-contract is two levels below the workspace root");
    let summary = check_workspace(&root.join("Cargo.toml")).expect("classified workspace bins");

    assert!(summary.binaries > 0);
    assert!(summary.platform > 0);
    assert_eq!(summary.platform + summary.excluded, summary.binaries);
}

#[test]
fn empty_metadata_extraction_is_a_failure() {
    // Mutation: the extractor hands the checker no Cargo binary targets at all.
    // Does not cover: whether Cargo itself discovers every cfg-gated read site;
    // this fixture protects only the cargo-metadata target-enumeration boundary.
    let errors = validate_target_classifications(&[], &[]).expect_err("empty must fail");

    assert_eq!(errors, vec![MetadataContractError::EmptyBinaryExtraction]);
}

#[test]
fn an_unclassified_binary_is_a_failure() {
    // Mutation: a new binary appears without package-owned classification.
    // Does not cover: whether a truthful `platform` record has a compiled
    // ConfigSpec registry; that cross-crate equality becomes active later.
    let binary = BinaryTarget {
        package: "fixture".to_owned(),
        target: "new-server".to_owned(),
        required_features: Vec::new(),
    };
    let errors = validate_target_classifications(&[binary], &[])
        .expect_err("an unclassified target must fail");

    assert!(errors.iter().any(|error| matches!(
        error,
        MetadataContractError::MissingClassification { target, .. }
            if target == "new-server"
    )));
}

#[test]
fn classification_needs_a_nonempty_reason() {
    // Mutation: a production target is classified with an empty explanation.
    // Does not cover: semantic review of a nonempty reason; prose cannot prove
    // that a target was assigned the right class.
    let binary = BinaryTarget {
        package: "fixture".to_owned(),
        target: "server".to_owned(),
        required_features: Vec::new(),
    };
    let classification = TargetClassification {
        package: "fixture".to_owned(),
        target: "server".to_owned(),
        class: TargetClass::Platform,
        reason: "  ".to_owned(),
    };
    let errors = validate_target_classifications(&[binary], &[classification])
        .expect_err("blank reason must fail");

    assert!(errors
        .iter()
        .any(|error| matches!(error, MetadataContractError::BlankReason { .. })));
}
