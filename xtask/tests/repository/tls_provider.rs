use super::repo;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

// rustls chooses the process-level CryptoProvider from its own features when no
// process installed one. `aws_lc_rs` alone selects aws-lc-rs; `ring` beside it,
// or `custom-provider`, leaves nothing selected, and the first config built
// without naming a provider panics - which is how cyper and compio-ws build
// theirs when handed none. `aws-lc-rs` is rustls' alias for `aws_lc_rs`.
//
// Update these and the root Cargo.toml `rustls` entry together.
const SELECTED: &str = "aws_lc_rs";
const COMPETING: &[&str] = &["ring", "custom-provider"];

// Target kinds that become, or load into, a process of their own.
const PROCESS_KINDS: &[&str] = &["bin", "cdylib"];

#[derive(Clone, Copy)]
enum Scope<'a> {
    Workspace,
    Package(&'a str),
}

/// Which edges one `cargo tree` resolve admits.
#[derive(Clone, Copy, Debug)]
struct Resolve {
    /// Dev edges too, which is what `cargo test --workspace` unifies. Without
    /// them the resolve describes what a deployed artifact compiles.
    include_dev: bool,
    /// Every member feature, not only the defaults.
    all_features: bool,
    /// Every target platform rather than the host's, so a dependency declared
    /// under `[target.'cfg(target_os = "macos")'.dependencies]` is read on a
    /// Linux runner too.
    all_targets: bool,
}

impl Resolve {
    const SHIPPED_ON_HOST: Self = Self {
        include_dev: false,
        all_features: false,
        all_targets: false,
    };
}

// The whole feature tree, not `-i rustls`: inverting on a package the selection
// never reaches is a Cargo error, and telling that error apart from a real one
// would mean matching its wording.
fn feature_tree(root: &Path, scope: Scope<'_>, resolve: Resolve) -> Result<String, String> {
    let edges = if resolve.include_dev {
        "features"
    } else {
        "features,normal"
    };
    let mut command = Command::new(env!("CARGO"));
    command
        .current_dir(root)
        .args(["tree", "--locked", "-e", edges, "--color", "never"]);
    match scope {
        Scope::Workspace => command.arg("--workspace"),
        Scope::Package(name) => command.args(["-p", name]),
    };
    if resolve.all_features {
        command.arg("--all-features");
    }
    if resolve.all_targets {
        command.args(["--target", "all"]);
    }
    let output = command.output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "cargo tree ({resolve:?}) failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

/// The rustls features a `cargo tree -e features` output enables, or `None`
/// when rustls is not in that graph at all.
///
/// Both patterns anchor the crate name after whitespace or at a line start, so
/// `futures-rustls v...` and `futures-rustls feature "..."` rows are not read
/// as rustls' own. Empty output is refused: every tree prints its root.
fn rustls_features(output: &str) -> Result<Option<BTreeSet<String>>, String> {
    if output.trim().is_empty() {
        return Err("empty cargo tree output".into());
    }
    let package = regex::Regex::new(r"(?m)(?:^|\s)rustls v[0-9]").unwrap();
    if !package.is_match(output) {
        return Ok(None);
    }
    let row = regex::Regex::new(r#"(?m)(?:^|\s)rustls feature "([A-Za-z0-9_-]+)""#).unwrap();
    Ok(Some(
        row.captures_iter(output)
            .map(|capture| capture[1].to_owned())
            .collect(),
    ))
}

/// Why rustls' own provider inference would not land on [`SELECTED`], if it
/// would not. An empty feature set is a violation, so a reader that matched
/// nothing cannot pass.
fn provider_violation(features: &BTreeSet<String>) -> Option<String> {
    let competing: Vec<_> = COMPETING
        .iter()
        .filter(|feature| features.contains(**feature))
        .collect();
    if features.contains(SELECTED) && competing.is_empty() {
        return None;
    }
    Some(format!(
        "rustls features {features:?}: `{SELECTED}` present = {}, competing {competing:?}",
        features.contains(SELECTED)
    ))
}

fn process_packages(packages: &[&'static Value]) -> Vec<&'static str> {
    packages
        .iter()
        .filter(|package| {
            package["targets"]
                .as_array()
                .expect("package targets")
                .iter()
                .flat_map(|target| target["kind"].as_array().expect("target kinds"))
                .any(|kind| PROCESS_KINDS.contains(&kind.as_str().expect("target kind")))
        })
        .map(|package| package["name"].as_str().expect("package name"))
        .collect()
}

/// Every feature set a root invocation can unify, on any target platform,
/// gives rustls exactly one provider: shipped edges and test edges, default
/// and all features. All targets is a superset of each platform's resolve, so
/// a competitor on any one of them fails here whichever host runs it.
#[test]
fn workspace_builds_leave_rustls_exactly_one_provider() {
    for include_dev in [false, true] {
        for all_features in [false, true] {
            let resolve = Resolve {
                include_dev,
                all_features,
                all_targets: true,
            };
            let output = feature_tree(&repo::root(), Scope::Workspace, resolve).unwrap();
            let features = rustls_features(&output)
                .unwrap()
                .expect("the workspace links rustls");
            if let Some(violation) = provider_violation(&features) {
                panic!(
                    "{resolve:?}: {violation}; \
                     `cargo tree --workspace --target all -e features -i rustls` names the edge"
                );
            }
        }
    }
}

/// A binary built on its own, for the host it is built on, compiles its
/// provider. The workspace union above cannot see this: a package that reaches
/// rustls only through cyper or compio-tls compiles rustls with NO provider
/// when built alone, so every client that relies on the default panics -
/// `cargo build -p` of a single service is how that shows up.
#[test]
fn every_process_artifact_that_links_rustls_compiles_its_provider() {
    let packages = repo::workspace();
    let processes = process_packages(&packages);
    assert!(processes.len() >= 5, "binary scan lost its packages");
    let mut linking = 0;
    let mut violations = Vec::new();
    for name in processes {
        let output = feature_tree(
            &repo::root(),
            Scope::Package(name),
            Resolve::SHIPPED_ON_HOST,
        )
        .unwrap();
        let Some(features) = rustls_features(&output).unwrap() else {
            continue;
        };
        linking += 1;
        if let Some(violation) = provider_violation(&features) {
            violations.push(format!("{name}: {violation}"));
        }
    }
    assert!(linking >= 5, "rustls scan lost the binaries that link it");
    assert!(
        violations.is_empty(),
        "binaries whose own build leaves rustls without exactly one provider: {violations:?}"
    );
}

#[test]
fn feature_reader_reads_rustls_rows_only_and_tells_absent_from_featureless() {
    let output = "\
app v0.1.0 (/a path)\n\
├── compio-postgres feature \"tls\"\n\
│   ├── compio-postgres v0.1.0 (/a path)\n\
│   │   ├── futures-rustls feature \"ring\"\n\
│   │   │   └── futures-rustls v0.26.0\n\
│   │   └── rustls feature \"tls12\"\n\
│   │       └── rustls v0.23.38\n\
│   └── rustls feature \"aws-lc-rs\"\n\
│       ├── rustls v0.23.38 (*)\n\
│       └── rustls feature \"aws_lc_rs\"\n\
│           └── rustls v0.23.38 (*)\n\
└── hyper-rustls feature \"custom-provider\"\n\
    └── hyper-rustls v0.27.9\n";
    assert_eq!(
        rustls_features(output).unwrap(),
        Some(BTreeSet::from([
            "aws-lc-rs".into(),
            "aws_lc_rs".into(),
            "tls12".into(),
        ]))
    );
    // A neighbour whose name ends in `rustls` is not rustls.
    let absent = "\
app v0.1.0 (/a path)\n\
└── futures-rustls feature \"ring\"\n\
    └── futures-rustls v0.26.0\n";
    assert_eq!(rustls_features(absent).unwrap(), None);
    // Present with nothing enabled is a set, and an empty one; the provider
    // check below is what refuses it.
    assert_eq!(
        rustls_features("svc v0.1.0 (/a path)\n└── rustls v0.1.0 (/fixture/rustls)\n").unwrap(),
        Some(BTreeSet::new())
    );
    for empty in ["", "\n"] {
        assert!(rustls_features(empty).is_err(), "accepted {empty:?}");
    }
}

#[test]
fn provider_check_requires_the_selected_provider_and_no_competitor() {
    let set = |features: &[&str]| -> BTreeSet<String> {
        features
            .iter()
            .map(|feature| (*feature).to_owned())
            .collect()
    };
    assert_eq!(
        provider_violation(&set(&["aws-lc-rs", "aws_lc_rs", "std"])),
        None
    );
    for rejected in [
        set(&[]),
        set(&["std", "tls12"]),
        set(&["ring", "std"]),
        set(&["aws_lc_rs", "ring"]),
        set(&["aws_lc_rs", "custom-provider"]),
    ] {
        assert!(
            provider_violation(&rejected).is_some(),
            "accepted {rejected:?}"
        );
    }
}

/// The reader over a real Cargo resolve, not over hand-written text: a dev edge
/// that enables `ring` is invisible on shipped edges and visible on test edges,
/// a macOS-only edge that enables it is visible on every host once all targets
/// are read, a binary whose only path to rustls names no provider is caught when
/// built on its own, and a package that never links rustls is told apart from
/// all of them.
#[test]
fn cargo_resolve_exposes_competing_and_missing_providers_by_edge_target_and_package() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers=['app', 'desk', 'svc', 'bare']\n\
         exclude=['rustls', 'oracle', 'carrier']\nresolver='3'\n",
    )
    .unwrap();
    for (name, manifest, entry) in [
        (
            "rustls",
            "[features]\naws_lc_rs=[]\naws-lc-rs=['aws_lc_rs']\nring=[]\nstd=[]\n",
            "src/lib.rs",
        ),
        (
            "oracle",
            "[dependencies]\nrustls={path='../rustls', features=['ring']}\n",
            "src/lib.rs",
        ),
        (
            "carrier",
            "[dependencies]\nrustls={path='../rustls', features=['std']}\n",
            "src/lib.rs",
        ),
        (
            "app",
            "[dependencies]\nrustls={path='../rustls', features=['aws-lc-rs']}\n\
             [dev-dependencies]\noracle={path='../oracle'}\n",
            "src/main.rs",
        ),
        (
            "desk",
            "[dependencies]\nrustls={path='../rustls', features=['aws-lc-rs']}\n\
             [target.'cfg(target_os = \"macos\")'.dependencies]\noracle={path='../oracle'}\n",
            "src/main.rs",
        ),
        (
            "svc",
            "[dependencies]\ncarrier={path='../carrier'}\n",
            "src/main.rs",
        ),
        ("bare", "", "src/main.rs"),
    ] {
        let package = root.join(name);
        std::fs::create_dir_all(package.join("src")).unwrap();
        std::fs::write(package.join(entry), "").unwrap();
        std::fs::write(
            package.join("Cargo.toml"),
            format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2021'\n{manifest}"),
        )
        .unwrap();
    }
    let output = Command::new(env!("CARGO"))
        .current_dir(root)
        .args(["generate-lockfile", "--offline"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let features =
        |scope, resolve| rustls_features(&feature_tree(root, scope, resolve).unwrap()).unwrap();
    let with_dev = Resolve {
        include_dev: true,
        ..Resolve::SHIPPED_ON_HOST
    };
    let on_all_targets = Resolve {
        all_targets: true,
        ..Resolve::SHIPPED_ON_HOST
    };
    let on_macos = cfg!(target_os = "macos");

    let app = features(Scope::Package("app"), Resolve::SHIPPED_ON_HOST).expect("app links rustls");
    assert_eq!(provider_violation(&app), None, "{app:?}");
    let tested = features(Scope::Package("app"), with_dev).expect("app links rustls");
    assert!(tested.contains("ring"), "{tested:?}");
    assert!(provider_violation(&tested).is_some());

    let desk_here =
        features(Scope::Package("desk"), Resolve::SHIPPED_ON_HOST).expect("desk links rustls");
    assert_eq!(desk_here.contains("ring"), on_macos, "{desk_here:?}");
    let desk_anywhere =
        features(Scope::Package("desk"), on_all_targets).expect("desk links rustls");
    assert!(desk_anywhere.contains("ring"), "{desk_anywhere:?}");
    assert!(provider_violation(&desk_anywhere).is_some());

    // The workspace union carries both competitors, each only where its edge
    // is admitted.
    let shipped =
        features(Scope::Workspace, Resolve::SHIPPED_ON_HOST).expect("fixture links rustls");
    assert_eq!(shipped.contains("ring"), on_macos, "{shipped:?}");
    for resolve in [with_dev, on_all_targets] {
        let union = features(Scope::Workspace, resolve).expect("fixture links rustls");
        assert!(
            provider_violation(&union).is_some(),
            "{resolve:?}: {union:?}"
        );
    }

    let svc = features(Scope::Package("svc"), Resolve::SHIPPED_ON_HOST).expect("svc links rustls");
    assert!(svc.contains("std"), "{svc:?}");
    assert!(provider_violation(&svc).is_some(), "{svc:?}");
    assert_eq!(
        features(Scope::Package("bare"), Resolve::SHIPPED_ON_HOST),
        None
    );

    std::fs::remove_file(root.join("Cargo.lock")).unwrap();
    assert!(
        feature_tree(root, Scope::Workspace, Resolve::SHIPPED_ON_HOST).is_err(),
        "Cargo failure was accepted"
    );
}
