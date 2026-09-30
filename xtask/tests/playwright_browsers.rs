//! The workspace's Playwright against the browsers the flake provides.
//!
//! A Playwright release launches the browser builds its own `playwright-core`
//! `browsers.json` names, and looks each one up as a `<name>-<revision>`
//! directory under `PLAYWRIGHT_BROWSERS_PATH`. The browsers Playwright downloads
//! for itself do not load their libraries on NixOS, so the development shell
//! (`flake.nix`) exports its `playwright-driver`'s browsers there instead, and
//! the repository's Playwright suites run under `nix develop`. The workspace
//! therefore pins Playwright once, in the `catalog` of `pnpm-workspace.yaml`, at
//! that driver's version, and a `flake.lock` bump that moves the driver has to
//! move the catalog in the same change. Nothing else fails when the two part:
//! every browser launch dies with `Executable doesn't exist`, and CI runs no
//! browser suite. The auth UI suite is the exception that needs none of this: it
//! runs the shell's own `playwright`, which matches the shell's browsers by
//! construction.
//!
//! Three checks, each naming its fix:
//! - every workspace package declares Playwright as `catalog:`, read from the
//!   lockfile's importers;
//! - the catalog's version is the version of the driver the development shell
//!   carries, read from the lockfile and the shell's definition, with no install
//!   needed;
//! - each installed `playwright-core`, found the way Node finds it from each
//!   declaring package, is the version the lockfile resolves and names only
//!   builds the shell's browsers hold.
//!
//! The shell is read with `nix eval`, not from the environment of whichever
//! shell runs the check. That needs Nix (the check enables the `nix-command`
//! and `flakes` features itself), a completed `pnpm install`, and the shell's
//! browsers in the store, which entering `nix develop` builds.
//! `cargo xtask test playwright-browsers` runs this target, and CI runs that area
//! inside the development shell.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The builds a suite's launch loads. `chromium.launch()` starts
/// `chromium-headless-shell` headless and `chromium` headed
/// (`playwright-core/lib/server/chromium/chromium.js`, `getExecutableName`),
/// and a context that records video encodes it with `ffmpeg`
/// (`playwright-core/lib/server/videoRecorder.js`, `VideoRecorder.start`).
const LAUNCHED: &[&str] = &["chromium", "chromium-headless-shell", "ffmpeg"];

/// The packages a workspace package declares Playwright through, each with the
/// path from it to the `playwright-core` it loads. The first of each is also a
/// `catalog` entry.
const CHAINS: &[&[&str]] = &[
    &["@playwright/test", "playwright", "playwright-core"],
    &["playwright", "playwright-core"],
];

/// The specifiers a workspace package may declare Playwright with: the default
/// catalog, which pnpm also spells `catalog:default`.
const CATALOG: &str = "catalog:";
const DEFAULT_CATALOG: &str = "catalog:default";

/// Every `nix` invocation enables what it uses, so a Nix installed with its
/// default configuration runs the check.
const NIX_FEATURES: [&str; 2] = ["--extra-experimental-features", "nix-command flakes"];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the repository")
        .to_owned()
}

/// One workspace package's declaration of Playwright, as the lockfile records it.
#[derive(Debug)]
struct Declaration {
    importer: String,
    chain: &'static [&'static str],
    specifier: String,
    version: String,
}

/// One installed `playwright-core` and the workspace packages that load it.
#[derive(Debug)]
struct Install {
    version: String,
    /// The directory names its registry looks the [`LAUNCHED`] builds up by.
    builds: Vec<String>,
    importers: BTreeSet<String>,
}

/// The development shell's `playwright-driver` version and the browsers
/// directory the shell exports as `PLAYWRIGHT_BROWSERS_PATH`.
#[derive(Debug)]
struct Flake {
    version: String,
    browsers: PathBuf,
}

fn parse_lockfile(text: &str) -> serde_yaml::Value {
    serde_yaml::from_str(text).expect("parse pnpm-lock.yaml")
}

fn lockfile() -> serde_yaml::Value {
    let path = root().join("pnpm-lock.yaml");
    parse_lockfile(
        &std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
    )
}

/// Every declaration of Playwright among the lockfile's importers. None at all
/// is refused: the checks below would then pass over nothing.
fn declarations(lockfile: &serde_yaml::Value) -> Result<Vec<Declaration>, String> {
    let importers = lockfile["importers"]
        .as_mapping()
        .ok_or("pnpm-lock.yaml has no importers")?;
    let mut found = Vec::new();
    for (importer, entry) in importers {
        let importer = importer
            .as_str()
            .ok_or("an importer is named by its path")?;
        for kind in ["dependencies", "devDependencies", "optionalDependencies"] {
            let Some(dependencies) = entry[kind].as_mapping() else {
                continue;
            };
            for &chain in CHAINS {
                let Some(dependency) = dependencies.get(chain[0]) else {
                    continue;
                };
                let field = |name: &str| {
                    dependency[name]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("{importer}: {} has no {name}", chain[0]))
                };
                found.push(Declaration {
                    importer: importer.to_owned(),
                    chain,
                    specifier: field("specifier")?,
                    version: field("version")?,
                });
            }
        }
    }
    if found.is_empty() {
        return Err(format!(
            "no importer in pnpm-lock.yaml declares {}: the scan read the wrong lockfile",
            CHAINS
                .iter()
                .map(|chain| chain[0])
                .collect::<Vec<_>>()
                .join(" or ")
        ));
    }
    Ok(found)
}

/// The declarations that pin a version of their own, reported with the fix.
fn uncatalogued(declarations: &[Declaration]) -> Option<String> {
    let pinned: Vec<String> = declarations
        .iter()
        .filter(|declaration| ![CATALOG, DEFAULT_CATALOG].contains(&declaration.specifier.as_str()))
        .map(|declaration| {
            format!(
                "  {}: {} \"{}\"",
                declaration.importer, declaration.chain[0], declaration.specifier
            )
        })
        .collect();
    (!pinned.is_empty()).then(|| {
        format!(
            "these workspace packages declare Playwright with a version of their own \
             instead of the catalog's:\n{}\n\
             Fix: declare each as \"{CATALOG}\", then run `pnpm install`.",
            pinned.join("\n")
        )
    })
}

/// The version the lockfile's default catalog resolves for each package a
/// workspace package declares Playwright through.
fn catalogued(lockfile: &serde_yaml::Value) -> Result<BTreeMap<&'static str, String>, String> {
    CHAINS
        .iter()
        .map(|chain| {
            let version = lockfile["catalogs"]["default"][chain[0]]["version"]
                .as_str()
                .ok_or_else(|| {
                    format!(
                        "the default catalog in pnpm-lock.yaml has no {} entry.\n\
                         Fix: add it to the `catalog` of pnpm-workspace.yaml at the \
                         development shell's playwright-driver version, then run \
                         `pnpm install`.",
                        chain[0]
                    )
                })?;
            Ok((chain[0], version.to_owned()))
        })
        .collect()
}

/// The catalog entries on another version than the shell's driver, reported
/// with both versions and the fix.
fn version_skew(catalog: &BTreeMap<&'static str, String>, flake: &Flake) -> Option<String> {
    let skewed: Vec<String> = catalog
        .iter()
        .filter(|(_, version)| **version != flake.version)
        .map(|(package, version)| format!("{package} {version}"))
        .collect();
    (!skewed.is_empty()).then(|| {
        format!(
            "the catalog pins {}, and the development shell's playwright-driver is {}, \
             whose browsers it exports as PLAYWRIGHT_BROWSERS_PATH ({}).\n\
             Fix: set the Playwright version in the `catalog` of pnpm-workspace.yaml to \
             {}, then run `pnpm install`.",
            skewed.join(", "),
            flake.version,
            flake.browsers.display(),
            flake.version
        )
    })
}

fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn manifest_version(directory: &Path) -> String {
    let manifest = directory.join("package.json");
    read_json(&manifest)["version"]
        .as_str()
        .unwrap_or_else(|| panic!("{} has no version", manifest.display()))
        .to_owned()
}

/// Node's lookup: the nearest `node_modules/<name>` at or above `from`, with its
/// links resolved so that the next lookup starts where the package really is.
fn resolve(from: &Path, name: &str) -> Option<PathBuf> {
    let found = from
        .ancestors()
        .map(|directory| directory.join("node_modules").join(name))
        .find(|candidate| candidate.join("package.json").is_file())?;
    Some(
        found
            .canonicalize()
            .unwrap_or_else(|error| panic!("resolve {}: {error}", found.display())),
    )
}

/// The directory Playwright's registry looks each [`LAUNCHED`] build up in:
/// the browser name with its dashes spelled as underscores, a dash, and the
/// revision (`playwright-core/lib/server/registry/index.js`,
/// `readDescriptors`).
fn builds(browsers: &Value, source: &Path) -> Vec<String> {
    let entries = browsers["browsers"]
        .as_array()
        .unwrap_or_else(|| panic!("{} lists no browsers", source.display()));
    LAUNCHED
        .iter()
        .map(|name| {
            let entry = entries
                .iter()
                .find(|entry| entry["name"] == *name)
                .unwrap_or_else(|| panic!("{} names no {name} build", source.display()));
            let revision = entry["revision"]
                .as_str()
                .unwrap_or_else(|| panic!("{} gives {name} no revision", source.display()));
            format!("{}-{revision}", name.replace('-', "_"))
        })
        .collect()
}

/// The version and builds of the `playwright-core` installed at `directory`.
fn install(directory: &Path) -> Install {
    let browsers = directory.join("browsers.json");
    Install {
        version: manifest_version(directory),
        builds: builds(&read_json(&browsers), &browsers),
        importers: BTreeSet::new(),
    }
}

/// Every `playwright-core` the declaring packages under `root` load, found the
/// way Node finds it from each package.
///
/// A declaration with nothing installed behind it, or with another version
/// installed than the lockfile resolves, is refused: the install is stale, and
/// `pnpm install` is the fix.
fn installed(root: &Path, declarations: &[Declaration]) -> Result<Vec<Install>, String> {
    let mut stale = Vec::new();
    let mut installs: BTreeMap<PathBuf, Install> = BTreeMap::new();
    for declaration in declarations {
        let importer = &declaration.importer;
        let package = declaration.chain[0];
        let Some(direct) = resolve(&root.join(importer), package) else {
            stale.push(format!("  {importer}: {package} is not installed"));
            continue;
        };
        let version = manifest_version(&direct);
        if version != declaration.version {
            stale.push(format!(
                "  {importer}: {package} {version} is installed, the lockfile resolves {}",
                declaration.version
            ));
            continue;
        }
        let Some(core) = declaration.chain[1..]
            .iter()
            .try_fold(direct, |at, name| resolve(&at, name))
        else {
            stale.push(format!("  {importer}: {package} has no playwright-core"));
            continue;
        };
        installs
            .entry(core.clone())
            .or_insert_with(|| install(&core))
            .importers
            .insert(importer.clone());
    }
    if stale.is_empty() {
        Ok(installs.into_values().collect())
    } else {
        Err(format!(
            "node_modules does not hold the Playwright pnpm-lock.yaml resolves:\n{}\n\
             Fix: run `pnpm install` from the repository root.",
            stale.join("\n")
        ))
    }
}

fn nix_eval(arguments: &[&str]) -> String {
    let output = Command::new("nix")
        .current_dir(root())
        .arg("eval")
        .args(NIX_FEATURES)
        .args(arguments)
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "spawn `nix`: {error}. This check reads the development shell's \
                 Playwright driver from flake.nix, so it needs Nix installed \
                 (https://nixos.org/download); it enables the `nix-command` and \
                 `flakes` features itself."
            )
        });
    assert!(
        output.status.success(),
        "`nix eval {}` failed, so there is no driver to check the workspace's \
         Playwright against:\n{}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).expect("`nix eval` prints UTF-8")
}

/// The development shell's `playwright-driver` version and exported browsers
/// path, both read from the shell's own definition for the system Nix builds
/// for.
fn flake() -> &'static Flake {
    static FLAKE: OnceLock<Flake> = OnceLock::new();
    FLAKE.get_or_init(|| {
        let system = nix_eval(&["--impure", "--raw", "--expr", "builtins.currentSystem"]);
        let shell: Value = serde_json::from_str(&nix_eval(&[
            "--json",
            &format!(".#devShells.{system}.default"),
            "--apply",
            "shell: { version = shell.playwright-driver.version; \
             browsers = shell.PLAYWRIGHT_BROWSERS_PATH; }",
        ]))
        .expect("parse the development shell's Playwright facts");
        let field = |name: &str| {
            shell[name]
                .as_str()
                .unwrap_or_else(|| panic!("the development shell gave no {name}: {shell}"))
                .to_owned()
        };
        Flake {
            version: field("version"),
            browsers: PathBuf::from(field("browsers")),
        }
    })
}

/// The directory names under the shell's browsers path.
fn held(flake: &Flake) -> BTreeSet<String> {
    std::fs::read_dir(&flake.browsers)
        .unwrap_or_else(|error| {
            panic!(
                "read the development shell's browsers at {}: {error}. Entering \
                 `nix develop` builds them.",
                flake.browsers.display()
            )
        })
        .map(|entry| entry.expect("browsers directory entry").path())
        .filter(|path| path.is_dir())
        .map(|path| {
            path.file_name()
                .expect("a directory entry has a name")
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// A report naming every install whose builds the shell's browsers lack, both
/// versions and the fix, or `None` when every build is there.
fn mismatch(installs: &[Install], flake: &Flake) -> Option<String> {
    let held = held(flake);
    let lines: Vec<String> = installs
        .iter()
        .filter(|install| install.builds.iter().any(|build| !held.contains(build)))
        .map(|install| {
            let builds: Vec<String> = install
                .builds
                .iter()
                .map(|build| {
                    if held.contains(build) {
                        build.clone()
                    } else {
                        format!("{build} (missing)")
                    }
                })
                .collect();
            let fix = if install.version == flake.version {
                "Fix: the shell's browsers lack a build its own driver names, so flake.nix \
                 has to provide them."
                    .to_owned()
            } else {
                format!(
                    "Fix: set the Playwright version in the `catalog` of \
                     pnpm-workspace.yaml to {}, then run `pnpm install`.",
                    flake.version
                )
            };
            format!(
                "  playwright-core {}, loaded by {}, launches {}\n  {fix}",
                install.version,
                install
                    .importers
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
                builds.join(", ")
            )
        })
        .collect();
    (!lines.is_empty()).then(|| {
        format!(
            "the installed Playwright launches browser builds the development shell's \
             browsers do not hold, so under `nix develop` every browser launch fails \
             with `Executable doesn't exist`:\n\
             {}\n\
             The development shell's playwright-driver is {}. Its browsers, exported as \
             PLAYWRIGHT_BROWSERS_PATH, are {} and hold {}.",
            lines.join("\n"),
            flake.version,
            flake.browsers.display(),
            held.iter().cloned().collect::<Vec<_>>().join(", "),
        )
    })
}

fn workspace_declarations() -> Vec<Declaration> {
    declarations(&lockfile()).unwrap_or_else(|error| panic!("{error}"))
}

#[test]
fn every_workspace_package_takes_playwright_from_the_catalog() {
    if let Some(report) = uncatalogued(&workspace_declarations()) {
        panic!("{report}");
    }
}

#[test]
fn the_catalog_pins_the_development_shell_driver_version() {
    let catalog = catalogued(&lockfile()).unwrap_or_else(|error| panic!("{error}"));
    if let Some(report) = version_skew(&catalog, flake()) {
        panic!("{report}");
    }
}

#[test]
fn the_installed_playwright_launches_builds_the_shell_browsers_hold() {
    let installs =
        installed(&root(), &workspace_declarations()).unwrap_or_else(|error| panic!("{error}"));
    if let Some(report) = mismatch(&installs, flake()) {
        panic!("{report}");
    }
}

/// A lockfile whose catalog is on 1.58.2, with packages taking it under both
/// spellings of the default catalog, one taking a named catalog, and one
/// pinning its own version.
const FIXTURE_LOCKFILE: &str = r"
lockfileVersion: '9.0'
catalogs:
  default:
    '@playwright/test':
      specifier: 1.58.2
      version: 1.58.2
    playwright:
      specifier: 1.58.2
      version: 1.58.2
importers:
  .: {}
  examples/catalogued:
    devDependencies:
      playwright:
        specifier: 'catalog:'
        version: 1.58.2
  examples/default-catalog:
    dependencies:
      '@playwright/test':
        specifier: 'catalog:default'
        version: 1.58.2
  examples/named-catalog:
    devDependencies:
      playwright:
        specifier: 'catalog:react18'
        version: 1.58.2
  examples/pinned:
    devDependencies:
      '@playwright/test':
        specifier: 1.58.2
        version: 1.58.2
";

fn assert_names(report: &str, expected: &[String]) {
    for expected in expected {
        assert!(
            report.contains(expected.as_str()),
            "the report lacks {expected:?}:\n{report}"
        );
    }
}

#[test]
fn a_package_pinning_its_own_playwright_version_is_refused() {
    let found = declarations(&parse_lockfile(FIXTURE_LOCKFILE)).expect("declarations");
    assert_eq!(found.len(), 4, "{found:?}");
    let report = uncatalogued(&found).expect("a literal version passed");
    assert_names(
        &report,
        &[
            "examples/pinned: @playwright/test \"1.58.2\"".to_owned(),
            // A named catalog is not the one whose version the shell's driver
            // is checked against.
            "examples/named-catalog: playwright \"catalog:react18\"".to_owned(),
            "Fix: declare each as \"catalog:\"".to_owned(),
        ],
    );
    for taken in ["examples/catalogued", "examples/default-catalog"] {
        assert!(
            !report.contains(taken),
            "{taken} takes the default catalog and was reported:\n{report}"
        );
    }
    let default_only: Vec<Declaration> = found
        .into_iter()
        .filter(|declaration| {
            declaration.importer.ends_with("catalogued")
                || declaration.importer.ends_with("default-catalog")
        })
        .collect();
    assert_eq!(default_only.len(), 2, "{default_only:?}");
    assert_eq!(uncatalogued(&default_only), None);
}

#[test]
fn a_lockfile_declaring_no_playwright_is_refused() {
    let lockfile = parse_lockfile(
        r"
importers:
  .: {}
  examples/plain:
    dependencies:
      react:
        specifier: 'catalog:'
        version: 19.2.5
",
    );
    let error = declarations(&lockfile).expect_err("a lockfile without Playwright passed");
    assert!(
        error.contains("the scan read the wrong lockfile"),
        "{error}"
    );
}

#[test]
fn a_catalog_version_other_than_the_shell_driver_is_refused() {
    let catalog = catalogued(&parse_lockfile(FIXTURE_LOCKFILE)).expect("catalog");
    let at = |version: &str| Flake {
        version: version.to_owned(),
        browsers: PathBuf::from("/nix/store/shell-playwright-browsers"),
    };
    let report = version_skew(&catalog, &at("1.59.1")).expect("a skewed catalog passed");
    assert_names(
        &report,
        &[
            "the catalog pins @playwright/test 1.58.2, playwright 1.58.2".to_owned(),
            "the development shell's playwright-driver is 1.59.1".to_owned(),
            "pnpm-workspace.yaml to 1.59.1, then run `pnpm install`".to_owned(),
        ],
    );
    assert_eq!(version_skew(&catalog, &at("1.58.2")), None);

    let missing = parse_lockfile(
        "catalogs:\n  default:\n    playwright:\n      specifier: 1.58.2\n      version: 1.58.2\n",
    );
    let error = catalogued(&missing).expect_err("a catalog without @playwright/test passed");
    assert!(error.contains("has no @playwright/test entry"), "{error}");
}

/// Write `node_modules/<name>` under `directory` at `version`; a
/// `playwright-core` also gets a `browsers.json` naming `revision`.
fn package(directory: &Path, name: &str, version: &str, revision: &str) {
    let path = directory.join("node_modules").join(name);
    std::fs::create_dir_all(&path).expect("create a package directory");
    std::fs::write(
        path.join("package.json"),
        serde_json::json!({ "name": name, "version": version }).to_string(),
    )
    .expect("write package.json");
    if name == "playwright-core" {
        let browsers: Vec<Value> = LAUNCHED
            .iter()
            .map(|name| serde_json::json!({ "name": name, "revision": revision }))
            .collect();
        std::fs::write(
            path.join("browsers.json"),
            serde_json::json!({ "browsers": browsers }).to_string(),
        )
        .expect("write browsers.json");
    }
}

fn catalogued_declaration() -> Vec<Declaration> {
    declarations(&parse_lockfile(FIXTURE_LOCKFILE))
        .expect("declarations")
        .into_iter()
        .filter(|declaration| declaration.importer == "examples/catalogued")
        .collect()
}

#[test]
fn a_declaration_with_nothing_installed_is_refused() {
    let root = tempfile::tempdir().expect("create a workspace");
    let error = installed(root.path(), &catalogued_declaration())
        .expect_err("a declaration with nothing installed passed");
    assert_names(
        &error,
        &[
            "examples/catalogued: playwright is not installed".to_owned(),
            "Fix: run `pnpm install`".to_owned(),
        ],
    );
}

#[test]
fn an_install_other_than_the_lockfile_resolves_is_refused() {
    let root = tempfile::tempdir().expect("create a workspace");
    let importer = root.path().join("examples/catalogued");
    package(&importer, "playwright", "1.57.0", "");
    package(&importer, "playwright-core", "1.57.0", "1200");
    let error =
        installed(root.path(), &catalogued_declaration()).expect_err("a stale install passed");
    assert_names(
        &error,
        &[
            "examples/catalogued: playwright 1.57.0 is installed, the lockfile resolves 1.58.2"
                .to_owned(),
            "Fix: run `pnpm install`".to_owned(),
        ],
    );
}

/// Node takes the nearest `node_modules`, so a package's own install shadows
/// one further up, and that is the `playwright-core` it launches with.
#[test]
fn a_nested_install_shadows_the_root_one() {
    let root = tempfile::tempdir().expect("create a workspace");
    package(root.path(), "playwright", "1.58.2", "");
    package(root.path(), "playwright-core", "1.58.2", "1100");
    let importer = root.path().join("examples/catalogued");
    package(&importer, "playwright", "1.58.2", "");
    package(&importer, "playwright-core", "1.58.2", "1208");

    let installs =
        installed(root.path(), &catalogued_declaration()).expect("a nested install passed");
    assert_eq!(installs.len(), 1, "{installs:?}");
    assert_eq!(
        installs[0].builds,
        [
            "chromium-1208",
            "chromium_headless_shell-1208",
            "ffmpeg-1208"
        ]
    );
    assert_eq!(
        installs[0].importers,
        BTreeSet::from(["examples/catalogued".to_owned()])
    );

    // With the package's own install gone, the root one is what it loads.
    std::fs::remove_dir_all(importer.join("node_modules")).expect("remove the nested install");
    let installs = installed(root.path(), &catalogued_declaration()).expect("the root install");
    assert_eq!(installs[0].builds[0], "chromium-1100");
}

/// A browsers directory holding exactly `names`.
fn browsers_holding<'a>(names: impl IntoIterator<Item = &'a str>) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("create a browsers directory");
    for name in names {
        std::fs::create_dir(directory.path().join(name)).expect("create a build directory");
    }
    directory
}

/// The metadata a `playwright-core` release ships, loaded by `importer`: its
/// version, and the revisions its Chromium builds and `ffmpeg` have.
fn release(version: &str, chromium: &str, ffmpeg: &str, importer: &str) -> Install {
    let core = tempfile::tempdir().expect("create a playwright-core directory");
    std::fs::write(
        core.path().join("package.json"),
        serde_json::json!({ "version": version }).to_string(),
    )
    .expect("write package.json");
    let browsers = serde_json::json!({ "browsers": [
        { "name": "chromium", "revision": chromium },
        { "name": "chromium-headless-shell", "revision": chromium },
        { "name": "ffmpeg", "revision": ffmpeg },
    ] });
    std::fs::write(core.path().join("browsers.json"), browsers.to_string())
        .expect("write browsers.json");
    let mut release = install(core.path());
    release.importers.insert(importer.to_owned());
    release
}

/// `playwright-core` 1.58.2, a release before the shell's driver.
fn release_1_58_2() -> Install {
    let stale = release("1.58.2", "1208", "1011", "examples/stale");
    assert_eq!(
        stale.builds,
        [
            "chromium-1208",
            "chromium_headless_shell-1208",
            "ffmpeg-1011"
        ]
    );
    stale
}

#[test]
fn a_browsers_path_is_reported_when_it_lacks_a_build_the_install_launches() {
    // Two releases installed side by side, which share their `ffmpeg` build:
    // the directory holding both is built from the union of their builds.
    let installs = [
        release("1.59.1", "1217", "1011", "examples/current"),
        release_1_58_2(),
    ];
    let wanted: BTreeSet<&str> = installs
        .iter()
        .flat_map(|install| install.builds.iter().map(String::as_str))
        .collect();
    let at = |browsers: &tempfile::TempDir, version: &str| Flake {
        version: version.to_owned(),
        browsers: browsers.path().to_owned(),
    };

    // Holding every build passes.
    let aligned = browsers_holding(wanted.iter().copied());
    assert_eq!(
        mismatch(&installs, &at(&aligned, "9.9.9")),
        None,
        "a path holding every launched build was reported"
    );

    // One build short is a mismatch, and the report names that build.
    let withheld = "chromium_headless_shell-1208";
    assert!(wanted.contains(withheld), "{wanted:?}");
    let skewed = browsers_holding(wanted.iter().copied().filter(|build| *build != withheld));
    let report = mismatch(&installs, &at(&skewed, "9.9.9"))
        .expect("a path without the headless shell passed");
    assert_names(
        &report,
        &[
            format!("{withheld} (missing)"),
            "playwright-core 1.58.2, loaded by examples/stale".to_owned(),
            "The development shell's playwright-driver is 9.9.9".to_owned(),
            format!("PLAYWRIGHT_BROWSERS_PATH, are {}", skewed.path().display()),
            "pnpm-workspace.yaml to 9.9.9, then run `pnpm install`".to_owned(),
        ],
    );

    // When the install already is the driver's release, the catalog is not the
    // fix: the shell's browsers are.
    let report = mismatch(&installs, &at(&skewed, "1.58.2"))
        .expect("a path without the headless shell passed");
    assert!(
        report.contains("so flake.nix has to provide them") && !report.contains("pnpm install"),
        "{report}"
    );
}

/// The failure this target exists for: the flake moved its driver on while the
/// workspace kept the release before it, checked against the development
/// shell's real browsers.
#[test]
fn an_install_of_another_release_is_reported_against_the_shell_browsers() {
    let flake = flake();
    let report = mismatch(&[release_1_58_2()], flake)
        .expect("builds the shell's browsers do not hold passed");
    assert_names(
        &report,
        &[
            "playwright-core 1.58.2, loaded by examples/stale".to_owned(),
            "chromium_headless_shell-1208 (missing)".to_owned(),
            format!(
                "The development shell's playwright-driver is {}",
                flake.version
            ),
            format!("PLAYWRIGHT_BROWSERS_PATH, are {}", flake.browsers.display()),
            format!(
                "pnpm-workspace.yaml to {}, then run `pnpm install`",
                flake.version
            ),
        ],
    );
}
