use super::{repo, source};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};

fn tree(crate_name: &str) -> BTreeMap<std::path::PathBuf, source::Source> {
    source::module_tree(&repo::root().join(format!("crates/{crate_name}/src/lib.rs")))
}
fn adapter_dependency_forbidden(name: &str) -> bool {
    matches!(
        name,
        "aes-gcm"
            | "hkdf"
            | "hmac"
            | "zeroize"
            | "rusqlite"
            | "sqlite-vec"
            | "compio-postgres"
            | "zeroship-migrate-policy"
    )
}
const DRIVERS: &[&str] = &[
    "compio_postgres",
    "rusqlite",
    "PostgresBackend",
    "SqliteBackend",
];

#[test]
fn adapter_has_no_backend_dependencies_exports_or_concrete_knowledge() {
    let dependencies = repo::package("zeroship-data-v8")["dependencies"]
        .as_array()
        .unwrap();
    assert!(!dependencies.is_empty());
    for dependency in dependencies.iter().filter(|d| d["kind"] != "dev") {
        let name = dependency["name"].as_str().unwrap();
        assert!(
            !adapter_dependency_forbidden(name),
            "adapter depends on {name}"
        );
    }
    let files = tree("zeroship-data-v8");
    assert!(!files.is_empty());
    let owner = Regex::new(r"\b(zeroship_data_orm|backend)\b").unwrap();
    for (path, source) in files {
        assert!(
            !source.names_any(DRIVERS) && !source.names_any(&["BackendUrl"]),
            "adapter names a concrete backend: {}",
            path.display()
        );
        assert!(
            source.public_uses.iter().all(|item| !owner.is_match(item)),
            "adapter re-exports its implementation owner: {}",
            path.display()
        );
    }
}

#[test]
fn adapter_boundary_checks_reject_forbidden_controls() {
    for name in [
        "aes-gcm",
        "hkdf",
        "hmac",
        "zeroize",
        "rusqlite",
        "sqlite-vec",
        "compio-postgres",
        "zeroship-migrate-policy",
    ] {
        assert!(adapter_dependency_forbidden(name));
    }
    for name in ["zeroship-data-orm", "zeroship-runtime"] {
        assert!(!adapter_dependency_forbidden(name));
    }
    for input in [
        "pub use zeroship_data_orm::cdc::broker;",
        "pub use zeroship_data_orm::sql;",
        "pub\nuse\nbackend::pg_row_json;",
    ] {
        assert!(!source::parse(input).public_uses.is_empty());
    }
    for input in [
        "use zeroship_data_orm::cdc::broker;",
        "pub(crate) use zeroship_data_orm::sql::mapping;",
    ] {
        assert!(source::parse(input).public_uses.is_empty());
    }
    assert!(source::parse("fn f() { PostgresBackend::connect(); }").names_any(DRIVERS));
    assert!(!source::parse("fn f() { ConnectionFactory::for_url(); }").names_any(DRIVERS));
}

#[test]
fn contracts_codecs_and_context_have_authoritative_owners() {
    let duplicates = [
        "SqlExecutor",
        "SchemaIntrospect",
        "VectorIndex",
        "SpatialIndex",
        "DialectBuilder",
    ];
    for file in [
        "driver",
        "executor",
        "protection",
        "search",
        "storage",
        "cdc/source",
    ] {
        let input = repo::read(&format!("crates/zeroship-data-orm/src/{file}.rs"));
        let source = source::parse(&input);
        assert!(
            duplicates
                .iter()
                .all(|name| !source.public_traits.contains(*name)),
            "duplicate runtime contract in {file}"
        );
    }
    for file in ["crud/mod", "crud/write_pipeline", "crud/read_pipeline"] {
        let source = source::parse(&repo::read(&format!(
            "crates/zeroship-data-orm/src/{file}.rs"
        )));
        assert!(
            !source.contains("if dialect == SqlDialect::") && !source.contains("match dialect"),
            "dialect conversion in {file}"
        );
        assert!(
            !source.names_any(&[
                "lower_boolean",
                "encode_sqlite",
                "normalize_boolean",
                "normalize_timestamp"
            ]),
            "physical codec in {file}"
        );
    }
    for file in [
        "schema_cache",
        "tx_lanes",
        "protection/mask_policy",
        "protection/protection_floor",
    ] {
        let source = source::parse(&repo::read(&format!(
            "crates/zeroship-data-orm/src/{file}.rs"
        )));
        assert!(
            !source.names_any(&["thread_local"]),
            "state outside OrmContext: {file}"
        );
    }
    for name in duplicates {
        assert!(source::parse(&format!("pub trait {name} {{}}"))
            .public_traits
            .contains(name));
    }
    assert!(
        source::parse("thread_local! { static STATE: usize = 0; }").names_any(&["thread_local"])
    );
}

#[test]
fn backend_compilers_dispatch_every_statement_family() {
    for backend in ["postgres", "sqlite"] {
        let source = source::parse(&repo::read(&format!(
            "crates/zeroship-data-orm/src/sql/compiler/{backend}.rs"
        )));
        for statement in [
            "Select",
            "VectorSearch",
            "SpatialNear",
            "Insert",
            "Upsert",
            "Update",
            "Delete",
        ] {
            assert!(
                source.contains(&format!("Statement :: {statement}")),
                "{backend} compiler does not dispatch Statement::{statement}"
            );
        }
    }
}

#[test]
fn dependency_closures_preserve_library_and_service_boundaries() {
    let members: BTreeSet<_> = repo::workspace()
        .into_iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(!members.contains("zeroship-data-sql"), "SQL belongs to the ORM package");
    for name in ["zeroship-core", "zeroship-migrate-server"] {
        assert!(members.contains(name));
        let closure = repo::normal_closure(name);
        assert!(!closure.contains("zeroship-data-orm"), "{name} must not depend on ORM execution");
    }
    for name in ["zeroship-data-macros"] {
        assert!(members.contains(name));
        let closure = repo::normal_closure(name);
        for forbidden in [
            "compio",
            "zeroship-data-orm",
            "compio-postgres",
            "rusqlite",
            "zeroship-runtime",
            "v8",
        ] {
            assert!(!closure.contains(forbidden), "{name} reaches {forbidden}");
        }
    }
    let closure = repo::normal_closure("zeroship-data-macros");
    assert!(
        !closure.contains("zeroship-data-sql"),
        "macros must validate descriptors without depending on SQL"
    );
    assert!(
        closure.contains("syn"),
        "macro parsing must be in the closure"
    );
    let closure = repo::normal_closure("zeroship-data-orm");
    for forbidden in [
        "zeroship-runtime",
        "zeroship-data-v8",
        "v8",
        "zeroship-metering",
    ] {
        assert!(!closure.contains(forbidden), "ORM reaches {forbidden}");
    }
    assert!(
        closure.contains("compio-postgres"),
        "the dependency detector must find the ORM's real driver"
    );
    // Usage attribution is the host's: the adapter meters creator bindings and
    // hands the ORM a sink. The control proves the detector sees the meter.
    assert!(
        repo::normal_closure("zeroship-data-v8").contains("zeroship-metering"),
        "the dependency detector must find the adapter's meter"
    );
    let mut binaries = 0;
    let mut control = false;
    for package in repo::workspace() {
        if !package["targets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["kind"].as_array().unwrap().iter().any(|k| k == "bin"))
        {
            continue;
        }
        let name = package["name"].as_str().unwrap();
        if name == "zeroship-data-cdc-server" {
            continue;
        }
        binaries += 1;
        let reaches = repo::normal_closure(name).contains("zeroship-data-cdc-server");
        if name == "zeroship-config-contract" {
            control = true;
            assert!(
                reaches,
                "registry checker must link the relay's configuration registry"
            );
        } else {
            assert!(!reaches, "shipped binary {name} links the privileged relay");
        }
    }
    assert!(
        binaries >= 7 && control,
        "binary corpus or its positive control disappeared"
    );
}

/// The packages a shipped build of `name` actually links on normal edges.
///
/// `cargo tree -e normal` selects the activated normal edges for this package
/// alone, so an optional dependency enabled only by a sibling's dev-dependency
/// feature is absent. Reading the workspace metadata instead unifies those dev
/// features and reports an edge a deployed binary does not have.
fn cargo_normal_closure(name: &str) -> BTreeSet<String> {
    let output = std::process::Command::new(env!("CARGO"))
        .current_dir(repo::root())
        .args([
            "tree",
            "-p",
            name,
            "--locked",
            "-e",
            "normal",
            "--prefix",
            "none",
            "--format",
            "{p}",
            "--color",
            "never",
        ])
        .output()
        .unwrap_or_else(|error| panic!("cargo tree -p {name}: {error}"));
    assert!(
        output.status.success(),
        "cargo tree -p {name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).expect("cargo tree prints utf-8");
    let row = Regex::new(r"^([A-Za-z0-9_-]+) v[0-9][^ ]*(?: \(.+\))?$").unwrap();
    let mut packages = BTreeSet::new();
    for line in text.lines() {
        let line = line.strip_suffix(" (*)").unwrap_or(line);
        let capture = row
            .captures(line)
            .unwrap_or_else(|| panic!("unexpected cargo tree row: {line:?}"));
        packages.insert(capture[1].to_owned());
    }
    assert!(
        !packages.is_empty(),
        "cargo tree -p {name} reported no packages"
    );
    packages
}

/// The classes a package's manifest gives its targets, keyed by target name.
fn target_classes(package: &serde_json::Value) -> BTreeMap<String, String> {
    package["metadata"]["zeroship-config"]["targets"]
        .as_array()
        .map(|targets| {
            targets
                .iter()
                .filter_map(|target| {
                    Some((
                        target["target"].as_str()?.to_owned(),
                        target["class"].as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The names of a package's binary targets.
fn bin_targets(package: &serde_json::Value) -> Vec<String> {
    package["targets"]
        .as_array()
        .expect("package targets")
        .iter()
        .filter(|target| {
            target["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"))
        })
        .filter_map(|target| target["name"].as_str().map(str::to_owned))
        .collect()
}

/// Whether the package ships a binary: one its manifest does not classify
/// `test-dev-tool`.
fn ships_a_binary(package: &serde_json::Value) -> bool {
    let classes = target_classes(package);
    bin_targets(package)
        .iter()
        .any(|name| classes.get(name).map(String::as_str) != Some("test-dev-tool"))
}

/// Whether the package exists only for tests: its manifest classifies the
/// package itself `test-dev-tool` and it ships no binary.
fn dev_only(package: &serde_json::Value) -> bool {
    let name = package["name"].as_str().expect("package name");
    target_classes(package).get(name).map(String::as_str) == Some("test-dev-tool")
        && !ships_a_binary(package)
}

/// The real package names `package` declares on `kind` edges (`None` for
/// normal), whatever their alias or target table.
fn declared(package: &serde_json::Value, kind: Option<&str>) -> BTreeSet<String> {
    package["dependencies"]
        .as_array()
        .expect("package dependencies")
        .iter()
        .filter(|dependency| dependency["kind"].as_str() == kind)
        .filter_map(|dependency| dependency["name"].as_str().map(str::to_owned))
        .collect()
}

/// No shipped binary may carry a dev-only package or the testcontainer client
/// in its normal closure.
///
/// The dev-only set is every workspace package its own manifest classifies
/// `test-dev-tool`, so a new testkit is covered the day it is classified. They
/// are workspace members, so a mistaken normal edge would put `testcontainers`
/// and its tokio runtime into a deployed binary. `testcontainers` reaches tokio
/// `rt`, which the tokio boundary also reports as a feature change; this
/// states the package edge on its own so a future dev-only dependency that does
/// not need `rt` still cannot ship.
#[test]
fn shipped_binaries_do_not_reach_the_dev_only_testkits() {
    let workspace = repo::workspace();
    let mut forbidden: BTreeSet<String> = workspace
        .iter()
        .filter(|package| dev_only(package))
        .map(|package| package["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        forbidden.len() >= 3 && forbidden.contains("zeroship-testkit"),
        "the dev-only classification read found no testkit: {forbidden:?}"
    );
    forbidden.extend(["testcontainers".to_owned(), "bollard".to_owned()]);

    let mut examined = 0;
    let mut dev_edges = 0;
    for package in &workspace {
        if !ships_a_binary(package) {
            continue;
        }
        examined += 1;
        let name = package["name"].as_str().unwrap();
        let closure = cargo_normal_closure(name);
        for edge in &forbidden {
            assert!(
                !closure.contains(edge),
                "shipped binary {name} reaches {edge} in its normal closure"
            );
        }
        // The positive control: the reader sees every unconditional normal
        // workspace edge the manifest declares, so the absences above are read
        // through a closure that holds what is really there.
        for dependency in package["dependencies"].as_array().unwrap() {
            let real = dependency["name"].as_str().unwrap();
            if dependency["kind"].is_null()
                && dependency["target"].is_null()
                && dependency["optional"] == false
                && workspace.iter().any(|member| member["name"] == real)
            {
                assert!(
                    closure.contains(real),
                    "the normal closure reader lost {name}'s normal edge to {real}"
                );
            }
        }
        // A shipped crate that DECLARES a dev edge to a dev-only package is the
        // case the reader exists for: the edge is real in the manifest and
        // absent from what a deployed binary links.
        if declared(package, Some("dev")).iter().any(|dev| forbidden.contains(dev)) {
            dev_edges += 1;
        }
    }
    assert!(
        examined >= 5,
        "shipped-binary scan lost its corpus: {examined}"
    );
    assert!(
        dev_edges >= 3,
        "no shipped crate dev-depends on a dev-only package, so the scan never \
         separated a dev edge from a normal one: {dev_edges}"
    );
}

#[test]
fn the_dev_only_classification_reader_admits_only_library_only_test_dev_tools() {
    let testkit = serde_json::json!({
        "name": "zeroship-testkit",
        "targets": [{"name": "zeroship-testkit", "kind": ["lib"]}],
        "metadata": {"zeroship-config": {"targets": [
            {"target": "zeroship-testkit", "class": "test-dev-tool"}
        ]}}
    });
    assert!(dev_only(&testkit));
    let service_with_a_test_bin = serde_json::json!({
        "name": "zeroship-control",
        "targets": [
            {"name": "zeroship-control", "kind": ["bin"]},
            {"name": "zeroship-mock-stripe", "kind": ["bin"]}
        ],
        "metadata": {"zeroship-config": {"targets": [
            {"target": "zeroship-control", "class": "platform"},
            {"target": "zeroship-mock-stripe", "class": "test-dev-tool"}
        ]}}
    });
    assert!(!dev_only(&service_with_a_test_bin));
    assert!(ships_a_binary(&service_with_a_test_bin));
    let unclassified = serde_json::json!({
        "name": "zeroship-id",
        "targets": [{"name": "zeroship-id", "kind": ["lib"]}]
    });
    assert!(!dev_only(&unclassified));
}

fn vendor_tier(path: &std::path::Path) -> bool {
    path.components().any(|c| c.as_os_str() == "backend")
        && path
            .components()
            .any(|c| c.as_os_str() == "postgres" || c.as_os_str() == "sqlite")
}

#[test]
fn driver_names_stay_inside_backend_modules() {
    let mut examined = 0;
    for name in ["zeroship-data-v8", "zeroship-data-orm"] {
        for (path, source) in tree(name) {
            if vendor_tier(&path) {
                continue;
            }
            examined += 1;
            let names_driver = source.names_any(&["compio_postgres", "rusqlite"]);
            assert!(
                !names_driver,
                "non-backend code names a driver: {}",
                path.display()
            );
        }
    }
    assert!(examined >= 25, "source corpus disappeared");
}

fn host_service(source: &source::Source) -> bool {
    source.names_any(&[
        "DbBinding",
        "KeyStore",
        "ProjectKeySource",
        "VectorSearch",
        "SpatialSearch",
        "Search",
        "Catalog",
        "Protection",
        "Backend",
        "prepare_for_app",
        "app_id",
        "publishes_committed_changes",
        "broker",
        "cdc",
        "binding",
        "encryption",
        "protection",
        "search",
    ]) || source.contains("use super::*")
}
#[test]
fn physical_drivers_are_plain_execution_contracts() {
    for file in [
        "driver.rs",
        "backend/postgres/driver.rs",
        "backend/sqlite/driver.rs",
    ] {
        assert!(
            !host_service(&source::parse(&repo::read(&format!(
                "crates/zeroship-data-orm/src/{file}"
            )))),
            "host service inside {file}"
        );
    }
    assert!(host_service(&source::parse("fn f(binding: DbBinding) {}")));
    assert!(host_service(&source::parse(
        "fn f() -> KeyStore { todo!() }"
    )));
    assert!(host_service(&source::parse("use super::*;")));
    assert!(!host_service(&source::parse(
        "fn execute(statement: &Statement) {}"
    )));
}

fn sql_verbs(source: &source::Source) -> Vec<&'static str> {
    const VERBS: &[&str] = &[
        "ROLLBACK TO SAVEPOINT",
        "RELEASE SAVEPOINT",
        "INSERT INTO",
        "DELETE FROM",
        "SAVEPOINT",
        "SET LOCAL",
        "TRUNCATE",
        "ROLLBACK",
        "SET ROLE",
        "EXECUTE",
        "RELEASE",
        "REVOKE",
        "SELECT",
        "UPDATE",
        "CREATE",
        "COMMIT",
        "VALUES",
        "DO $$",
        "BEGIN",
        "GRANT",
        "ALTER",
        "DROP",
    ];
    source
        .raw_literals
        .iter()
        .flat_map(|literal| literal.lines())
        .filter_map(|line| {
            let line = line.trim().trim_start_matches(['r', '#', '"', '\'']);
            VERBS.iter().copied().find(|verb| {
                line.strip_prefix(verb).is_some_and(|tail| {
                    !tail.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
                })
            })
        })
        .collect()
}

#[test]
fn shared_execution_keeps_sql_and_driver_work_at_their_boundaries() {
    let baseline = [
        ("error.rs", "COMMIT"),
        ("error.rs", "ROLLBACK"),
        ("error.rs", "UPDATE"),
        ("transaction/driver.rs", "SAVEPOINT"),
        ("transaction/driver.rs", "ROLLBACK TO SAVEPOINT"),
        ("transaction/driver.rs", "RELEASE SAVEPOINT"),
        ("crud/assignment_pass.rs", "UPDATE"),
    ];
    let baseline: BTreeSet<_> = baseline
        .into_iter()
        .map(|(f, v)| (f.to_owned(), v.to_owned()))
        .collect();
    let mut observed = BTreeSet::new();
    let mut total = 0;
    let mut vendor = 0;
    let mut shared = 0;
    let root = repo::root().join("crates/zeroship-data-orm/src");
    for (path, source) in tree("zeroship-data-orm") {
        if vendor_tier(&path) {
            continue;
        }
        let file = path.strip_prefix(&root).unwrap().to_str().unwrap();
        if file.starts_with("sql/") {
            continue;
        }
        for verb in sql_verbs(&source) {
            total += 1;
            vendor += usize::from(file == "auth/bootstrap.rs");
            observed.insert((file.to_owned(), verb.to_owned()));
        }
        if [
            "orm",
            "crud/",
            "protection",
            "search.rs",
            "executor.rs",
            "transaction/",
            "exec.rs",
            "backend_handle.rs",
            "tx_lanes.rs",
            "driver.rs",
        ]
        .iter()
        .any(|prefix| file.starts_with(prefix))
        {
            shared += 1;
            assert!(
                !source.names_any(DRIVERS)
                    && !source.contains("backend::postgres")
                    && !source.contains("backend::sqlite")
                    && !source.contains(".as_postgres(")
                    && !source.contains(".as_sqlite(")
                    && !source.contains(".get::<")
                    && !source.contains(".get_rc::<"),
                "shared execution names a driver: {file}"
            );
        }
    }
    assert!(
        shared >= 20 && total >= 4,
        "the source scan lost its corpus"
    );
    assert_eq!(
        observed, baseline,
        "SQL appeared outside its owner, or a baseline became stale"
    );
    assert!(
        total <= 31 && vendor <= 23,
        "baselined SQL grew: total={total}, vendor={vendor}"
    );
}

#[test]
fn sql_detection_uses_literals_and_preserves_longest_statement_verbs() {
    assert_eq!(
        sql_verbs(&source::parse(
            "fn q() { format!(\"SELECT id FROM users\"); }"
        )),
        ["SELECT"]
    );
    assert_eq!(
        sql_verbs(&source::parse(
            "fn q() { format!(\"ROLLBACK TO SAVEPOINT {name}\"); }"
        )),
        ["ROLLBACK TO SAVEPOINT"]
    );
    for source in [
        "// SELECT id FROM users\nfn q() {}",
        "const SELECT_LIMIT: usize = 10;",
        "fn q() { panic!(\"db.transaction: SAVEPOINT did not open\"); }",
        "#[cfg(test)] fn q() { format!(\"SELECT id FROM users\"); }",
    ] {
        assert!(sql_verbs(&super::source::parse(source)).is_empty());
    }
}
