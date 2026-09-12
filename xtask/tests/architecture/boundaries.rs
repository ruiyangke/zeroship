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
    for forbidden in ["zeroship-runtime", "zeroship-data-v8", "v8"] {
        assert!(!closure.contains(forbidden), "ORM reaches {forbidden}");
    }
    assert!(
        closure.contains("compio-postgres"),
        "the dependency detector must find the ORM's real driver"
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
