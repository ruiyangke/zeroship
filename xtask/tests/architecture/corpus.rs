use super::repo;
use regex::Regex;
use std::collections::BTreeSet;
use std::path::PathBuf;

const CODE: &str = r"(?i)(^|[^[:alnum:]])fts(5)?([^[:alnum:]]|$)|ftsLanguage|tsvector|fulltextindex|full[-_]text|(^|[^[:alnum:]])_rank\??\s*:|\$search";
const EXTENSIONS: &[&str] = &["rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"];

fn executable_lines(input: &str, attributes: bool) -> String {
    input
        .lines()
        .filter(|line| {
            let line = line.trim();
            !["//", "/*", "*"]
                .iter()
                .any(|prefix| line.starts_with(prefix))
                && (attributes || !(line.starts_with("#[") || line.starts_with("#![")))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn scan(files: Vec<PathBuf>, floor: usize, pattern: &Regex, attributes: Option<bool>) {
    assert!(
        files.len() >= floor,
        "source arm lost its corpus: {} < {floor}",
        files.len()
    );
    let mut findings = Vec::new();
    for file in files {
        let body = std::fs::read_to_string(&file).unwrap();
        let body = attributes.map_or_else(|| body.clone(), |attrs| executable_lines(&body, attrs));
        if pattern.is_match(&body) {
            findings.push(file);
        }
    }
    assert!(
        findings.is_empty(),
        "deleted database surface remains: {findings:?}"
    );
}

#[test]
fn removed_database_search_surface_has_no_producer_or_documented_api() {
    let code = Regex::new(CODE).unwrap();
    let migration = Regex::new(&format!(r#"{CODE}|engine_goodie_ddl|GENERATED_PREFIX|pragma_name\.eq_ignore_ascii_case\("data_version"\)"#)).unwrap();
    let is_migration = |file: &PathBuf| {
        file.strip_prefix(repo::root().join("crates"))
            .unwrap()
            .components()
            .next()
            .unwrap()
            .as_os_str()
            .to_str()
            .unwrap()
            .starts_with("zeroship-migrate")
    };
    scan(
        repo::files("crates", EXTENSIONS)
            .into_iter()
            .filter(|file| {
                !is_migration(file)
                    && !file
                        .starts_with(repo::root().join("crates/zeroship-runtime/tests/fixtures"))
            })
            .collect(),
        900,
        &code,
        Some(false),
    );
    let mut ext = EXTENSIONS.to_vec();
    ext.push("json");
    scan(
        repo::files("crates", &ext)
            .into_iter()
            .filter(is_migration)
            .collect(),
        500,
        &migration,
        Some(true),
    );
    scan(
        repo::files("sdks", EXTENSIONS),
        500,
        &migration,
        Some(false),
    );
    scan(
        repo::files("packages/zero-migrate", &ext),
        30,
        &migration,
        Some(false),
    );
    scan(repo::files("examples", EXTENSIONS), 150, &code, Some(false));
    let mut docs = repo::files("docs/reference", &["md"]);
    docs.extend([
        repo::root().join("docs/feature-map.md"),
        repo::root().join("AGENTS.md"),
    ]);
    scan(docs, 20, &Regex::new(r"(?i)(^|[^[:alnum:]])fts(5)?([^[:alnum:]]|$)|ftsLanguage|tsvector|full[-_ ]text|(^|[^[:alnum:]])_rank([^[:alnum:]]|$)|\$search").unwrap(), None);
}

#[test]
fn removed_surface_scan_distinguishes_live_code_from_removal_comments() {
    let pattern = Regex::new(CODE).unwrap();
    for code in [
        "fn fts() {}",
        "type Index = FullTextIndex;",
        "const query = { $search: 'x' };",
        "rank: { _rank: number }",
        "CREATE VIRTUAL TABLE t USING fts5(body)",
    ] {
        assert!(pattern.is_match(&executable_lines(code, false)), "{code}");
    }
    for code in [
        "// Removed fts5 support",
        "//! Deleted tsvector",
        "const offsets = 1;",
        "fn vector_search() {}",
    ] {
        assert!(!pattern.is_match(&executable_lines(code, false)), "{code}");
    }
}

fn creates_postgres_database(input: &str) -> Option<bool> {
    let body = input
        .lines()
        .map(|line| line.split("//").next().unwrap())
        .collect::<Vec<_>>()
        .join(" ");
    if !Regex::new(r"(?i)CREATE\s+DATABASE")
        .unwrap()
        .is_match(&body)
    {
        return None;
    }
    let postgres = Regex::new(r"compio_postgres|compio-postgres|PgSession|PG_TEST_URL|require_pg|Pool::connect|tokio_postgres").unwrap();
    let mysql = Regex::new(r"MysqlDevSession|require_live_mysql|MYSQL_TEST_URL|MysqlBackend|mysql_ident|quote_ident_mysql").unwrap();
    Some(postgres.is_match(input) || !mysql.is_match(input))
}

#[test]
fn database_creation_stays_inside_its_explicit_fixture_owners() {
    let allow: BTreeSet<_> = [
        // The statement itself is the driver's subject.
        "libs/compio-postgres/tests/suite/integration.rs",
        // Tracked violation: workflow tests clone the live suite database.
        "crates/zeroship-control/tests/workflow_engine_test.rs",
        // The shared provisioning fixture owns database creation.
        "crates/zeroship-testkit/src/admin.rs",
        // Asserts a database-creation permission error, without executing DDL.
        "crates/zeroship-testkit/src/suite_db.rs",
    ]
    .into_iter()
    .map(|p| repo::root().join(p))
    .collect();
    let mut observed = BTreeSet::new();
    let mut examined = 0;
    let mut mysql = 0;
    for file in repo::files("crates", &["rs"])
        .into_iter()
        .chain(repo::files("libs", &["rs"]))
    {
        let input = std::fs::read_to_string(&file).unwrap();
        if !file.components().any(|c| c.as_os_str() == "tests") && !input.contains("#[cfg(test)]") {
            continue;
        }
        examined += 1;
        match creates_postgres_database(&input) {
            Some(true) => {
                observed.insert(file);
            }
            Some(false) => mysql += 1,
            None => {}
        }
    }
    assert!(
        examined >= 300 && mysql >= 10 && observed.len() >= 3,
        "creation scan lost a decision arm: examined={examined}, mysql={mysql}, observed={}",
        observed.len()
    );
    assert_eq!(
        observed, allow,
        "unowned database creation or a stale exemption"
    );
}

#[test]
fn database_creation_discriminator_fails_closed_for_unknown_drivers() {
    assert_eq!(
        creates_postgres_database("let sql = \"CREATE\n DATABASE test\";"),
        Some(true)
    );
    assert_eq!(
        creates_postgres_database("// CREATE DATABASE t\nfn f() {}"),
        None
    );
    assert_eq!(
        creates_postgres_database("MysqlDevSession::new(); execute(\"CREATE DATABASE t\");"),
        Some(false)
    );
    assert_eq!(
        creates_postgres_database(
            "MysqlDevSession::new(); PgSession::new(); execute(\"CREATE DATABASE t\");"
        ),
        Some(true)
    );
}
