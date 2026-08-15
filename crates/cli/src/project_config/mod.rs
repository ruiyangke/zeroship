//! `zeroship.jsonc` - the creator project configuration, CLI side.
//!
//! SCOPE INVARIANT: this file is
//! read by the `zeroship` CLI and by the build toolchain. It is NEVER read by
//! the runtime, NEVER packed into a `.zship`, and never leaves the creator's
//! machine. `tests/project_config_gate.sh` enforces all three.
//!
//! NO DEFAULTS FOR CLI-READ FACTS LIVE HERE. When the file is present and a key
//! the CLI operationally reads is absent, the command errors naming the key.
//! Optional non-CLI defaults are generated from the schema into both readers so
//! their resolved JSON stays byte-identical.
//!
//! WHEN NO FILE IS PRESENT nothing changes: `--flag`, then the environment
//! variable, then the compiled fallback each command already had. A creator in
//! a scratch directory keeps the CLI they have. The no-default rule is scoped to
//! "there IS a file and it does not say", which is the case where a guess would
//! contradict a written intention.

pub mod generated;
mod jsonc;

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

pub use generated::CONFIG_FILENAME;

/// Where a resolved value came from. Printed before every mutating call.
///
/// A config file that silently supplies a control URL is strictly more
/// dangerous than a flag that must be typed, because the flag is in the shell
/// history and the file is not in the command. The provenance line is what
/// makes the file safe to have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Flag(&'static str),
    EnvVar(&'static str),
    /// The root of the config file.
    File,
    /// The named environment block of the config file.
    FileEnvironment(String),
    /// A DIFFERENT member of the file standing in for the one asked for.
    /// Today the only case is `deploy` using `name` when `app` is absent, on
    /// a first push. Named separately so the provenance line says so rather
    /// than claiming the file set `app`.
    FileMember(&'static str),
    /// The command's compiled fallback, reached only when there is no file.
    Fallback,
}

impl Source {
    pub fn describe(&self) -> String {
        match self {
            Source::Flag(f) => format!("{f} flag"),
            Source::EnvVar(n) => format!("${n}"),
            Source::File => CONFIG_FILENAME.to_string(),
            Source::FileEnvironment(name) => {
                format!("{CONFIG_FILENAME} environments.{name}")
            }
            Source::FileMember(member) => format!("{CONFIG_FILENAME} {member}"),
            Source::Fallback => "built-in default".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Locating the file
// ---------------------------------------------------------------------------

/// Find the config file: `--config=` flag, then `ZEROSHIP_CONFIG`, then
/// `zeroship.jsonc` in the current directory.
///
/// NO FORMAT FALLBACKS and NO UPWARD WALK. Cloudflare searches
/// `.jsonc` then `.json` then `.toml`; that is a back-compat artifact and we
/// have no legacy files to accept. The walk is refused for a sharper reason: a
/// `zeroship deploy` run in a subdirectory would silently pick up a sibling
/// app's `app` and `control`, which is the cross-targeting hazard the
/// environments rule exists to close, arriving through the file-location door.
///
/// An explicitly named file that does not exist is an ERROR. Only
/// auto-discovery is allowed to come up empty.
pub fn locate(args: &[String], cwd: &Path) -> Result<Option<PathBuf>, String> {
    if let Some(flag) = crate::flag_str(args, "--config=") {
        let path = cwd.join(&flag);
        if !path.is_file() {
            return Err(format!(
                "--config={flag}: no such file ({})",
                path.display()
            ));
        }
        return Ok(Some(path));
    }
    if let Some(from_env) = zeroship_core::declared_env!(
        cli,
        "ZEROSHIP_CONFIG",
        crate::ZeroshipCliConsumer
    ) {
        let path = cwd.join(&from_env);
        if !path.is_file() {
            return Err(format!(
                "{}={from_env}: no such file ({})",
                generated::CONFIG_ENV_VAR,
                path.display()
            ));
        }
        return Ok(Some(path));
    }
    let path = cwd.join(CONFIG_FILENAME);
    Ok(path.is_file().then_some(path))
}

// ---------------------------------------------------------------------------
// Loading + validation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ProjectConfig {
    pub path: PathBuf,
    /// The file's exact bytes. Kept so the writeback can splice rather than
    /// re-serialise.
    pub text: String,
    root: Map<String, Value>,
}

impl ProjectConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let project_root = std::env::current_dir()
            .map_err(|e| format!("cannot read the working directory: {e}"))?;
        Self::parse_with_root(path.to_path_buf(), text, &project_root)
    }

    pub fn parse(path: PathBuf, text: String) -> Result<Self, String> {
        let project_root = path.parent().unwrap_or_else(|| Path::new("."));
        Self::parse_with_root(path.clone(), text, project_root)
    }

    fn parse_with_root(path: PathBuf, text: String, project_root: &Path) -> Result<Self, String> {
        let stripped = jsonc::strip(&text);
        let value: Value = serde_json::from_str(&stripped)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let root = match value {
            Value::Object(map) => map,
            _ => {
                return Err(format!(
                    "{}: the top level must be an object",
                    path.display()
                ))
            }
        };
        let cfg = ProjectConfig { path, text, root };
        cfg.validate(project_root)?;
        Ok(cfg)
    }

    fn err(&self, msg: impl AsRef<str>) -> String {
        format!("{}: {}", self.path.display(), msg.as_ref())
    }

    fn validate(&self, project_root: &Path) -> Result<(), String> {
        self.reject_forbidden_names(&Value::Object(self.root.clone()), "")?;
        // A `$schema` pointing somewhere else is a file written against a
        // different contract. Accepting it silently would mean validating v1
        // rules against a v2 document and reporting the mismatches as the
        // creator's typos.
        if let Some(id) = self.root.get("$schema").and_then(Value::as_str) {
            if id != generated::SCHEMA_ID {
                return Err(self.err(format!(
                    "$schema is {id}, but this `zeroship` reads {}",
                    generated::SCHEMA_ID
                )));
            }
        }
        check_object(
            &self.root,
            "",
            generated::ROOT_KNOWN_KEYS,
            generated::ROOT_REQUIRED_KEYS,
        )
        .map_err(|e| self.err(e))?;
        check_members(&self.root, "").map_err(|e| self.err(e))?;
        self.reject_dist_containing_config(&self.root, project_root, "build.dist")?;

        if let Some(envs) = self.root.get("environments") {
            let envs = envs.as_object().ok_or_else(|| {
                self.err("environments must be an object of named targets")
            })?;
            for (name, entry) in envs {
                let entry = entry.as_object().ok_or_else(|| {
                    self.err(format!("environments.{name} must be an object"))
                })?;
                check_object(
                    entry,
                    &format!("environments.{name}"),
                    generated::ENVIRONMENT_KNOWN_KEYS,
                    generated::ENVIRONMENT_REQUIRED_KEYS,
                )
                .map_err(|e| {
                    self.err(format!(
                        "{e}\n\
                         `app` and `control` are NON-INHERITABLE: an environment that names a \
                         control and inherits the root app is exactly the silent cross-targeting \
                         this rule exists to prevent."
                    ))
                })?;
                check_members(entry, &format!("environments.{name}"))
                    .map_err(|e| self.err(e))?;
                self.reject_dist_containing_config(
                    entry,
                    project_root,
                    &format!("environments.{name}.build.dist"),
                )?;
            }
        }
        Ok(())
    }

    fn reject_dist_containing_config(
        &self,
        value: &Map<String, Value>,
        project_root: &Path,
        field: &str,
    ) -> Result<(), String> {
        let Some(dist) = value
            .get("build")
            .and_then(Value::as_object)
            .and_then(|build| build.get("dist"))
            .and_then(Value::as_str)
        else {
            return Ok(());
        };
        let cwd = std::env::current_dir()
            .map_err(|e| self.err(format!("cannot read the working directory: {e}")))?;
        let root = lexical_normalize(&if project_root.is_absolute() {
            project_root.to_path_buf()
        } else {
            cwd.join(project_root)
        });
        let config = lexical_normalize(&if self.path.is_absolute() {
            self.path.clone()
        } else {
            cwd.join(&self.path)
        });
        let candidate = Path::new(dist);
        let dist_dir = lexical_normalize(&if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            root.join(candidate)
        });
        if root.starts_with(&dist_dir) || config.starts_with(&dist_dir) {
            return Err(self.err(format!(
                "`{field}` ({dist}) cannot resolve to the project root or an ancestor containing \
                 {CONFIG_FILENAME}"
            )));
        }
        Ok(())
    }

    /// A `password` / `token` / `secret` key ANYWHERE in the tree is a parse
    /// error naming where the value belongs. `additionalProperties:
    /// false` already rejects these at every level; this exists so the message
    /// is the one the creator needs rather than "unknown key".
    fn reject_forbidden_names(&self, value: &Value, path: &str) -> Result<(), String> {
        match value {
            Value::Object(map) => {
                for (k, v) in map {
                    if generated::FORBIDDEN_KEY_NAMES.contains(&k.as_str()) {
                        let at = if path.is_empty() {
                            k.clone()
                        } else {
                            format!("{path}.{k}")
                        };
                        return Err(self.err(format!(
                            "`{at}` may not appear in this file - it is tracked, and a \
                             plaintext secret in a tracked file is unrecoverable once committed. \
                             Use `zeroship secret set {}=<value> --app=<id>` for a deployed \
                             value, or `.env` for a dev one, and declare only the NAME here \
                             under `secrets`.",
                            k.to_uppercase()
                        )));
                    }
                    let child = if path.is_empty() {
                        k.clone()
                    } else {
                        format!("{path}.{k}")
                    };
                    self.reject_forbidden_names(v, &child)?;
                }
                Ok(())
            }
            Value::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    self.reject_forbidden_names(v, &format!("{path}[{i}]"))?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Resolve the effective target, applying the named environment overlay.
    ///
    /// `app` and `control` come from the environment ALONE when one is selected
    /// (they are non-inheritable and the schema requires both). `build`,
    /// `migrations` and `secrets` are merged member by member over the root.
    pub fn resolve(&self, environment: Option<&str>) -> Result<Resolved, String> {
        let mut out = self.root.clone();
        out.remove("$schema");
        out.remove("environments");

        let mut origin = Source::File;
        if let Some(name) = environment {
            let entry = self
                .root
                .get("environments")
                .and_then(|e| e.get(name))
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    let known: Vec<&str> = self
                        .root
                        .get("environments")
                        .and_then(Value::as_object)
                        .map(|m| m.keys().map(String::as_str).collect())
                        .unwrap_or_default();
                    self.err(format!(
                        "--env={name} names no environment (declared: {})",
                        if known.is_empty() {
                            "none".to_string()
                        } else {
                            known.join(", ")
                        }
                    ))
                })?;
            for (k, v) in entry {
                match (out.get(k), v) {
                    (Some(Value::Object(base)), Value::Object(over)) => {
                        let mut merged = base.clone();
                        for (mk, mv) in over {
                            merged.insert(mk.clone(), mv.clone());
                        }
                        out.insert(k.clone(), Value::Object(merged));
                    }
                    _ => {
                        out.insert(k.clone(), v.clone());
                    }
                }
            }
            origin = Source::FileEnvironment(name.to_string());
        }

        for (path, json) in generated::RESOLVED_OPTIONAL_DEFAULTS_JSON {
            let value = serde_json::from_str(json).map_err(|e| {
                self.err(format!("generated default for `{path}` is invalid JSON: {e}"))
            })?;
            insert_default(&mut out, path, value).map_err(|e| self.err(e))?;
        }

        Ok(Resolved {
            path: self.path.clone(),
            value: out,
            origin,
        })
    }

    /// Splice a new `app` value into the file, preserving every other byte.
    ///
    /// Returns `Spliced` when the member existed and was rewritten, or
    /// `PrintInstead` when it did not. Inserting a member into arbitrary JSONC
    /// is where round-trip libraries get ugly - which comment does the new
    /// member sit under, what indentation, before or after the blank line - so
    /// the CLI refuses and hands the creator the exact line.
    pub fn write_app(&self, app_id: &str) -> Result<WriteOutcome, String> {
        let stripped = jsonc::strip(&self.text);
        let Some((start, end)) = jsonc::top_level_value_span(&stripped, "app") else {
            return Ok(WriteOutcome::PrintInstead);
        };
        let mut next = String::with_capacity(self.text.len() + app_id.len());
        next.push_str(&self.text[..start]);
        next.push_str(&serde_json::to_string(app_id).map_err(|e| e.to_string())?);
        next.push_str(&self.text[end..]);
        // Re-parse before writing: a splice that produced an unreadable file
        // would be discovered by the NEXT command, in a working tree the
        // creator did not change.
        ProjectConfig::parse(self.path.clone(), next.clone())
            .map_err(|e| format!("refusing to write a file that would not parse: {e}"))?;
        std::fs::write(&self.path, &next)
            .map_err(|e| format!("failed to write {}: {e}", self.path.display()))?;
        Ok(WriteOutcome::Spliced)
    }
}

fn insert_default(
    map: &mut Map<String, Value>,
    dotted: &str,
    value: Value,
) -> Result<(), String> {
    let Some((head, tail)) = dotted.split_once('.') else {
        map.entry(dotted.to_string()).or_insert(value);
        return Ok(());
    };
    let block = map
        .entry(head.to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| format!("generated default `{dotted}` has non-object parent `{head}`"))?;
    insert_default(block, tail, value)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(std::path::MAIN_SEPARATOR_STR),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    Spliced,
    PrintInstead,
}

fn check_object(
    map: &Map<String, Value>,
    path: &str,
    known: &[&str],
    required: &[&str],
) -> Result<(), String> {
    let at = |k: &str| {
        if path.is_empty() {
            k.to_string()
        } else {
            format!("{path}.{k}")
        }
    };
    for k in map.keys() {
        if !known.contains(&k.as_str()) {
            return Err(format!(
                "unknown key `{}` (known: {})",
                at(k),
                known.join(", ")
            ));
        }
    }
    for k in required {
        if !map.contains_key(*k) {
            return Err(format!("missing required key `{}`", at(k)));
        }
    }
    Ok(())
}

/// Type, pattern and enum checks for the members of one object.
///
/// The patterns are hand-written rather than compiled from the schema string:
/// the CLI links no regex crate, and six fixed shapes do not justify one. The
/// generated `FIELD_PATTERNS` table is what keeps the hand-written arms
/// honest - a pattern in the schema with no arm here is a gate failure.
fn check_members(map: &Map<String, Value>, path: &str) -> Result<(), String> {
    let at = |k: &str| {
        if path.is_empty() {
            k.to_string()
        } else {
            format!("{path}.{k}")
        }
    };
    for (k, v) in map {
        // Members inside an `environments.<name>` block carry the same dotted
        // paths as the root ones, so one table serves both.
        let dotted = k.as_str();
        match dotted {
            "name" => {
                let s = as_str(v, &at(k))?;
                if !matches_name(s) {
                    return Err(format!(
                        "`{}` must match {} (got `{s}`)",
                        at(k),
                        pattern_for("name")
                    ));
                }
            }
            "runtime_date" => {
                let s = as_str(v, &at(k))?;
                if !matches_iso_date(s) {
                    return Err(format!(
                        "`{}` must be an ISO date matching {} (got `{s}`)",
                        at(k),
                        pattern_for("runtime_date")
                    ));
                }
            }
            "app" | "control" => {
                as_str(v, &at(k))?;
            }
            "protected" => {
                if !v.is_boolean() {
                    return Err(format!("`{}` must be a boolean", at(k)));
                }
            }
            "secrets" => {
                let items = v
                    .as_array()
                    .ok_or_else(|| format!("`{}` must be an array of secret NAMES", at(k)))?;
                for item in items {
                    let s = as_str(item, &at(k))?;
                    if !matches_secret_name(s) {
                        return Err(format!(
                            "`{}` entry `{s}` must match {} - this array holds NAMES, never \
                             values",
                            at(k),
                            pattern_for("secrets[]")
                        ));
                    }
                }
            }
            "build" => {
                let inner = v
                    .as_object()
                    .ok_or_else(|| format!("`{}` must be an object", at(k)))?;
                let required: &[&str] = if path.is_empty() {
                    generated::BUILD_REQUIRED_KEYS
                } else {
                    &[]
                };
                check_object(inner, &at(k), generated::BUILD_KNOWN_KEYS, required)?;
                if let Some(mode) = inner.get("mode") {
                    let s = as_str(mode, &format!("{}.mode", at(k)))?;
                    let allowed = enum_for("build.mode");
                    if !allowed.contains(&s) {
                        return Err(format!(
                            "`{}.mode` must be one of {} (got `{s}`)",
                            at(k),
                            allowed.join(" | ")
                        ));
                    }
                }
                for m in ["serverEntry", "dist", "output"] {
                    if let Some(x) = inner.get(m) {
                        as_str(x, &format!("{}.{m}", at(k)))?;
                    }
                }
            }
            "migrations" => {
                let inner = v
                    .as_object()
                    .ok_or_else(|| format!("`{}` must be an object", at(k)))?;
                let required: &[&str] = if path.is_empty() {
                    generated::MIGRATIONS_REQUIRED_KEYS
                } else {
                    &[]
                };
                check_object(inner, &at(k), generated::MIGRATIONS_KNOWN_KEYS, required)?;
                for m in ["dir", "out"] {
                    if let Some(x) = inner.get(m) {
                        as_str(x, &format!("{}.{m}", at(k)))?;
                    }
                }
            }
            "$schema" | "environments" => {}
            _ => {}
        }
    }
    Ok(())
}

fn as_str<'a>(v: &'a Value, at: &str) -> Result<&'a str, String> {
    v.as_str()
        .ok_or_else(|| format!("`{at}` must be a string"))
}

fn pattern_for(path: &str) -> &'static str {
    generated::FIELD_PATTERNS
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, rx)| *rx)
        .unwrap_or("<no pattern in schema>")
}

fn enum_for(path: &str) -> Vec<&'static str> {
    generated::FIELD_ENUMS
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, vs)| vs.to_vec())
        .unwrap_or_default()
}

fn matches_name(s: &str) -> bool {
    // ^[a-z0-9][a-z0-9-]{0,62}$
    let b = s.as_bytes();
    (1..=63).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn matches_iso_date(s: &str) -> bool {
    // ^[0-9]{4}-[0-9]{2}-[0-9]{2}$
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && [0, 1, 2, 3, 5, 6, 8, 9]
            .iter()
            .all(|i| b[*i].is_ascii_digit())
}

fn matches_secret_name(s: &str) -> bool {
    // ^[A-Z][A-Z0-9_]{0,63}$
    let b = s.as_bytes();
    (1..=64).contains(&b.len())
        && b[0].is_ascii_uppercase()
        && b.iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

// ---------------------------------------------------------------------------
// The resolved view
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Resolved {
    pub path: PathBuf,
    pub origin: Source,
    value: Map<String, Value>,
}

impl Resolved {
    /// A dotted-path lookup. `None` when the key is absent - there is no
    /// fallback on this side.
    pub fn get(&self, dotted: &str) -> Option<&Value> {
        let mut cursor = self.value.get(dotted.split('.').next()?)?;
        for seg in dotted.split('.').skip(1) {
            cursor = cursor.get(seg)?;
        }
        Some(cursor)
    }

    pub fn str(&self, dotted: &str) -> Option<&str> {
        self.get(dotted).and_then(Value::as_str)
    }

    /// The value, or an error NAMING the key. The CLI never guesses a
    /// cross-tool fact.
    pub fn require(&self, dotted: &str) -> Result<&str, String> {
        self.str(dotted).ok_or_else(|| {
            format!(
                "{} does not set `{dotted}`, and `zeroship` has no default for it.\n\
                 Add it to the file (the scaffold writes every cross-tool key explicitly), \
                 or pass the matching flag. A guess here would disagree with the build, \
                 which is the drift this file exists to remove.\n\
                 Fields with no CLI-side fallback: {}.",
                self.path.display(),
                generated::CLI_READ_FIELDS.join(", ")
            )
        })
    }

    pub fn is_protected(&self) -> bool {
        self.value
            .get("protected")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// The resolved config as canonical JSON: object keys sorted, compact.
    ///
    /// Byte-compared against the TypeScript reader's dump by
    /// `tests/project_config_gate.sh`. That single comparison catches divergent
    /// defaults, silently ignored keys and type-coercion differences at once,
    /// without a second parser being written to catch the first.
    pub fn canonical_json(&self) -> String {
        canonical(&Value::Object(self.value.clone()))
    }
}

fn canonical(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let body: Vec<String> = keys
                .iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).expect("key"),
                        canonical(&map[*k])
                    )
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
        Value::Array(items) => {
            let body: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", body.join(","))
        }
        other => serde_json::to_string(other).expect("scalar"),
    }
}

// ---------------------------------------------------------------------------
// Command-facing resolution
// ---------------------------------------------------------------------------

/// One resolved value with the source that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sourced {
    pub value: String,
    pub source: Source,
}

/// `flag > env var > file(selected environment) > file(root) > fallback`.
///
/// `fallback` is `None` for `app`, and `Some("http://localhost:9090")` for
/// `control` - but ONLY when there is no config file. With a file present an
/// absent key is an error naming the key, because a file that says nothing
/// about the control plane and a CLI that quietly picks localhost is how a
/// migration lands on the wrong database.
pub fn resolve_value(
    args: &[String],
    flag: &'static str,
    env_var: Option<&'static str>,
    env_value: Option<String>,
    cfg: Option<&Resolved>,
    key: &str,
    fallback: Option<&str>,
) -> Result<Sourced, String> {
    if let Some(v) = crate::flag_str(args, &format!("{flag}=")) {
        return Ok(Sourced {
            value: v,
            source: Source::Flag(flag),
        });
    }
    if let (Some(name), Some(v)) = (env_var, env_value) {
        return Ok(Sourced {
            value: v,
            source: Source::EnvVar(name),
        });
    }
    if let Some(cfg) = cfg {
        if let Some(v) = cfg.str(key) {
            return Ok(Sourced {
                value: v.to_string(),
                source: cfg.origin.clone(),
            });
        }
        return Err(cfg.require(key).unwrap_err());
    }
    match fallback {
        Some(v) => Ok(Sourced {
            value: v.to_string(),
            source: Source::Fallback,
        }),
        None => Err(format!(
            "{flag}=<value> is required. There is no {CONFIG_FILENAME} in this directory to \
             read it from - create one (see docs/reference/project-config.md) or pass the flag."
        )),
    }
}

/// Print `key = value (source)` on stderr before a mutating call.
pub fn print_provenance(command: &str, pairs: &[(&str, &Sourced)]) {
    for (key, sourced) in pairs {
        eprintln!(
            "zeroship {command}: {key} = {} (from {})",
            sourced.value,
            sourced.source.describe()
        );
    }
}

// ---------------------------------------------------------------------------
// `zeroship config`
// ---------------------------------------------------------------------------

/// `zeroship config show [--config=PATH] [--env=NAME]`.
///
/// Prints the resolved configuration as CANONICAL JSON (object keys sorted,
/// compact) on stdout. It exists for two readers:
///
/// - a creator asking "which app and control plane is this directory pointed
///   at", which is a question the file alone cannot answer once `--env` and the
///   flag precedence are in play;
/// - `tests/project_config_gate.sh`, which byte-compares this output against
///   the TypeScript reader's dump of the same file. That single comparison is
///   what makes two parsers safe.
pub fn cmd_config(args: &[String]) -> Result<(), String> {
    let sub = args.get(2).map(String::as_str).unwrap_or("");
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read the working directory: {e}"))?;
    let path = locate(args, &cwd)?;

    match sub {
        "show" => {
            let path = path.ok_or_else(|| {
                format!(
                    "no {CONFIG_FILENAME} in {}. Create one (docs/reference/project-config.md), \
                     pass --config=<path>, or set {}.",
                    cwd.display(),
                    generated::CONFIG_ENV_VAR
                )
            })?;
            let cfg = ProjectConfig::load(&path)?;
            let resolved = cfg.resolve(crate::flag_str(args, "--env=").as_deref())?;
            println!("{}", resolved.canonical_json());
            Ok(())
        }
        "path" => {
            match path {
                Some(p) => println!("{}", p.display()),
                None => return Err(format!("no {CONFIG_FILENAME} in {}", cwd.display())),
            }
            Ok(())
        }
        _ => Err(
            "Usage:\n  zeroship config show [--config=<path>] [--env=<name>]\n  \
             zeroship config path [--config=<path>]"
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests;
