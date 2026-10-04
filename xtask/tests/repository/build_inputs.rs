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

use crate::architecture::repo;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use xtask::build_chain::BUILD_CHAIN;
use zeroship_testkit::prebuilt;

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

/// The Rust the workflow process executables are compiled from: every
/// workspace crate in the normal closure of the packages the service build
/// selects, plus the lockfile and the root manifest.
fn workflow_executable_sources() -> Vec<String> {
    let workspace: BTreeSet<String> = repo::workspace()
        .iter()
        .map(|package| {
            package["name"]
                .as_str()
                .expect("workspace package name")
                .to_owned()
        })
        .collect();
    let mut names = BTreeSet::new();
    for package in [
        "zeroship-control",
        "zeroship-worker",
        "zeroship-gateway",
        "zeroship-data-cdc-server",
        "zeroship-workflow-server",
    ] {
        names.extend(
            repo::normal_closure(package)
                .into_iter()
                .filter(|name| workspace.contains(name)),
        );
    }
    assert!(
        names.contains("zeroship-control"),
        "the executable closure lost the control package"
    );
    let mut sources = vec!["Cargo.lock".to_owned(), "Cargo.toml".to_owned()];
    for name in names {
        let directory = crate_directory(&name);
        sources.push(format!("{directory}/Cargo.toml"));
        let src = format!("{directory}/src");
        if repo::root().join(&src).is_dir() {
            sources.extend(tree(&src, &["rs"]));
        }
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
        // The same crate embeds the DB SDK's PUBLISHED bundle with a second
        // `include_str!`, at src/tests/startup_policy.rs. It is a different
        // artifact from the adapter above and was declared by neither rule, so a
        // test binary could compile against an SDK bundle older than its sources
        // and answer from what the facade used to export.
        BuildInput {
            artifacts: vec!["packages/db/dist/index.js".to_owned()],
            sources: also(
                tree("packages/db/src", &["ts"]),
                &["packages/db/tsup.config.ts", "packages/schema/dist/index.js"],
            ),
            rebuild: "pnpm --filter @zeroship/db build",
        },
        // The workspace executables the control workflow process suites run.
        // They are generated under the target directory the same way the
        // bundles above are generated under the repository, and a stale one
        // answers from sources that have moved. The suites locate them and
        // refuse on a missing or stale artifact; this rule is the
        // repository-side twin for a checkout whose target directory is in the
        // repository.
        BuildInput {
            artifacts: prebuilt::ARTIFACTS
                .iter()
                .map(|path| (*path).to_owned())
                .collect(),
            sources: workflow_executable_sources(),
            rebuild: prebuilt::REBUILD,
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

/// `build_host` rewrites the schema bundle, and the freshness gate pairs every
/// declared artifact against its newest source. A build-input rule that names a
/// bundle the chain rewrites is therefore left stale by a chain that stops
/// before rebuilding it. Binding the chain's output to the table keeps the two
/// producers of these inputs in the same order.
#[test]
fn the_host_chain_rebuilds_every_consumer_of_the_artifacts_it_writes() {
    let produced: BTreeSet<&str> = BUILD_CHAIN
        .iter()
        .flat_map(|step| step.artifacts.iter().copied())
        .collect();
    assert!(
        !produced.is_empty(),
        "the host chain declares no artifacts, so this check compares nothing"
    );

    let mut consumers = 0;
    for input in build_inputs() {
        let consumed: Vec<&str> = input
            .sources
            .iter()
            .filter(|source| produced.contains(source.as_str()))
            .map(String::as_str)
            .collect();
        if consumed.is_empty() {
            continue;
        }
        consumers += 1;
        for artifact in &input.artifacts {
            assert!(
                produced.contains(artifact.as_str()),
                "`{}` reads {} - a bundle the host chain rewrites - but the \
                 chain does not rebuild its artifact {artifact}, so running it \
                 leaves {artifact} older than that source",
                input.rebuild,
                consumed.join(", "),
            );
        }
    }
    assert!(
        consumers > 0,
        "no build-input rule reads a host-chain artifact, so the closure \
         check has no subjects"
    );
}

/// The `wpt` flake input pins one upstream Web Platform Tests commit, and the
/// development shell links that input at `crates/zeroship-runtime/tests/wpt`
/// for the runtime's `wpt` test target to `include_str!`. The tree is built from
/// nothing in this repository, so the mtime rule above has nothing to say about
/// it. What it does have is the revision `flake.nix` declares and the revision
/// `flake.lock` resolved; the two must agree, or the shell links a tree nobody
/// asked for while the lock says otherwise.
fn wpt_input_rev(flake: &str) -> Option<&str> {
    let (_, rest) = flake.split_once("github:web-platform-tests/wpt/")?;
    let rev = rest.split(['"', '\n', ' ', ';']).next()?;
    (rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit())).then_some(rev)
}

#[test]
fn the_wpt_input_is_pinned_to_the_revision_the_lock_resolves() {
    let flake = repo::read("flake.nix");
    let pin = wpt_input_rev(&flake)
        .unwrap_or_else(|| panic!("flake.nix declares no wpt input pinned to a full commit"));
    let lock: Value = serde_json::from_str(&repo::read("flake.lock")).expect("parse flake.lock");
    let node = &lock["nodes"]["wpt"];
    assert_eq!(
        node["flake"], false,
        "the wpt input must stay a non-flake source tree"
    );
    assert_eq!(node["locked"]["type"], "github");
    assert_eq!(node["locked"]["owner"], "web-platform-tests");
    assert_eq!(node["locked"]["repo"], "wpt");
    assert_eq!(
        node["locked"]["rev"], pin,
        "flake.nix pins wpt at {pin} but flake.lock resolved {} - run `nix flake lock`",
        node["locked"]["rev"]
    );
    assert_eq!(
        lock["nodes"]["root"]["inputs"]["wpt"], "wpt",
        "flake.lock does not wire the wpt input into the root"
    );
}

#[test]
fn the_wpt_input_reader_rejects_a_pin_that_is_not_a_full_commit() {
    assert_eq!(
        wpt_input_rev(
            "url = \"github:web-platform-tests/wpt/e053afbbd005bed4b6100f98f0de744da8d1d09d\";\n"
        ),
        Some("e053afbbd005bed4b6100f98f0de744da8d1d09d")
    );
    assert_eq!(wpt_input_rev("nothing here\n"), None);
    assert_eq!(
        wpt_input_rev("url = \"github:web-platform-tests/wpt/master\";\n"),
        None
    );
    assert_eq!(
        wpt_input_rev("url = \"github:web-platform-tests/wpt/e053afbb\";\n"),
        None
    );
}

/// Every `packages/*/dist` bundle a crate embeds with `include_str!` must be
/// declared as a build input.
///
/// The staleness rule above compares what it is TOLD about. Nothing tells it
/// what the tree actually embeds, so an artifact can reach a compiled binary
/// without that rule ever seeing it - which is how
/// `packages/db/dist/index.js` sat undeclared in the same crate whose adapter
/// was declared for precisely this reason.
#[test]
fn every_embedded_package_bundle_is_declared_as_a_build_input() {
    let embedded = embedded_package_bundles();

    // A difference against an empty scan passes and proves nothing.
    assert!(
        !embedded.is_empty(),
        "no `include_str!` of a packages/*/dist path found under crates/: the \
         scan read the wrong tree, so a clean comparison here would be a \
         statement about nothing"
    );

    let declared: BTreeSet<String> = build_inputs()
        .iter()
        .flat_map(|input| input.artifacts.iter().cloned())
        .collect();

    let undeclared: Vec<&String> = embedded.difference(&declared).collect();

    assert!(
        undeclared.is_empty(),
        "these generated bundles are embedded with `include_str!` and declared \
         by no build-input rule, so the staleness gate is silent about them:\n  \
         {}\nDeclare each with its sources and rebuild command.",
        undeclared
            .iter()
            .map(|path| (*path).clone())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// Repository-relative `packages/*/dist/...` paths embedded by `include_str!`
/// anywhere under `crates/`.
///
/// The literal is normalised from its `packages/` segment rather than resolved
/// against the file, because the relative prefix varies by depth and only the
/// tail identifies the artifact.
fn embedded_package_bundles() -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for path in repo::files("crates", &["rs"]) {
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        for rest in source.split("include_str!(\"").skip(1) {
            let Some((literal, _)) = rest.split_once('"') else {
                continue;
            };
            let Some(index) = literal.find("packages/") else {
                continue;
            };
            let tail = &literal[index..];
            if tail.contains("/dist/") {
                found.insert(tail.to_owned());
            }
        }
    }
    found
}

/// The root `build:test-artifacts` script must run the same cargo invocation
/// the control workflow test areas run, so the build-input gate's rebuild
/// command and the test-side failure both name one command.
#[test]
fn the_root_build_script_runs_the_test_artifact_build() {
    let manifest: Value =
        serde_json::from_str(&repo::read("package.json")).expect("parse package.json");
    let script = manifest["scripts"]["build:test-artifacts"]
        .as_str()
        .expect("the root package declares build:test-artifacts");
    assert_eq!(
        script,
        prebuilt::build_command(),
        "the root script and the test areas must name one cargo invocation"
    );
}
