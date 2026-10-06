//! The platform image provides every executable a compose service runs from it.
//!
//! `deploy/compose/docker-compose.yml` starts several services from images
//! built by `deploy/Dockerfile`. A service's image is the stage its
//! `build.target` names, or the final stage when it names none; the image holds
//! the `COPY` instructions of that stage and of every stage it is built from. A
//! service whose `command` runs a binary, or whose flags-only `command`
//! supplies arguments to the image's `ENTRYPOINT`, is asking the image for an
//! executable, and a shell wrapper asks for the process its `exec` names. The
//! builder must select the package that produces a workspace binary AND the
//! image must install it, either copied out of the builder stage or written by
//! a context `COPY`. The tokio-boundary check reads the builder selection
//! alone, and only to prove it is a subset of the shipped packages, so an
//! executable the image never installs is invisible to it: the service starts,
//! the container exits because the executable is absent, and nothing in the
//! repository names the omission.
//!
//! The compose file is parsed as YAML and the Dockerfile is parsed instruction
//! by instruction, so a service and its install are compared as structure
//! rather than by searching the text.

use crate::architecture::repo;
use serde_yaml::Value as Yaml;
use std::collections::{BTreeMap, BTreeSet};

/// The compose file whose services build the image.
const COMPOSE: &str = "deploy/compose/docker-compose.yml";

/// The image recipe.
const DOCKERFILE: &str = "deploy/Dockerfile";

/// The `build.dockerfile` spelling that selects the image under test.
const COMPOSE_DOCKERFILE: &str = "deploy/Dockerfile";

/// Binary target name to the package that produces it.
type Binaries = BTreeMap<String, String>;

/// Shell programs that run a script rather than being the service binary: the
/// process the container runs is named by the script's `exec`.
const SHELLS: &[&str] = &["sh", "bash", "dash", "zsh", "ksh", "ash"];

/// A `COPY` instruction: the stage it reads from (none is the build context)
/// and the source and destination paths.
#[derive(Debug)]
struct Copy {
    from: Option<String>,
    source: String,
    destination: String,
}

/// A Dockerfile stage: its name (`""` when unnamed), the image or stage it is
/// built from, the `ENTRYPOINT` it declares, and its copies.
#[derive(Debug)]
struct Stage {
    name: String,
    base: String,
    entrypoint: Option<String>,
    copies: Vec<Copy>,
}

/// A parsed Dockerfile: its stages in declaration order.
#[derive(Debug)]
struct Dockerfile {
    stages: Vec<Stage>,
}

/// The executables one image installs, by name.
#[derive(Debug, Default)]
struct Installs {
    builder: BTreeSet<String>,
    context: BTreeSet<String>,
}

fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

fn is_shell(token: &str) -> bool {
    let name = basename(token);
    SHELLS.contains(&name.as_str())
}

/// A shell word with its surrounding quotes removed.
fn unquote(word: &str) -> String {
    word.trim_matches(|c| c == '"' || c == '\'').to_owned()
}

/// The logical lines of a Dockerfile, with backslash-continued lines joined and
/// comments and blank lines skipped.
fn logical_lines(source: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for raw in source.lines() {
        let line = raw.trim();
        if current.is_empty() && (line.is_empty() || line.starts_with('#')) {
            continue;
        }
        current.push_str(line.trim_end_matches('\\').trim_end());
        if line.ends_with('\\') {
            current.push(' ');
            continue;
        }
        lines.push(std::mem::take(&mut current));
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// The operand of an instruction named `name`, matched case-insensitively.
fn instruction<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let mut words = line.splitn(2, char::is_whitespace);
    let head = words.next()?;
    head.eq_ignore_ascii_case(name)
        .then(|| words.next().unwrap_or("").trim())
}

/// The stage name a `FROM` operand declares, or its image reference when it
/// declares none.
fn stage_name(operand: &str) -> String {
    let words: Vec<&str> = operand.split_whitespace().collect();
    for (index, word) in words.iter().enumerate() {
        if word.eq_ignore_ascii_case("as") {
            if let Some(name) = words.get(index + 1) {
                return (*name).to_owned();
            }
        }
    }
    words.first().copied().unwrap_or("").to_owned()
}

/// The first token of an `ENTRYPOINT`: the JSON array element or the shell-form
/// program.
fn entrypoint_token(operand: &str) -> Option<String> {
    let operand = operand.trim();
    if operand.starts_with('[') {
        let argv: Vec<String> = serde_json::from_str(operand).ok()?;
        argv.into_iter().next()
    } else {
        operand.split_whitespace().next().map(unquote)
    }
}

/// A `COPY` line with one source and a destination, or `None` for another
/// instruction or the multi-source shape this reader does not resolve.
fn parse_copy(line: &str) -> Option<Copy> {
    let operand = instruction(line, "COPY")?;
    let mut from = None;
    let mut paths = Vec::new();
    for word in operand.split_whitespace() {
        if let Some(stage) = word.strip_prefix("--from=") {
            from = Some(stage.to_owned());
        } else if !word.starts_with("--") {
            paths.push(word.to_owned());
        }
    }
    let [source, destination] = paths.as_slice() else {
        return None;
    };
    Some(Copy {
        from,
        source: source.clone(),
        destination: destination.clone(),
    })
}

/// Parse a Dockerfile into its stages and their copies.
fn parse_dockerfile(source: &str) -> Dockerfile {
    let mut stages: Vec<Stage> = Vec::new();
    for line in logical_lines(source) {
        if let Some(operand) = instruction(&line, "FROM") {
            let base = operand.split_whitespace().next().unwrap_or("").to_owned();
            stages.push(Stage {
                name: stage_name(operand),
                base,
                entrypoint: None,
                copies: Vec::new(),
            });
        } else if let Some(operand) = instruction(&line, "ENTRYPOINT") {
            if let Some(stage) = stages.last_mut() {
                stage.entrypoint = entrypoint_token(operand);
            }
        } else if let Some(copy) = parse_copy(&line) {
            if let Some(stage) = stages.last_mut() {
                stage.copies.push(copy);
            }
        }
    }
    Dockerfile { stages }
}

impl Dockerfile {
    /// The index of the stage a service builds, or of the final stage when the
    /// service names no target.
    fn stage_index(&self, target: Option<&str>) -> Option<usize> {
        match target {
            Some(target) => self.stages.iter().position(|stage| stage.name == target),
            None => self.stages.len().checked_sub(1),
        }
    }

    /// The stage and every earlier stage it is built from, by index.
    fn chain(&self, index: usize) -> Vec<usize> {
        let mut chain = vec![index];
        let mut current = index;
        while current > 0 {
            let base = self.stages[current].base.as_str();
            match self.stages[..current]
                .iter()
                .position(|stage| stage.name == base)
            {
                Some(parent) => {
                    chain.push(parent);
                    current = parent;
                }
                None => break,
            }
        }
        chain
    }

    /// The executables the image at `index` installs: the copies of that stage
    /// and of every stage it is built from.
    fn installs(&self, index: usize) -> Result<Installs, String> {
        collect_installs(
            self.chain(index)
                .into_iter()
                .flat_map(|stage| self.stages[stage].copies.iter()),
        )
    }
}

/// The executable name a `COPY` installs: the source for a directory
/// destination, the destination for a file (a rename).
fn install_name(copy: &Copy) -> String {
    if copy.destination.ends_with('/') {
        basename(&copy.source)
    } else {
        basename(&copy.destination)
    }
}

/// Fold copies into the installs they name. A copy of a directory names no
/// executable and is refused rather than credited with an empty name.
fn collect_installs<'a>(copies: impl Iterator<Item = &'a Copy>) -> Result<Installs, String> {
    let mut installs = Installs::default();
    for copy in copies {
        if copy.source.ends_with('/') {
            match copy.from.as_deref() {
                Some("builder") => {
                    return Err(format!(
                        "a builder COPY installs a directory, which names no executable: {copy:?}"
                    ));
                }
                None if copy.destination.starts_with("/usr/local/bin/") => {
                    return Err(format!(
                        "a context COPY installs a directory into /usr/local/bin, which names \
                         no executable: {copy:?}"
                    ));
                }
                _ => {}
            }
        }
        let name = install_name(copy);
        match copy.from.as_deref() {
            Some("builder") => {
                installs.builder.insert(name);
            }
            None if copy.destination.starts_with("/usr/local/bin/") => {
                installs.context.insert(name);
            }
            _ => {}
        }
    }
    Ok(installs)
}

impl Installs {
    /// Whether a resolved executable is installed, by the name it is installed
    /// under.
    fn contains(&self, token: &str) -> bool {
        let name = basename(token);
        self.builder.contains(&name) || self.context.contains(&name)
    }
}

/// Every binary target the workspace declares, mapped to the package that
/// produces it. A compose service runs the target by name, so the name is the
/// vocabulary this reader matches against.
fn workspace_binaries() -> Binaries {
    let mut binaries = Binaries::new();
    for package in repo::workspace() {
        let package_name = package["name"].as_str().expect("package name").to_owned();
        for name in repo::bin_targets(package) {
            if let Some(previous) = binaries.insert(name.clone(), package_name.clone()) {
                assert_eq!(
                    previous, package_name,
                    "binary target {name} is declared by two packages"
                );
            }
        }
    }
    assert!(
        binaries.contains_key("zeroship-workflow-server"),
        "the workspace scan does not declare zeroship-workflow-server: {binaries:?}"
    );
    binaries
}

/// The string tokens of a scalar or sequence value, with surrounding quotes
/// removed. A command written as one folded scalar yields its shell words; a
/// command written as a list yields its elements.
fn string_tokens(value: &Yaml) -> Vec<String> {
    match value {
        Yaml::String(text) => text.split_whitespace().map(unquote).collect(),
        Yaml::Sequence(items) => items.iter().flat_map(string_tokens).collect(),
        _ => Vec::new(),
    }
}

/// The executable a shell script runs: the word after its last `exec`, or
/// nothing when it never execs.
fn exec_target(tokens: &[String]) -> Option<String> {
    let last = tokens.iter().rposition(|token| token == "exec")?;
    let target = unquote(tokens.get(last + 1)?);
    (!target.is_empty()).then_some(target)
}

/// The executable a service runs.
///
/// A service `entrypoint` names it directly unless that entrypoint is a shell,
/// whose script - the rest of the entrypoint and the command together - names
/// the process it execs. With no entrypoint, a flags-only command supplies
/// arguments to the image `ENTRYPOINT`, and any other command's first token is
/// the executable.
fn resolve_executable(service: &Yaml, image_entrypoint: Option<&str>) -> Option<String> {
    let entrypoint = string_tokens(&service["entrypoint"]);
    let command = string_tokens(&service["command"]);

    if let Some(first) = entrypoint.first() {
        if is_shell(first) {
            let mut script: Vec<String> = entrypoint[1..].to_vec();
            script.extend(command.iter().cloned());
            return exec_target(&script);
        }
        return Some(first.clone());
    }

    let first = command.first()?;
    if first.starts_with('-') {
        return image_entrypoint.map(str::to_owned);
    }
    if is_shell(first) {
        return exec_target(&command[1..]);
    }
    Some(first.clone())
}

/// What the image provides and what the compose services ask of it.
struct Image {
    /// Compose services whose `build.dockerfile` selects this image.
    services: BTreeSet<String>,
    /// Packages the builder invocation selects.
    built: BTreeSet<String>,
    /// Executable names the scoped images copy out of the builder stage.
    builder_installs: BTreeSet<String>,
    /// Executable names the scoped images install by a context `COPY` into
    /// /usr/local/bin.
    context_installs: BTreeSet<String>,
    /// Service executables named by compose.
    required: BTreeSet<(String, String)>,
    /// Requirements the image does not satisfy, as sentences.
    missing: Vec<String>,
}

/// Compare the compose services against the image each one gets.
fn analyze(
    compose_source: &str,
    dockerfile_source: &str,
    binaries: &Binaries,
) -> Result<Image, String> {
    let compose: Yaml =
        serde_yaml::from_str(compose_source).map_err(|error| format!("parse compose: {error}"))?;
    let services = compose["services"]
        .as_mapping()
        .ok_or("the compose file declares no services")?;
    if services.is_empty() {
        return Err("the compose file declares no services".into());
    }

    let dockerfile = parse_dockerfile(dockerfile_source);
    let any: Installs =
        collect_installs(dockerfile.stages.iter().flat_map(|stage| stage.copies.iter()))?;
    if any.builder.is_empty() && any.context.is_empty() {
        return Err("the Dockerfile installs no executable into /usr/local/bin".into());
    }
    let built: BTreeSet<String> = super::tokio_boundary::dockerfile_build(dockerfile_source)?
        .into_iter()
        .collect();

    let mut scoped = BTreeSet::new();
    let mut required = BTreeSet::new();
    let mut missing = Vec::new();
    let mut builder_installs = BTreeSet::new();
    let mut context_installs = BTreeSet::new();
    for (key, service) in services {
        let name = key.as_str().ok_or("a compose service name is not a string")?;
        if service["build"]["dockerfile"].as_str() != Some(COMPOSE_DOCKERFILE) {
            continue;
        }
        scoped.insert(name.to_owned());
        let target = service["build"]["target"].as_str();
        let Some(index) = dockerfile.stage_index(target) else {
            missing.push(format!(
                "service `{name}` builds target `{}`, which the Dockerfile does not declare",
                target.unwrap_or("")
            ));
            continue;
        };
        let installs = dockerfile.installs(index)?;
        builder_installs.extend(installs.builder.iter().cloned());
        context_installs.extend(installs.context.iter().cloned());
        let image_entrypoint = dockerfile.stages[index].entrypoint.as_deref();
        match resolve_executable(service, image_entrypoint) {
            Some(token) => {
                required.insert((name.to_owned(), token.clone()));
                if !installs.contains(&token) {
                    missing.push(format!(
                        "service `{name}` runs `{token}`, which no COPY of its image installs"
                    ));
                } else if let Some(package) = binaries
                    .get(&token)
                    .or_else(|| binaries.get(&basename(&token)))
                {
                    if !built.contains(package) {
                        missing.push(format!(
                            "service `{name}` runs `{token}`, but the builder invocation \
                             does not select package `{package}`"
                        ));
                    }
                }
            }
            None => missing.push(format!(
                "service `{name}` names no executable the image installs"
            )),
        }
    }

    if scoped.is_empty() {
        return Err(format!("no compose service builds {COMPOSE_DOCKERFILE}"));
    }

    Ok(Image {
        services: scoped,
        built,
        builder_installs,
        context_installs,
        required,
        missing,
    })
}

/// Every executable compose asks an image for is built and installed.
#[test]
fn compose_runs_only_binaries_the_image_builds_and_copies() {
    let binaries = workspace_binaries();
    let image = analyze(&repo::read(COMPOSE), &repo::read(DOCKERFILE), &binaries)
        .expect("read the image and the compose services");

    assert!(
        image.missing.is_empty(),
        "these compose services run an executable the image does not provide:\n  {}",
        image.missing.join("\n  ")
    );

    for service in ["workflow", "migrate", "gateway"] {
        assert!(
            image.services.contains(service),
            "the compose scan does not build the `{service}` service from the image: {:?}",
            image.services
        );
    }
    assert!(
        image.builder_installs.contains("zeroship-workflow-server"),
        "the runtime stage does not copy zeroship-workflow-server out of the builder: {:?}",
        image.builder_installs
    );
    assert!(
        image.context_installs.contains("zeroship-migrate-platform"),
        "the image does not install the migrate entrypoint: {:?}",
        image.context_installs
    );
    assert!(
        image.built.contains("zeroship-workflow-server"),
        "the builder invocation does not select zeroship-workflow-server: {:?}",
        image.built
    );
    for required in [
        ("workflow", "zeroship-workflow-server"),
        ("migrate", "/usr/local/bin/zeroship-migrate-platform"),
        ("gateway", "zeroship-gate"),
    ] {
        assert!(
            image
                .required
                .contains(&(required.0.to_owned(), required.1.to_owned())),
            "the service scan does not resolve {required:?}: {:?}",
            image.required
        );
    }
}

/// The reader reports every service it cannot resolve to an executable its own
/// image installs, and accepts each pair once that executable is installed.
#[test]
fn the_service_binary_reader_refuses_a_missing_builder_copy() {
    let binaries: Binaries = [
        ("demo-app".to_owned(), "demo".to_owned()),
        ("other-app".to_owned(), "other".to_owned()),
    ]
    .into_iter()
    .collect();

    let compose = "\
services:
  app:
    build:
      context: ../..
      dockerfile: deploy/Dockerfile
    command: demo-app --port 1
  base:
    image: alpine
    command: sh
";

    let copied = "\
FROM rust:latest AS builder
WORKDIR /build
RUN cargo build --release \\
    -p demo

FROM ubuntu:24.04 AS runtime
COPY --from=builder /build/target/release/demo-app /usr/local/bin/
";

    let accepted = analyze(compose, copied, &binaries).expect("read the copied pair");
    assert_eq!(
        accepted.required.iter().cloned().collect::<Vec<_>>(),
        [("app".to_owned(), "demo-app".to_owned())],
        "the service scan resolved the wrong executable"
    );
    assert!(
        accepted.missing.is_empty(),
        "a copied executable was reported missing: {:?}",
        accepted.missing
    );

    let uncopied = copied.replace(
        "/build/target/release/demo-app",
        "/build/target/release/other-app",
    );
    let refused = analyze(compose, &uncopied, &binaries).expect("read the uncopied pair");
    assert_eq!(
        refused.missing.len(),
        1,
        "the reader did not report exactly the missing copy: {:?}",
        refused.missing
    );
    assert!(
        refused.missing[0].contains("demo-app"),
        "the refusal does not name the executable: {:?}",
        refused.missing
    );

    // A Dockerfile that installs nothing out of the builder stage is refused.
    let no_builder_copy = "\
FROM rust:latest AS builder
WORKDIR /build
RUN cargo build --release \\
    -p demo

FROM ubuntu:24.04 AS runtime
COPY --from=other /build/target/release/demo-app /usr/local/bin/
";
    assert!(
        analyze(compose, no_builder_copy, &binaries).is_err(),
        "a Dockerfile that installs no executable was accepted"
    );

    // A compose file whose services none build this image is refused.
    let no_image_service = "\
services:
  base:
    image: alpine
    command: sh
";
    assert!(
        analyze(no_image_service, copied, &binaries).is_err(),
        "a compose with no service built from the image was accepted"
    );

    // A compose file that is not YAML is refused rather than read as empty.
    assert!(
        analyze("services: [", copied, &binaries).is_err(),
        "a malformed compose file was accepted"
    );
    // A service key that is not a string is refused.
    assert!(
        analyze("services:\n  1:\n    image: alpine\n", copied, &binaries).is_err(),
        "a non-string service name was accepted"
    );

    // Two copies, so a shape that drops one still leaves the other installed.
    let two_copies = "\
FROM rust:latest AS builder
WORKDIR /build
RUN cargo build --release \\
    -p demo \\
    -p other

FROM ubuntu:24.04 AS runtime
COPY --from=builder /build/target/release/demo-app /usr/local/bin/
COPY --from=builder /build/target/release/other-app /usr/local/bin/
";
    // A multi-source COPY names no single executable and is not credited.
    let multi_source = two_copies.replace(
        "COPY --from=builder /build/target/release/demo-app /usr/local/bin/",
        "COPY --from=builder /build/target/release/demo-app /build/target/release/other-app /usr/local/bin/",
    );
    let image =
        analyze(compose, &multi_source, &binaries).expect("read the multi-source pair");
    assert!(
        image.missing.iter().any(|line| line.contains("demo-app")),
        "a multi-source COPY was credited instead of refused: {:?}",
        image.missing
    );

    // A renamed install resolves to its destination basename.
    let renamed = two_copies.replace(
        "COPY --from=builder /build/target/release/demo-app /usr/local/bin/",
        "COPY --from=builder /build/target/release/demo-app /usr/local/bin/renamed-demo",
    );
    let renamed_compose = "\
services:
  renamed:
    build:
      context: ../..
      dockerfile: deploy/Dockerfile
    command: renamed-demo --port 1
";
    let accepted =
        analyze(renamed_compose, &renamed, &binaries).expect("read the renamed pair");
    assert!(
        accepted
            .required
            .contains(&("renamed".to_owned(), "renamed-demo".to_owned())),
        "a renamed install did not resolve to its destination basename: {:?}",
        accepted.required
    );
    assert!(
        accepted.missing.is_empty(),
        "the destination basename of a renamed install was reported missing: {:?}",
        accepted.missing
    );
    let image = analyze(compose, &renamed, &binaries).expect("read the renamed pair");
    assert!(
        image.missing.iter().any(|line| line.contains("demo-app")),
        "a service that asks for the source name of a renamed install was accepted: {:?}",
        image.missing
    );

    // A COPY of a whole directory names no executable and is refused.
    let directory_source = copied.replace(
        "COPY --from=builder /build/target/release/demo-app /usr/local/bin/",
        "COPY --from=builder /build/target/release/ /usr/local/bin/",
    );
    assert!(
        analyze(compose, &directory_source, &binaries).is_err(),
        "a directory-source COPY was accepted"
    );

    // A target-scoped service sees only its own stage and the stages it is
    // built from: the `bare` stage has none of the runtime stage's copies.
    let scoped_stages = "\
FROM rust:latest AS builder
WORKDIR /build
RUN cargo build --release \\
    -p demo

FROM ubuntu:24.04 AS runtime
COPY --from=builder /build/target/release/demo-app /usr/local/bin/

FROM ubuntu:24.04 AS bare
";
    let scoped_compose = "\
services:
  scoped:
    build:
      context: ../..
      dockerfile: deploy/Dockerfile
      target: bare
    command: demo-app --port 1
";
    let image =
        analyze(scoped_compose, scoped_stages, &binaries).expect("read the scoped pair");
    assert!(
        image.missing.iter().any(|line| line.contains("demo-app")),
        "a stage without another stage's copy credited it: {:?}",
        image.missing
    );

    // A target the Dockerfile does not declare is refused.
    let bogus_compose = "\
services:
  bogus:
    build:
      context: ../..
      dockerfile: deploy/Dockerfile
      target: nope
    command: demo-app --port 1
";
    let image = analyze(bogus_compose, copied, &binaries).expect("read the bogus-target pair");
    assert!(
        image.missing.iter().any(|line| line.contains("nope")),
        "an undeclared target was accepted: {:?}",
        image.missing
    );

    // A shell wrapper's executable is its `exec` target, not the first
    // installed word in the script.
    let wrapper_compose = "\
services:
  wrapper:
    build:
      context: ../..
      dockerfile: deploy/Dockerfile
    entrypoint: /bin/sh
    command:
      - -c
      - \"demo-app --help; exec missing-binary\"
";
    let image =
        analyze(wrapper_compose, copied, &binaries).expect("read the wrapper pair");
    assert!(
        image
            .missing
            .iter()
            .any(|line| line.contains("missing-binary")),
        "a wrapper that execs an uninstalled binary was accepted: {:?}",
        image.missing
    );

    // A shell wrapper that never execs resolves to nothing, and is reported.
    let no_exec_compose = "\
services:
  wrapper:
    build:
      context: ../..
      dockerfile: deploy/Dockerfile
    entrypoint: /bin/sh
    command:
      - -c
      - echo hi
";
    let image = analyze(no_exec_compose, copied, &binaries).expect("read the no-exec pair");
    assert!(
        image.missing.iter().any(|line| line.contains("wrapper")),
        "a wrapper with no exec was skipped: {:?}",
        image.missing
    );

    // A list-form entrypoint carries the invocation in its later elements.
    let list_entry_compose = "\
services:
  wrapper:
    build:
      context: ../..
      dockerfile: deploy/Dockerfile
    entrypoint:
      - /bin/sh
      - -c
      - exec demo-app
";
    let accepted =
        analyze(list_entry_compose, copied, &binaries).expect("read the list-entrypoint pair");
    assert!(
        accepted
            .required
            .contains(&("wrapper".to_owned(), "demo-app".to_owned())),
        "a list-form entrypoint did not resolve its exec target: {:?}",
        accepted.required
    );
    assert!(
        accepted.missing.is_empty(),
        "an installed list-form entrypoint was reported missing: {:?}",
        accepted.missing
    );

    // A quoted scalar command is unquoted before it is resolved.
    let quoted_compose = "\
services:
  quoted:
    build:
      context: ../..
      dockerfile: deploy/Dockerfile
    command: sh -c \"exec demo-app\"
";
    let accepted =
        analyze(quoted_compose, copied, &binaries).expect("read the quoted-command pair");
    assert!(
        accepted
            .required
            .contains(&("quoted".to_owned(), "demo-app".to_owned())),
        "a quoted scalar command did not resolve its exec target: {:?}",
        accepted.required
    );
    assert!(
        accepted.missing.is_empty(),
        "an installed quoted command was reported missing: {:?}",
        accepted.missing
    );

    // A service entrypoint that names an installed binary is accepted.
    let entry_compose = "\
services:
  entry:
    build:
      context: ../..
      dockerfile: deploy/Dockerfile
    entrypoint: demo-app
";
    let accepted =
        analyze(entry_compose, copied, &binaries).expect("read the entrypoint binary pair");
    assert!(
        accepted
            .required
            .contains(&("entry".to_owned(), "demo-app".to_owned())),
        "the service entrypoint did not resolve: {:?}",
        accepted.required
    );
    assert!(
        accepted.missing.is_empty(),
        "an installed service entrypoint was reported missing: {:?}",
        accepted.missing
    );

    // A flags-only command supplies arguments to the image ENTRYPOINT. With
    // that executable installed by a context COPY, the service is accepted.
    let flags_compose = "\
services:
  flags:
    build:
      context: ../..
      dockerfile: deploy/Dockerfile
    command:
      - --migrations-dir
      - /app/db/migrations-ts
";
    let entrypoint_installed = "\
FROM rust:latest AS builder
WORKDIR /build
RUN cargo build --release \\
    -p demo

FROM ubuntu:24.04 AS runtime
COPY --from=builder /build/target/release/demo-app /usr/local/bin/
COPY deploy/ops/entrypoint.sh /usr/local/bin/entrypoint
ENTRYPOINT [\"/usr/local/bin/entrypoint\"]
";
    let accepted =
        analyze(flags_compose, entrypoint_installed, &binaries).expect("read the entrypoint image");
    assert!(
        accepted
            .required
            .contains(&("flags".to_owned(), "/usr/local/bin/entrypoint".to_owned())),
        "the flags-only service did not resolve to the image ENTRYPOINT: {:?}",
        accepted.required
    );
    assert!(
        accepted.missing.is_empty(),
        "an installed image ENTRYPOINT was reported missing: {:?}",
        accepted.missing
    );

    // The same flags-only command with the ENTRYPOINT target not installed is
    // refused rather than skipped.
    let entrypoint_uninstalled = "\
FROM rust:latest AS builder
WORKDIR /build
RUN cargo build --release \\
    -p demo

FROM ubuntu:24.04 AS runtime
COPY --from=builder /build/target/release/demo-app /usr/local/bin/
ENTRYPOINT [\"/usr/local/bin/entrypoint\"]
";
    let refused = analyze(flags_compose, entrypoint_uninstalled, &binaries)
        .expect("read the uninstalled entrypoint image");
    assert!(
        refused.missing.iter().any(|line| line.contains("entrypoint")),
        "an uninstalled image ENTRYPOINT was accepted: {:?}",
        refused.missing
    );

    // A shell-form ENTRYPOINT is read the same way as the JSON form.
    let entrypoint_shell_form = "\
FROM rust:latest AS builder
WORKDIR /build
RUN cargo build --release \\
    -p demo

FROM ubuntu:24.04 AS runtime
COPY --from=builder /build/target/release/demo-app /usr/local/bin/
COPY deploy/ops/entrypoint.sh /usr/local/bin/entrypoint
ENTRYPOINT /usr/local/bin/entrypoint
";
    let accepted = analyze(flags_compose, entrypoint_shell_form, &binaries)
        .expect("read the shell-form entrypoint image");
    assert!(
        accepted
            .required
            .contains(&("flags".to_owned(), "/usr/local/bin/entrypoint".to_owned())),
        "a shell-form ENTRYPOINT did not resolve: {:?}",
        accepted.required
    );

    // A final stage that declares no name is still the image a target-less
    // service gets.
    let unnamed_stage = "\
FROM rust:latest AS builder
WORKDIR /build
RUN cargo build --release \\
    -p demo

FROM ubuntu:24.04
COPY --from=builder /build/target/release/demo-app /usr/local/bin/
";
    let accepted = analyze(compose, unnamed_stage, &binaries).expect("read the unnamed-stage pair");
    assert!(
        accepted.missing.is_empty(),
        "an unnamed final stage did not serve its service: {:?}",
        accepted.missing
    );
}
