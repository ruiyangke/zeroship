//! Generated files that other build steps, code generators and Rust compile
//! units read as INPUT.
//!
//! Every one of them is gitignored, so a fresh checkout has none of them and an
//! older checkout can hold one built from sources that have since moved. Unlike
//! a missing artifact, a stale one does not error: it answers, confidently and
//! wrongly, and the blame lands on whatever consumed it. The napi addon is the
//! sharpest case - a stale compiler reports a CORRECT schema as stale, and
//! "regenerating" writes its old output over the right answer.
//!
//! A generator's own `--check` mode cannot see this. It compares an artifact
//! against what the generator would produce now, and the generator is the thing
//! that went stale.

use super::repo;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// The ordered chain that rebuilds every artifact below in one command.
const WHOLE_CHAIN: &str = "pnpm build";

/// One generated build input: the paths it is written to, the paths it is
/// written FROM, and the command that rewrites it.
struct BuildInput {
    artifacts: Vec<String>,
    sources: Vec<String>,
    rebuild: &'static str,
}

fn relative(path: &Path) -> String {
    path.strip_prefix(repo::root())
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Every source file under `directory`, as repository-relative paths.
///
/// This walker REFUSES a generated directory where the shared one skips it: a
/// declared source tree that grows a `dist` would otherwise shrink the set the
/// comparison runs over, silently and in the direction that passes.
fn tree(directory: &str, extensions: &[&str]) -> Vec<String> {
    fn walk(directory: &Path, extensions: &[&str], output: &mut Vec<PathBuf>) {
        let entries = std::fs::read_dir(directory)
            .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()));
        for entry in entries {
            let entry = entry.expect("directory entry");
            let path = entry.path();
            if entry.file_type().expect("file type").is_dir() {
                assert!(
                    !matches!(
                        entry.file_name().to_str(),
                        Some("dist" | "node_modules" | "target")
                    ),
                    "declared source tree holds a generated directory: {}",
                    path.display()
                );
                walk(&path, extensions, output);
            } else if path
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|extension| extensions.contains(&extension))
            {
                output.push(path);
            }
        }
    }
    let root = repo::root().join(directory);
    assert!(
        root.is_dir(),
        "build-input source tree {directory} does not exist"
    );
    let mut files = Vec::new();
    walk(&root, extensions, &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "build-input source tree {directory} holds no {extensions:?} file"
    );
    files.into_iter().map(|path| relative(&path)).collect()
}

fn also(mut sources: Vec<String>, extra: &[&str]) -> Vec<String> {
    sources.extend(extra.iter().map(|path| (*path).to_owned()));
    sources
}

/// The `.node` addons present for this platform. `napi build` names the file
/// after the host triple, so the set is discovered rather than declared.
fn node_addons() -> Vec<String> {
    let directory = repo::root().join("crates/zeroship-migrate-node");
    let mut addons: Vec<String> = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| path.extension().and_then(|v| v.to_str()) == Some("node"))
        .map(|path| relative(&path))
        .collect();
    addons.sort();
    addons
}

fn crate_directory(name: &str) -> String {
    let manifest = repo::package(name)["manifest_path"]
        .as_str()
        .expect("manifest path");
    relative(
        Path::new(manifest)
            .parent()
            .expect("a manifest has a directory"),
    )
}

/// The Rust the addon is compiled from: its own crate plus every workspace
/// crate in its normal dependency closure - the migration compiler itself.
/// Registry crates are left out; a version bump moves `Cargo.lock`, not these.
fn migration_compiler_sources() -> Vec<String> {
    let workspace: BTreeSet<String> = repo::workspace()
        .iter()
        .map(|package| {
            package["name"]
                .as_str()
                .expect("workspace package name")
                .to_owned()
        })
        .collect();
    let members: Vec<String> = repo::normal_closure("zeroship-migrate-node")
        .into_iter()
        .filter(|name| workspace.contains(name))
        .collect();
    assert!(
        members.contains(&"zeroship-migrate".to_owned()),
        "the addon closure lost the migration compiler"
    );
    let mut sources = Vec::new();
    for name in members {
        let directory = crate_directory(&name);
        sources.push(format!("{directory}/Cargo.toml"));
        sources.extend(tree(&format!("{directory}/src"), &["rs"]));
        let build_script = format!("{directory}/build.rs");
        if repo::root().join(&build_script).is_file() {
            sources.push(build_script);
        }
    }
    sources
}

fn build_inputs() -> Vec<BuildInput> {
    vec![
        // The shared lexicon. Both the migration DSL and the DB adapter inline
        // it, so its staleness is inherited by everything below.
        BuildInput {
            artifacts: vec!["packages/schema/dist/index.js".to_owned()],
            sources: also(
                tree("packages/schema/src", &["ts"]),
                &["packages/schema/tsup.config.ts"],
            ),
            rebuild: "pnpm --filter @zeroship/schema build",
        },
        // The authoring DSL and the recorder every schema generator drives.
        // `noExternal` inlines the lexicon, which is why its dist is a source.
        BuildInput {
            artifacts: vec![
                "packages/zero-migrate/dist/index.js".to_owned(),
                "packages/zero-migrate/dist/internal/recorder.js".to_owned(),
                "packages/zero-migrate/dist/embedded-recorder.js".to_owned(),
            ],
            sources: also(
                tree("packages/zero-migrate/src", &["ts"]),
                &[
                    "packages/zero-migrate/tsup.config.ts",
                    "packages/schema/dist/index.js",
                ],
            ),
            rebuild: "pnpm --filter @zeroship/migrate build",
        },
        // The host runtime the generators take `previewSql` and
        // `currentIrVersion` from. It keeps `@zeroship/migrate` external, so the
        // DSL is a runtime resolution rather than a bundled source.
        BuildInput {
            artifacts: vec!["packages/zero-migrate-cli/dist/index.js".to_owned()],
            sources: also(
                tree("packages/zero-migrate-cli/src", &["ts"]),
                &["packages/zero-migrate-cli/tsup.config.ts"],
            ),
            rebuild: "pnpm --filter zero-migrate-cli build",
        },
        // The migration compiler as the generators call it. When this predates
        // the compiler crates, `generate.mjs --check` reports the ARTIFACT
        // stale and regenerating overwrites correct output with old.
        BuildInput {
            artifacts: node_addons(),
            sources: migration_compiler_sources(),
            rebuild: "pnpm --filter zeroship-migrate-node build",
        },
        // The DB facade `zeroship-data-v8` embeds with `include_str!`. It
        // bundles the crate's own TypeScript, the SDK source it imports by
        // relative path, and the lexicon behind that.
        BuildInput {
            artifacts: vec!["crates/zeroship-data-v8/dist/adapter.js".to_owned()],
            sources: also(
                [
                    tree("crates/zeroship-data-v8/js", &["ts"]),
                    tree("packages/db/src", &["ts"]),
                ]
                .concat(),
                &[
                    "packages/db/tsup.adapter.config.ts",
                    "packages/schema/dist/index.js",
                ],
            ),
            rebuild: "pnpm build:data-v8-adapter",
        },
    ]
}

fn modified(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or_else(|error| panic!("modification time of {}: {error}", path.display()))
}

/// Declared sources, stamped. A path that no longer resolves fails here rather
/// than quietly shrinking the set the comparison runs over.
fn source_stamps(sources: &[String]) -> Vec<(String, SystemTime)> {
    sources
        .iter()
        .map(|source| {
            let path = repo::root().join(source);
            assert!(
                path.is_file(),
                "declared build-input source {source} does not exist"
            );
            (source.clone(), modified(&path))
        })
        .collect()
}

/// Declared artifacts that have actually been built. An absent one is a
/// different condition from a stale one and is left to the build: every
/// consumer already fails on it by name, and a fresh checkout has none.
fn artifact_stamps(artifacts: &[String]) -> Vec<(String, SystemTime)> {
    artifacts
        .iter()
        .filter_map(|artifact| {
            let path = repo::root().join(artifact);
            path.is_file().then(|| (artifact.clone(), modified(&path)))
        })
        .collect()
}

/// The oldest artifact and the newest source, when the source outlives it.
///
/// Oldest against newest is the conservative pairing: a half-rewritten output
/// directory is as stale as an untouched one. Equal stamps pass - a build reads
/// its sources before it writes, so it can only tie, never lose.
fn stale(
    artifacts: &[(String, SystemTime)],
    sources: &[(String, SystemTime)],
) -> Option<(String, String)> {
    let artifact = artifacts.iter().min_by_key(|(_, stamp)| *stamp)?;
    let source = sources.iter().max_by_key(|(_, stamp)| *stamp)?;
    (source.1 > artifact.1).then(|| (artifact.0.clone(), source.0.clone()))
}

#[test]
fn generated_build_inputs_outlive_the_sources_they_are_generated_from() {
    let inputs = build_inputs();
    assert!(inputs.len() >= 5, "the build-input table lost its rules");
    let mut violations = Vec::new();
    for input in &inputs {
        let sources = source_stamps(&input.sources);
        assert!(
            !sources.is_empty(),
            "no sources declared for `{}`",
            input.rebuild
        );
        let artifacts = artifact_stamps(&input.artifacts);
        if artifacts.is_empty() {
            continue;
        }
        if let Some((artifact, source)) = stale(&artifacts, &sources) {
            violations.push(format!(
                "  {artifact}\n    is older than {source}\n    rebuild: {}",
                input.rebuild
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "generated build inputs predate their sources. Each one answers from \
         what its sources used to say, so the failure it causes names the \
         consumer, not itself:\n{}\nRebuild the whole ordered chain with \
         `{WHOLE_CHAIN}` from the repository root. A branch switch that \
         rewrites a source with the content it already had trips this too; the \
         same command clears it.",
        violations.join("\n")
    );
}

#[test]
fn staleness_pairs_the_oldest_artifact_with_the_newest_source() {
    let at = |seconds| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds);
    let stamp = |name: &str, seconds| (name.to_owned(), at(seconds));
    let artifacts = [stamp("dist/late.js", 20), stamp("dist/early.js", 10)];

    let fresh = [stamp("src/a.ts", 4), stamp("src/b.ts", 9)];
    assert_eq!(stale(&artifacts, &fresh), None);

    // A source newer than the OLDEST artifact is enough, even though the
    // newest artifact outlives it.
    let partial = [stamp("src/a.ts", 4), stamp("src/b.ts", 11)];
    assert_eq!(
        stale(&artifacts, &partial),
        Some(("dist/early.js".to_owned(), "src/b.ts".to_owned()))
    );

    // A tie is what a build that reads then writes within one tick produces.
    assert_eq!(stale(&artifacts, &[stamp("src/b.ts", 10)]), None);

    // Unbuilt is not stale, and neither is a rule with nothing to compare.
    assert_eq!(stale(&[], &partial), None);
    assert_eq!(stale(&artifacts, &[]), None);
}

#[test]
fn every_declared_build_input_names_a_rebuild_command_and_resolvable_sources() {
    for input in build_inputs() {
        assert!(
            input.rebuild.starts_with("pnpm "),
            "{} is not a runnable rebuild command",
            input.rebuild
        );
        assert!(
            !input.sources.is_empty(),
            "no sources declared for `{}`",
            input.rebuild
        );
        for source in &input.sources {
            assert!(
                repo::root().join(source).is_file(),
                "`{}` declares a source that does not exist: {source}",
                input.rebuild
            );
            assert!(
                !input.artifacts.contains(source),
                "`{}` lists {source} as both artifact and source",
                input.rebuild
            );
        }
    }
}

/// The WPT tree under `crates/zeroship-runtime/tests/wpt` is FETCHED, not
/// generated: `setup-wpt.sh` shallow-clones one pinned upstream commit into a
/// gitignored directory with no tracked file in it. It is built from nothing in
/// this repository, so the mtime rule above has nothing to say about it. What
/// it does have is the pin the script declares, and a tree that must sit on it.
fn wpt_pin(script: &str) -> Option<&str> {
    let (_, rest) = script.split_once("WPT_COMMIT:-")?;
    let (pin, _) = rest.split_once('}')?;
    (!pin.is_empty() && pin.chars().all(|c| c.is_ascii_hexdigit())).then_some(pin)
}

#[test]
fn the_fetched_wpt_tree_sits_on_the_pin_its_setup_script_declares() {
    let script = "crates/zeroship-runtime/tests/setup-wpt.sh";
    let source = repo::read(script);
    let pin = wpt_pin(&source).unwrap_or_else(|| panic!("{script} declares no WPT_COMMIT default"));
    let tree = repo::root().join("crates/zeroship-runtime/tests/wpt");
    if !tree.join(".git").exists() {
        return;
    }
    let output = Command::new("git")
        .current_dir(&tree)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git rev-parse");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let head = String::from_utf8(output.stdout).expect("utf-8 revision");
    assert_eq!(
        head.trim(),
        pin,
        "the WPT tree is at a different commit from the pin {script} declares, \
         so the wpt test target compiles against vectors nobody asked for. \
         Refetch with `{script}`"
    );
}

#[test]
fn the_wpt_pin_reader_rejects_a_script_that_declares_none() {
    assert_eq!(
        wpt_pin("WPT_COMMIT=\"${WPT_COMMIT:-e053afbbd005bed4}\"\n"),
        Some("e053afbbd005bed4")
    );
    assert_eq!(wpt_pin("WPT_COMMIT=\"${WPT_COMMIT}\"\n"), None);
    assert_eq!(wpt_pin("WPT_COMMIT=\"${WPT_COMMIT:-}\"\n"), None);
    assert_eq!(wpt_pin("WPT_COMMIT=\"${WPT_COMMIT:-$OTHER}\"\n"), None);
    assert_eq!(wpt_pin("nothing here\n"), None);
}
