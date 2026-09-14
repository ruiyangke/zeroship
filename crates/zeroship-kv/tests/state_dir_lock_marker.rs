//! Keep the Vite diagnostic token aligned with the compiled storage constant.

#![cfg(feature = "redb")]

use zeroship_kv::STATE_DIR_LOCK_MARKER;

fn check_marker(source: &str, expected: &str) -> Result<(), String> {
    let declarations: Vec<_> = source
        .lines()
        .filter_map(|line| line.trim().strip_prefix("const STATE_DIR_LOCK_MARKER ="))
        .collect();
    let [declaration] = declarations.as_slice() else {
        return Err("expected a unique TypeScript STATE_DIR_LOCK_MARKER declaration".into());
    };
    let marker: String = serde_json::from_str(declaration.trim().trim_end_matches(';'))
        .map_err(|error| format!("cannot read TypeScript marker: {error}"))?;
    if marker.is_empty() || marker != expected {
        return Err(format!(
            "state-dir lock marker mismatch: Rust={expected:?}, TypeScript={marker:?}"
        ));
    }
    Ok(())
}

#[test]
fn vite_recognizes_the_storage_lock_marker() {
    check_marker(
        include_str!("../../../packages/vite-plugin/src/dev-server.ts"),
        STATE_DIR_LOCK_MARKER,
    )
    .unwrap();
}

#[test]
fn marker_check_rejects_drift_and_missing_or_ambiguous_declarations() {
    let declaration = "const STATE_DIR_LOCK_MARKER = \"expected\";\n";
    assert!(check_marker(declaration, "expected").is_ok());
    assert!(check_marker(declaration, "different").is_err());
    assert!(check_marker("// no declaration", "expected").is_err());
    assert!(check_marker(&declaration.repeat(2), "expected").is_err());
    assert!(check_marker("const STATE_DIR_LOCK_MARKER = compute();", "expected").is_err());
}
