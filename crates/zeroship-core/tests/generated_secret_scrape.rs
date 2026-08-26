//! `deploy/scripts/deploy-remote.sh` reads the generated-secret names out of
//! `crates/core/src/config/secrets.rs` with sed. This asserts that the
//! extraction still yields the table.
//!
//! WHY A SHELL SCRIPT IS STILL ALLOWED TO SCRAPE SOURCE. `deploy-remote.sh`
//! runs on an operator's machine and builds a docker image; it has no Rust
//! toolchain requirement today, and adding `cargo run` to a deploy path to
//! read eight strings is a worse trade than this test.
//!
//! WHAT MAKES IT SAFE, which the pattern alone does not. The script's previous
//! extraction read `crates/cli/src/dev.rs` for ANY `"NAME",` line. That
//! matched `ENV_KEYS`, and it matched nothing at all the moment that const was
//! deleted on 2026-08-20 in favour of the shared table - which would have set
//! GENERATED to empty and sent every generated secret down the "operator must
//! supply this" arm on a fresh host, or worse, past the rename guard. The
//! failure was SILENT: an empty `sed` and a healthy one both print nothing to
//! stderr.
//!
//! So the extraction is reproduced here, character for character, and pinned
//! to `PLATFORM_SECRETS`. If the table's spelling changes, this goes red and
//! names the script.

use std::path::Path;

use zeroship_core::config::PLATFORM_SECRETS;

const SECRETS_RS: &str = "src/config/secrets.rs";
const SCRIPT: &str = "../../deploy/scripts/deploy-remote.sh";

/// The Rust twin of `sed -n 's/^ *env: "\([A-Z_]*\)",$/\1/p'`.
fn scrape(source: &str) -> Vec<String> {
    let mut names: Vec<String> = source
        .lines()
        .filter_map(|line| {
            let body = line.trim_start_matches(' ');
            // `trim_start_matches` only strips spaces, so a tab-indented line
            // is rejected here exactly as sed's `^ *` would reject it.
            let rest = body.strip_prefix("env: \"")?.strip_suffix("\",")?;
            rest.bytes()
                .all(|b| b.is_ascii_uppercase() || b == b'_')
                .then(|| rest.to_owned())
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

#[test]
fn the_deploy_scripts_sed_still_yields_the_whole_table() {
    let source = std::fs::read_to_string(Path::new(SECRETS_RS))
        .unwrap_or_else(|e| panic!("read {SECRETS_RS}: {e}"));

    let mut expected: Vec<String> = PLATFORM_SECRETS
        .iter()
        .map(|secret| secret.env.to_owned())
        .collect();
    expected.sort_unstable();
    expected.dedup();
    assert!(!expected.is_empty(), "the table is empty");

    assert_eq!(
        scrape(&source),
        expected,
        "the sed in {SCRIPT} no longer extracts PLATFORM_SECRETS. Either restore the \
         `    env: \"NAME\",` spelling in {SECRETS_RS} or change the script's pattern to match."
    );

    // The script must still be RUNNING that pattern. Asserting the extraction
    // works while the script scrapes somewhere else entirely would be a test
    // of this file and nothing more.
    let script = std::fs::read_to_string(Path::new(SCRIPT))
        .unwrap_or_else(|e| panic!("read {SCRIPT}: {e}"));
    assert!(
        script.contains(r#"sed -n 's/^ *env: "\([A-Z_]*\)",$/\1/p' crates/core/src/config/secrets.rs"#),
        "{SCRIPT} no longer runs the extraction this test reproduces; update both together"
    );
}

/// The one-variable partner: the extraction must DISCRIMINATE, not merely
/// return a list. Source with no table yields nothing, which is precisely the
/// silent-empty case the script now refuses on.
#[test]
fn the_extraction_returns_nothing_when_the_table_is_absent() {
    assert!(scrape("fn main() {}\n").is_empty());
    assert!(
        scrape("    \"ZEROSHIP_WORKER_KEY\",\n").is_empty(),
        "the OLD pattern's shape must not match; that looseness is what went blind"
    );
    assert_eq!(
        scrape("    env: \"ZEROSHIP_WORKER_KEY\",\n"),
        vec!["ZEROSHIP_WORKER_KEY".to_owned()]
    );
}
