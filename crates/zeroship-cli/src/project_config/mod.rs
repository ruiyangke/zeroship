//! `zeroship.jsonc` - the creator project configuration, CLI side.
//!
//! SCOPE INVARIANT: this file is
//! read by the `zeroship` CLI and by the build toolchain. It is NEVER read by
//! the runtime, NEVER packed into a `.zship`, and never leaves the creator's
//! machine. The Vite package's archive tests enforce the packing boundary.
//!
//! CLI-read defaults live here only when the schema explicitly marks them safe.
//! Otherwise, when the file is present and an operationally read key is absent,
//! the command errors naming the key. Optional safe defaults are generated from
//! the schema into both readers so their resolved JSON stays byte-identical.
//!
//! WHEN NO FILE IS PRESENT nothing changes: `--flag`, then the environment
//! variable, then the compiled fallback each command already had. A creator in
//! a scratch directory keeps the CLI they have. The restricted-default rule is
//! scoped to "there IS a file and it does not say", where guessing a scalar
//! fact would contradict a written intention.

pub mod generated;
mod jsonc;

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use zeroship_core::AppId;

pub use generated::CONFIG_FILENAME;

pub const DEFAULT_CONTROL_URL: &str = "http://localhost:9090";

/// Printed with every environment-shape refusal, because the rule is the point
/// and the missing key is only how it was broken.
const NON_INHERITABLE_NOTE: &str =
    "`apps`, `control` and `databases` are NON-INHERITABLE, and each map must cover every \
     label the root declares. An environment that names a control and inherits the root app \
     targets the wrong code; one that inherits a database id lands WRITES in the wrong data.";

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
    /// One LABELLED entry of the file, already rendered with whatever origin
    /// produced it: `zeroship.jsonc apps.storefront`, or the same under an
    /// environment. The provenance line then names the entry a creator would
    /// edit rather than the file it lives in.
    FileLabel(String),
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
            Source::FileLabel(rendered) => rendered.clone(),
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
    /// The file's exact bytes. Kept so writeback can edit its CST or original
    /// value span rather than re-serialise it.
    pub text: String,
    root: Map<String, Value>,
    project_root: PathBuf,
}

impl ProjectConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        Self::parse(path.to_path_buf(), text)
    }

    pub fn parse(path: PathBuf, text: String) -> Result<Self, String> {
        let cwd = std::env::current_dir()
            .map_err(|e| format!("cannot read the working directory: {e}"))?;
        let absolute_path = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(&path)
        };
        let project_root = lexical_normalize(
            absolute_path
                .parent()
                .unwrap_or_else(|| Path::new(std::path::MAIN_SEPARATOR_STR)),
        );
        let value: Value = jsonc::parse(&text)
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
        let cfg = ProjectConfig {
            path,
            text,
            root,
            project_root,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn err(&self, msg: impl AsRef<str>) -> String {
        format!("{}: {}", self.path.display(), msg.as_ref())
    }

    fn validate(&self) -> Result<(), String> {
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
        self.reject_unsafe_write_path(
            &self.root,
            "build",
            "dist",
            "build.dist",
            None,
        )?;
        self.reject_unsafe_write_path(
            &self.root,
            "build",
            "output",
            "build.output",
            Some("zship"),
        )?;
        self.check_database_outputs()?;
        self.check_app_wiring()?;

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
                .map_err(|e| self.err(format!("{e}\n{NON_INHERITABLE_NOTE}")))?;
                check_members(entry, &format!("environments.{name}"))
                    .map_err(|e| self.err(e))?;
                self.check_environment_labels(name, entry)?;
                self.reject_unsafe_write_path(
                    entry,
                    "build",
                    "dist",
                    &format!("environments.{name}.build.dist"),
                    None,
                )?;
                self.reject_unsafe_write_path(
                    entry,
                    "build",
                    "output",
                    &format!("environments.{name}.build.output"),
                    Some("zship"),
                )?;
            }
        }
        Ok(())
    }

    /// Every database's gen-types output directory is a real write target, and
    /// no two databases may share one: `env.db.ts` and `schema.runtime.json`
    /// have fixed names, so a shared directory is one database's schema
    /// silently standing in for another's.
    fn check_database_outputs(&self) -> Result<(), String> {
        let mut claimed: Vec<(&str, &str)> = Vec::new();
        for (label, entry) in self.labelled(&self.root, "databases") {
            let out = entry
                .get("out")
                .and_then(Value::as_str)
                .ok_or_else(|| self.err(format!("`databases.{label}.out` must be a string")))?;
            self.reject_unsafe_path(out, &format!("databases.{label}.out"), None)?;
            if let Some((other, _)) = claimed.iter().find(|(_, path)| *path == out) {
                return Err(self.err(format!(
                    "`databases.{label}.out` and `databases.{other}.out` are both `{out}`. \
                     Two databases cannot share one gen-types directory: the filenames in it \
                     are fixed, so one database's schema would overwrite the other's."
                )));
            }
            claimed.push((label, out));
        }
        Ok(())
    }

    /// An app names database LABELS, so every one has to resolve here, and the
    /// primary has to be one of them. Declaring a database does not grant
    /// access to it: `zeroship db bind` does, and deploy verifies the binding.
    fn check_app_wiring(&self) -> Result<(), String> {
        let declared: Vec<&str> = self
            .labelled(&self.root, "databases")
            .map(|(label, _)| label)
            .collect();
        // Which app makes each database its `env.db`, and which merely uses
        // it. Collected across the whole file, because one database has ONE
        // gen-types directory - see `check_primacy_is_uniform`.
        let mut primary_of: Vec<(&str, &str)> = Vec::new();
        let mut secondary_of: Vec<(&str, &str)> = Vec::new();
        for (label, entry) in self.labelled(&self.root, "apps") {
            let used: Vec<&str> = entry
                .get("databases")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    self.err(format!("`apps.{label}.databases` must be an array of labels"))
                })?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            for (index, name) in used.iter().enumerate() {
                if !declared.contains(name) {
                    return Err(self.err(format!(
                        "`apps.{label}.databases` names `{name}`, which this file does not \
                         declare under `databases` (declared: {})",
                        join_or_none(&declared)
                    )));
                }
                if used[..index].contains(name) {
                    return Err(self.err(format!(
                        "`apps.{label}.databases` names `{name}` twice"
                    )));
                }
            }
            match entry.get("primary").and_then(Value::as_str) {
                Some(primary) if used.is_empty() => {
                    return Err(self.err(format!(
                        "`apps.{label}.primary` is `{primary}`, but `apps.{label}.databases` is \
                         empty. An app with no database has no primary and no `env.db`."
                    )));
                }
                Some(primary) if !used.contains(&primary) => {
                    return Err(self.err(format!(
                        "`apps.{label}.primary` is `{primary}`, which is not one of \
                         `apps.{label}.databases` ({})",
                        join_or_none(&used)
                    )));
                }
                Some(_) => {}
                None if used.is_empty() => {}
                None => {
                    return Err(self.err(format!(
                        "`apps.{label}` uses {} but names no `primary`. The primary is \
                         `env.db`, and `env.db === env.databases[primary]` by object identity, \
                         so it cannot be inferred.",
                        join_or_none(&used)
                    )));
                }
            }
            let primary = entry.get("primary").and_then(Value::as_str);
            for name in used {
                let bucket = if Some(name) == primary {
                    &mut primary_of
                } else {
                    &mut secondary_of
                };
                if !bucket.iter().any(|(database, _)| *database == name) {
                    bucket.push((name, label));
                }
            }
        }
        self.check_primacy_is_uniform(&primary_of, &secondary_of)
    }

    /// A database is the `env.db` of every app that uses it, or of none.
    ///
    /// ONE DATABASE HAS ONE GEN-TYPES DIRECTORY (`databases.<label>.out`,
    /// which [`Self::check_database_outputs`] already refuses to share), so it
    /// has ONE `env.db.ts`. That module declares the database's entry on
    /// `EnvDatabases` under its label always, and declares `Env.db` only when
    /// it is the primary. A workspace where one app makes `analytics` its
    /// primary and another merely uses it is asking that single file to be two
    /// different files: whichever app built last wins, and the other's
    /// `env.db` is typed as the wrong database or not at all.
    ///
    /// Refused here rather than discovered as a drift-gate failure, because a
    /// `--check` complaining that `env.db.ts` drifted names the artifact and
    /// not the two lines of configuration that cannot both hold.
    fn check_primacy_is_uniform(
        &self,
        primary_of: &[(&str, &str)],
        secondary_of: &[(&str, &str)],
    ) -> Result<(), String> {
        for (database, app) in primary_of {
            let Some((_, other)) = secondary_of.iter().find(|(name, _)| name == database) else {
                continue;
            };
            return Err(self.err(format!(
                "`apps.{app}` makes `{database}` its `primary` while `apps.{other}` uses it \
                 without naming it. A database has ONE generated `env.db.ts` \
                 (`databases.{database}.out`), and that file declares `Env.db` only for a \
                 primary, so the two apps cannot both be typed from it. Give one of them its \
                 own database, or make `{database}` the primary of both."
            )));
        }
        Ok(())
    }

    /// An environment's `apps` and `databases` must cover every label the root
    /// declares, and no others.
    ///
    /// Partial coverage is the whole hazard: an environment that names a
    /// production control and inherits a development database id lands WRITES
    /// in the wrong place. Requiring the key rather than the whole entry is
    /// what keeps the LABEL local - an environment overrides an id, never a
    /// label, so the manifest and the generated client are the same artifact
    /// across environments.
    fn check_environment_labels(
        &self,
        name: &str,
        entry: &Map<String, Value>,
    ) -> Result<(), String> {
        for section in ["apps", "databases"] {
            let declared: Vec<&str> = self
                .labelled(&self.root, section)
                .map(|(label, _)| label)
                .collect();
            let overridden: Vec<&str> = self
                .labelled(entry, section)
                .map(|(label, _)| label)
                .collect();
            for label in &declared {
                if !overridden.contains(label) {
                    return Err(self.err(format!(
                        "`environments.{name}.{section}` does not name `{label}`, which the \
                         root declares.\n{NON_INHERITABLE_NOTE}"
                    )));
                }
            }
            for label in &overridden {
                if !declared.contains(label) {
                    return Err(self.err(format!(
                        "`environments.{name}.{section}.{label}` names no root `{section}` \
                         entry (declared: {}). An environment overrides the id under a label, \
                         never the label itself.",
                        join_or_none(&declared)
                    )));
                }
            }
        }
        Ok(())
    }

    /// The entries of one label map, in file order. A map the file omits is
    /// empty here; its presence is `check_object`'s business, not this one's.
    fn labelled<'a>(
        &self,
        map: &'a Map<String, Value>,
        section: &str,
    ) -> impl Iterator<Item = (&'a str, &'a Map<String, Value>)> {
        map.get(section)
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|entries| {
                entries
                    .iter()
                    .filter_map(|(label, entry)| Some((label.as_str(), entry.as_object()?)))
            })
    }

    fn reject_unsafe_write_path(
        &self,
        value: &Map<String, Value>,
        section: &str,
        member: &str,
        field: &str,
        existing_artifact_extension: Option<&str>,
    ) -> Result<(), String> {
        let Some(configured_path) = value
            .get(section)
            .and_then(Value::as_object)
            .and_then(|block| block.get(member))
            .and_then(Value::as_str)
        else {
            return Ok(());
        };
        self.reject_unsafe_path(configured_path, field, existing_artifact_extension)
    }

    /// The path-safety rule itself: a configured write target may not resolve
    /// to the project root, to an ancestor holding the config file, or over an
    /// existing file that is not the artifact kind named.
    fn reject_unsafe_path(
        &self,
        configured_path: &str,
        field: &str,
        existing_artifact_extension: Option<&str>,
    ) -> Result<(), String> {
        let cwd = std::env::current_dir()
            .map_err(|e| self.err(format!("cannot read the working directory: {e}")))?;
        let lexical_root = self.project_root.clone();
        let lexical_config = lexical_normalize(&if self.path.is_absolute() {
            self.path.clone()
        } else {
            cwd.join(&self.path)
        });
        let root = canonicalize_existing_prefix(&lexical_root).map_err(|e| {
            self.err(format!("cannot resolve the project root for `{field}`: {e}"))
        })?;
        let config = canonicalize_existing_prefix(&lexical_config).map_err(|e| {
            self.err(format!("cannot resolve {CONFIG_FILENAME} for `{field}`: {e}"))
        })?;
        let candidate = Path::new(configured_path);
        let lexical_resolved = lexical_normalize(&if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            root.join(candidate)
        });
        let resolved = canonicalize_existing_prefix(&lexical_resolved).map_err(|e| {
            self.err(format!("cannot resolve `{field}` ({configured_path}): {e}"))
        })?;
        if root.starts_with(&resolved) || config.starts_with(&resolved) {
            return Err(self.err(format!(
                "`{field}` ({configured_path}) cannot resolve to the project root or an ancestor containing \
                 {CONFIG_FILENAME}"
            )));
        }
        if let Some(extension) = existing_artifact_extension {
            match std::fs::symlink_metadata(&lexical_resolved) {
                Ok(metadata) => {
                    let is_artifact_file = metadata.is_file()
                        && !metadata.file_type().is_symlink()
                        && resolved.extension().and_then(|value| value.to_str())
                            == Some(extension);
                    if !is_artifact_file {
                        return Err(self.err(format!(
                            "`{field}` ({configured_path}) resolves to existing non-artifact file {}; \
                             refusing to overwrite creator data",
                            resolved.display()
                        )));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(self.err(format!(
                        "cannot inspect `{field}` ({configured_path}) at {}: {error}",
                        lexical_resolved.display()
                    )));
                }
            }
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
                match (k.as_str(), out.get(k), v) {
                    // `apps` and `databases` are maps of labels, and an
                    // environment overrides the ID under a label, never the
                    // label and never the build-time paths beside it. So the
                    // merge runs one level deeper here than anywhere else: a
                    // whole-entry replace would drop `migrations`, `out`,
                    // `databases` and `primary` on the floor.
                    ("apps" | "databases", Some(Value::Object(base)), Value::Object(over)) => {
                        let mut merged = base.clone();
                        for (label, overridden) in over {
                            let mut entry = merged
                                .get(label)
                                .and_then(Value::as_object)
                                .cloned()
                                .unwrap_or_default();
                            if let Some(members) = overridden.as_object() {
                                for (mk, mv) in members {
                                    entry.insert(mk.clone(), mv.clone());
                                }
                            }
                            merged.insert(label.clone(), Value::Object(entry));
                        }
                        out.insert(k.clone(), Value::Object(merged));
                    }
                    (_, Some(Value::Object(base)), Value::Object(over)) => {
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
            project_root: self.project_root.clone(),
        })
    }

    /// Write one app label's `app` id while preserving the creator's JSONC
    /// formatting.
    ///
    /// An existing value is replaced at its original byte span. A missing
    /// member is appended through the CST, which retains comments, key order,
    /// interior blank lines, trailing commas, newlines, and source text. The
    /// CST deliberately normalises extra blank lines touching the root braces.
    pub fn write_app_id(&self, label: &str, app_id: &AppId) -> Result<(), String> {
        let app_id = app_id.as_str();
        let parents = ["apps", label];
        let next = if let Some((start, end)) =
            jsonc::member_value_span(&self.text, &parents, "app")
        {
            let mut next = String::with_capacity(self.text.len() + app_id.len());
            next.push_str(&self.text[..start]);
            next.push_str(&serde_json::to_string(app_id).map_err(|e| e.to_string())?);
            next.push_str(&self.text[end..]);
            next
        } else {
            jsonc::append_member_string(&self.text, &parents, "app", app_id)
                .map_err(|e| format!("failed to append `apps.{label}.app`: {e}"))?
        };
        // Re-parse before writing: an edit that produced an unreadable file
        // would be discovered by the NEXT command, in a working tree the
        // creator did not change.
        ProjectConfig::parse(self.path.clone(), next.clone())
            .map_err(|e| format!("refusing to write a file that would not parse: {e}"))?;
        let current = std::fs::read(&self.path)
            .map_err(|e| format!("failed to re-read {} before writing: {e}", self.path.display()))?;
        if current != self.text.as_bytes() {
            return Err(format!(
                "{} changed since it was loaded; refusing to overwrite it",
                self.path.display()
            ));
        }
        std::fs::write(&self.path, &next)
            .map_err(|e| format!("failed to write {}: {e}", self.path.display()))?;
        Ok(())
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

fn canonicalize_existing_prefix(path: &Path) -> std::io::Result<PathBuf> {
    let mut cursor = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(&cursor) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(lexical_normalize(&resolved));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(component) = cursor.file_name().map(ToOwned::to_owned) else {
                    return Err(error);
                };
                let Some(parent) = cursor.parent().map(Path::to_path_buf) else {
                    return Err(error);
                };
                missing.push(component);
                cursor = parent;
            }
            Err(error) => return Err(error),
        }
    }
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
            "control" => {
                as_str(v, &at(k))?;
            }
            "databases" => {
                let (known, required) = if path.is_empty() {
                    (
                        generated::DATABASE_KNOWN_KEYS,
                        generated::DATABASE_REQUIRED_KEYS,
                    )
                } else {
                    (
                        generated::ENVIRONMENT_DATABASE_KNOWN_KEYS,
                        generated::ENVIRONMENT_DATABASE_REQUIRED_KEYS,
                    )
                };
                for (label, entry) in check_label_map(v, &at(k), "databases.*")? {
                    let at_entry = format!("{}.{label}", at(k));
                    check_object(entry, &at_entry, known, required)?;
                    let id = as_str(&entry["id"], &format!("{at_entry}.id"))?;
                    if !matches_database_id(id) {
                        return Err(format!(
                            "`{at_entry}.id` must match {} (got `{id}`). \
                             `zeroship db create` prints it.",
                            pattern_for("databases.*.id")
                        ));
                    }
                    for m in ["migrations", "out"] {
                        if let Some(x) = entry.get(m) {
                            as_str(x, &format!("{at_entry}.{m}"))?;
                        }
                    }
                }
            }
            "apps" => {
                let (known, required) = if path.is_empty() {
                    (generated::APP_KNOWN_KEYS, generated::APP_REQUIRED_KEYS)
                } else {
                    (
                        generated::ENVIRONMENT_APP_KNOWN_KEYS,
                        generated::ENVIRONMENT_APP_REQUIRED_KEYS,
                    )
                };
                for (label, entry) in check_label_map(v, &at(k), "apps.*")? {
                    let at_entry = format!("{}.{label}", at(k));
                    check_object(entry, &at_entry, known, required)?;
                    if let Some(app) = entry.get("app") {
                        as_str(app, &format!("{at_entry}.app"))?;
                    }
                    if let Some(primary) = entry.get("primary") {
                        as_str(primary, &format!("{at_entry}.primary"))?;
                    }
                    let Some(used) = entry.get("databases") else {
                        continue;
                    };
                    let used = used.as_array().ok_or_else(|| {
                        format!("`{at_entry}.databases` must be an array of database LABELS")
                    })?;
                    for item in used {
                        let name = as_str(item, &format!("{at_entry}.databases"))?;
                        if !matches_label(name) {
                            return Err(format!(
                                "`{at_entry}.databases` entry `{name}` must match {}",
                                label_pattern_for("databases.*")
                            ));
                        }
                    }
                }
            }
            "protected" if !v.is_boolean() => {
                return Err(format!("`{}` must be a boolean", at(k)));
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

/// One label and the entry it names, out of a label-keyed map.
///
/// Named because the pair is returned from several checks and a bare tuple of
/// two references reads as noise at each of them.
type LabelledEntry<'a> = (&'a str, &'a Map<String, Value>);

/// Read one label map, checking every key against the rule the schema states.
fn check_label_map<'a>(
    value: &'a Value,
    at: &str,
    map_path: &str,
) -> Result<Vec<LabelledEntry<'a>>, String> {
    let entries = value
        .as_object()
        .ok_or_else(|| format!("`{at}` must be an object keyed by LOCAL LABELS"))?;
    entries
        .iter()
        .map(|(label, entry)| {
            if !matches_label(label) {
                return Err(format!(
                    "`{at}.{label}` is not a usable label: it must match {}. A label is a \
                     member name on `env.databases` as well as a key here.",
                    label_pattern_for(map_path)
                ));
            }
            let entry = entry
                .as_object()
                .ok_or_else(|| format!("`{at}.{label}` must be an object"))?;
            Ok((label.as_str(), entry))
        })
        .collect()
}

fn join_or_none(names: &[&str]) -> String {
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
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

fn label_pattern_for(path: &str) -> &'static str {
    generated::LABEL_PATTERNS
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, rx)| *rx)
        .unwrap_or("<no label pattern in schema>")
}

fn matches_label(s: &str) -> bool {
    // ^[a-z][a-z0-9_]{0,31}$
    let b = s.as_bytes();
    (1..=32).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

fn matches_database_id(s: &str) -> bool {
    // ^dbs_[0-9a-z]{25}$
    let b = s.as_bytes();
    b.len() == 29
        && s.starts_with("dbs_")
        && b[4..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
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
    project_root: PathBuf,
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

    /// Resolve a path read from the config against that config's directory.
    pub fn require_path(&self, dotted: &str) -> Result<PathBuf, String> {
        let configured = Path::new(self.require(dotted)?);
        Ok(lexical_normalize(&if configured.is_absolute() {
            configured.to_path_buf()
        } else {
            self.project_root.join(configured)
        }))
    }

    /// The app labels this file declares, in the order the file states them.
    #[must_use]
    pub fn app_labels(&self) -> Vec<&str> {
        self.labels("apps")
    }

    /// The database labels this file declares, in the order the file states them.
    #[must_use]
    pub fn database_labels(&self) -> Vec<&str> {
        self.labels("databases")
    }

    fn labels(&self, section: &str) -> Vec<&str> {
        self.value
            .get(section)
            .and_then(Value::as_object)
            .map(|entries| entries.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// Dereference a database label to its `dbs_` id.
    ///
    /// This is the whole reason a label is local: the CLI resolves it HERE,
    /// before any request, so nothing on a wire ever carries a name two
    /// workspaces could both choose.
    pub fn database_id(&self, label: &str) -> Result<&str, String> {
        self.str(&format!("databases.{label}.id")).ok_or_else(|| {
            format!(
                "{} declares no `databases.{label}` (declared: {})",
                self.path.display(),
                join_or_none(&self.database_labels())
            )
        })
    }

    /// The database labels one app uses, in declaration order.
    #[must_use]
    pub fn app_databases(&self, label: &str) -> Vec<&str> {
        self.get(&format!("apps.{label}.databases"))
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }

    /// The label of one app's primary database, which is its `env.db`.
    #[must_use]
    pub fn app_primary(&self, label: &str) -> Option<&str> {
        self.str(&format!("apps.{label}.primary"))
    }

    /// Resolve one database's `migrations` or `out` path against the config's
    /// own directory.
    pub fn database_path(&self, label: &str, member: &str) -> Result<PathBuf, String> {
        self.require_path(&format!("databases.{label}.{member}"))
    }

    pub fn is_protected(&self) -> bool {
        self.value
            .get("protected")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// The resolved config as canonical JSON: object keys sorted, compact.
    ///
    /// The CLI unit tests pin the output shape. The build-side reader owns its
    /// corresponding canonicalization cases.
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
/// A fallback is used ONLY when there is no config file. With a file present an
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
    if let Some(v) = crate::parse_flag(args, flag) {
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

/// Which declared app a command targets, and where its id came from.
#[derive(Debug, Clone)]
pub struct AppSelection {
    /// The `apps` label, when a config file declares one. `None` only when
    /// there is no file, because labels exist nowhere else.
    pub label: Option<String>,
    /// The app id, when one is known. `None` on a fresh project whose entry
    /// carries no id yet, which is what `zeroship deploy` auto-creates into.
    pub id: Option<Sourced>,
}

/// Choose the app a command targets.
///
/// **With a config file present `--app` names a LABEL**, because the file is
/// the namespace the CLI resolves in: the id comes from the file, so a label
/// never travels as an identifier and a typo names the labels that exist. With
/// NO file there are no labels, so `--app` is an app id exactly as before. The
/// two cases are told apart by whether there is a file, never by inspecting
/// the value.
pub fn select_app(args: &[String], cfg: Option<&Resolved>) -> Result<AppSelection, String> {
    let flag = crate::parse_flag(args, "--app");
    let Some(cfg) = cfg else {
        return match flag {
            Some(value) => Ok(AppSelection {
                label: None,
                id: Some(Sourced {
                    value,
                    source: Source::Flag("--app"),
                }),
            }),
            None => Err(format!(
                "--app=<id> is required. There is no {CONFIG_FILENAME} in this directory to \
                 read it from - create one (see docs/reference/project-config.md) or pass the \
                 flag."
            )),
        };
    };

    let declared = cfg.app_labels();
    let label = match flag {
        Some(named) => {
            if !declared.contains(&named.as_str()) {
                return Err(format!(
                    "--app={named} names no app in {} (declared: {}).\n\
                     With a {CONFIG_FILENAME} present `--app` names one of its labels: the id \
                     comes from the file, so a label never travels as an identifier.",
                    cfg.path.display(),
                    join_or_none(&declared)
                ));
            }
            named
        }
        None => match declared.as_slice() {
            [only] => (*only).to_string(),
            [] => {
                return Err(format!(
                    "{} declares no apps. Add one under `apps`.",
                    cfg.path.display()
                ))
            }
            many => {
                return Err(format!(
                    "{} declares more than one app ({}). Pass --app=<label> to say which.",
                    cfg.path.display(),
                    many.join(", ")
                ))
            }
        },
    };

    let id = cfg
        .str(&format!("apps.{label}.app"))
        .map(|value| Sourced {
            value: value.to_string(),
            source: Source::FileLabel(format!("{} apps.{label}", cfg.origin.describe())),
        });
    Ok(AppSelection {
        label: Some(label),
        id,
    })
}

/// Choose which declared database a command addresses.
///
/// **The same rule as [`select_app`], one section over**: `--database=<label>`
/// names one of the file's `databases` entries, a workspace declaring exactly
/// one implies it, and a workspace declaring several is asked which rather than
/// guessed at. A command that picked one of several would write a schema into a
/// database the creator did not name, which no message afterwards can undo.
///
/// **No app narrows the candidates.** A database belongs to its project, not to
/// an app: several apps may bind one and a legitimate database has none bound
/// at all. Scoping the labels to `apps.<app>.databases` would hide exactly
/// those, and `primary` decides only which handle is `env.db` - never which
/// schema is addressable.
pub fn select_database(args: &[String], cfg: &Resolved) -> Result<String, String> {
    let declared = cfg.database_labels();
    match crate::parse_flag(args, "--database") {
        Some(named) => {
            if !declared.contains(&named.as_str()) {
                return Err(format!(
                    "--database={named} names no database in {} (declared: {}).\n\
                     With a {CONFIG_FILENAME} present `--database` names one of its labels: \
                     the id comes from the file, so a label never travels as an identifier.",
                    cfg.path.display(),
                    join_or_none(&declared)
                ));
            }
            Ok(named)
        }
        None => match declared.as_slice() {
            [only] => Ok((*only).to_string()),
            [] => Err(format!(
                "{} declares no databases. Add one under `databases`.",
                cfg.path.display()
            )),
            many => Err(format!(
                "{} declares more than one database ({}). Pass --database=<label> to say which.",
                cfg.path.display(),
                many.join(", ")
            )),
        },
    }
}

/// Resolve the control-plane URL identically for every operational command.
pub fn resolve_control(args: &[String], cfg: Option<&Resolved>) -> Result<Sourced, String> {
    resolve_value(
        args,
        "--control",
        Some("ZEROSHIP_CONTROL_URL"),
        zeroship_core::declared_env!(cli, "ZEROSHIP_CONTROL_URL", crate::ZeroshipCliConsumer),
        cfg,
        "control",
        Some(DEFAULT_CONTROL_URL),
    )
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
/// Prints the resolved configuration as canonical JSON on stdout. It lets a
/// creator ask which app and control plane this directory points at, which the
/// file alone cannot answer once `--env` and flag precedence are in play.
///
/// The build-side reader has its own generated schema contract and parser
/// cases; `config show` remains a creator-facing diagnostic rather than a test
/// protocol between the implementations.
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
