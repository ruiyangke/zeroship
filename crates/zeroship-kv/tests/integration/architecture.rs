//! Dependency boundaries checked from Cargo's resolved graph.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

fn dependencies(package: &str, edges: &str) -> BTreeSet<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(env!("CARGO"))
        .current_dir(root)
        .args([
            "tree",
            "--locked",
            "-p",
            package,
            "--all-features",
            "-e",
            edges,
            "--prefix",
            "none",
            "--format",
            "{p}",
        ])
        .output()
        .expect("run cargo tree");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let packages: BTreeSet<_> = String::from_utf8(output.stdout)
        .expect("Cargo output is UTF-8")
        .lines()
        .map(|line| {
            line.split_whitespace()
                .next()
                .expect("package name")
                .to_owned()
        })
        .collect();
    assert!(
        packages.contains(package),
        "Cargo did not enumerate {package}"
    );
    packages
}

fn forbidden_in_storage(package: &str) -> bool {
    matches!(
        package,
        "v8" | "zeroship-runtime" | "zeroship-runtime-macros" | "zeroship-metering"
    ) || package.starts_with("zeroship-plugin-")
        || (package.starts_with("zeroship-") && package.ends_with("-v8"))
}

#[test]
fn storage_and_its_tests_do_not_reach_v8_or_metering() {
    let packages = dependencies("zeroship-kv", "normal,build,dev");
    for required in ["compio-redis", "redb"] {
        assert!(
            packages.contains(required),
            "storage graph omitted {required}"
        );
    }
    let forbidden: Vec<_> = packages
        .iter()
        .filter(|p| forbidden_in_storage(p))
        .collect();
    assert!(
        forbidden.is_empty(),
        "storage reaches forbidden dependencies: {forbidden:?}"
    );
}

#[test]
fn binding_connects_storage_to_the_runtime() {
    let packages = dependencies("zeroship-kv-v8", "normal,build");
    for required in ["zeroship-kv", "zeroship-runtime", "v8"] {
        assert!(
            packages.contains(required),
            "binding graph omitted {required}"
        );
    }
}

#[test]
fn boundary_recognizes_adapters_and_allows_storage() {
    for forbidden in [
        "v8",
        "zeroship-runtime",
        "zeroship-runtime-macros",
        "zeroship-metering",
        "zeroship-example-v8",
        "zeroship-plugin-example",
    ] {
        assert!(
            forbidden_in_storage(forbidden),
            "failed to reject {forbidden}"
        );
    }
    for allowed in ["zeroship-kv", "compio-redis", "redb", "testcontainers"] {
        assert!(
            !forbidden_in_storage(allowed),
            "incorrectly rejected {allowed}"
        );
    }
}
