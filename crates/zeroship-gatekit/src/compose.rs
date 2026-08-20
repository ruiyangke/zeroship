//! A shared, reusable model of a `docker compose` file.
//!
//! WHY THIS EXISTS. Surveyed 2026-08-20: four shell gates
//! (`compose_secret_strength_gate.sh`, `compose_stateful_volume_gate.sh`,
//! `compose_port_exposure_gate.sh`, `compose_workflow_advance_flag_gate.sh`)
//! plus `deploy/scripts/deploy-remote.sh` hand-roll SIX separate compose
//! "parsers" out of `grep`/`sed` over indentation. Every one of them is a
//! different approximation of YAML, and each has to be re-audited on its own.
//! One of those approximations - the secret gate's rule regex - is what went
//! silently blind on 2026-08-13.
//!
//! This module is the substrate those gates collapse onto: a real YAML parse
//! into typed structures, so a gate asks the model a question instead of
//! guessing at text. It is deliberately PARTIAL - it models the keys the gates
//! read and lets `serde` ignore the rest - but it is partial by declaration
//! rather than by accident, and unknown shapes surface as parse errors on the
//! keys it does claim.
//!
//! It models the FILE, not what `docker compose` would resolve it to. No
//! variable substitution is performed and no `.env` is read: a gate about what
//! a repository SHIPS must see `${FOO:-weak}` as a shipped default, which is
//! exactly what a resolved view would hide.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// A parsed compose file.
#[derive(Debug, Clone, Deserialize)]
pub struct ComposeFile {
    /// Services, keyed by service name. Order is the file's, normalised to
    /// sorted order so a gate's output is stable across runs.
    #[serde(default)]
    pub services: BTreeMap<String, Service>,
    /// Top-level named volumes. A key with a null body (`myvol:`) is the
    /// idiomatic "default driver" spelling, so the value is unmodelled.
    #[serde(default)]
    pub volumes: BTreeMap<String, serde_norway::Value>,
    /// Where this file was read from, for diagnostics. Not a compose key.
    #[serde(skip)]
    path: PathBuf,
}

/// One compose service.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Service {
    /// `image:`, when the service is not built from a `build:` context.
    #[serde(default)]
    pub image: Option<String>,
    /// `environment:`, in either of the two spellings compose accepts.
    #[serde(default)]
    pub environment: Environment,
    /// `ports:`, verbatim. Short and long syntax are not normalised here.
    #[serde(default)]
    pub ports: Vec<serde_norway::Value>,
    /// `volumes:`, verbatim bind/named-volume specifications.
    #[serde(default)]
    pub volumes: Vec<String>,
    /// `command:`, as written (a string or an argv list).
    #[serde(default)]
    pub command: Option<serde_norway::Value>,
}

/// `environment:` accepts a mapping (`FOO: bar`) or a list (`- FOO=bar`).
///
/// Both are modelled because both are legal and a gate that only understood
/// one would silently see nothing in a file using the other - the failure mode
/// this crate exists to end.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
pub enum Environment {
    /// `FOO: bar` mapping form. A null value (`FOO:`) means "pass through from
    /// the host environment" and is modelled as `None`.
    Mapping(BTreeMap<String, Option<String>>),
    /// `- FOO=bar` list form. An entry with no `=` is a pass-through.
    List(Vec<String>),
    /// The key was absent.
    #[default]
    Absent,
}

impl Environment {
    /// Every `(name, value)` pair, with pass-through entries yielding `None`.
    ///
    /// Normalising both spellings here is what lets a gate be written once.
    #[must_use]
    pub fn entries(&self) -> Vec<(&str, Option<&str>)> {
        match self {
            Self::Mapping(map) => map
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_deref()))
                .collect(),
            Self::List(items) => items
                .iter()
                .map(|item| match item.split_once('=') {
                    Some((name, value)) => (name, Some(value)),
                    None => (item.as_str(), None),
                })
                .collect(),
            Self::Absent => Vec::new(),
        }
    }

    /// The value declared for `name`, if the service declares it at all.
    ///
    /// The outer `Option` is "is it declared"; the inner is "does it have a
    /// value or is it a host pass-through". Collapsing the two is how a gate
    /// ends up treating an unset variable as a checked one.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Option<&str>> {
        self.entries()
            .into_iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
    }
}

/// What a compose value IS, before docker resolves anything.
///
/// The distinction the old shell gate blurred: `${FOO:?msg}` supplies no value
/// at all and cannot be measured, while `${FOO:-weak}` ships `weak` as a
/// built-in default and absolutely can be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeValue<'a> {
    /// No interpolation: the text is the value, verbatim.
    Literal(&'a str),
    /// `${NAME:?reason}` or `${NAME?reason}` - the whole value. Compose
    /// REFUSES to start when `NAME` is unset, so nothing is shipped.
    Required {
        /// The variable an operator must set.
        name: &'a str,
        /// The message compose prints when it is unset.
        reason: &'a str,
    },
    /// `${NAME:-default}` or `${NAME-default}` - the whole value. `default` is
    /// shipped by this file and is used whenever `NAME` is unset.
    Defaulted {
        /// The variable that overrides the default.
        name: &'a str,
        /// The value this file ships.
        default: &'a str,
    },
    /// `${NAME}` - the whole value, with no default and no refusal. An unset
    /// `NAME` silently becomes the empty string.
    Substituted {
        /// The variable substituted in.
        name: &'a str,
    },
    /// An interpolation embedded in surrounding text, or more than one. Real
    /// in this tree (`${ZEROSHIP_SECRETS_DIR:-./secrets}/broker-secret`) and
    /// deliberately NOT decomposed: a gate that wants to reason about one of
    /// these should say so rather than get a half-answer.
    Composite(&'a str),
}

impl<'a> ComposeValue<'a> {
    /// Classify a raw compose value.
    #[must_use]
    pub fn parse(raw: &'a str) -> Self {
        let Some(open) = raw.find("${") else {
            return ComposeValue::Literal(raw);
        };
        // `$${...}` is an escaped literal `${...}`, not an interpolation.
        if open > 0 && raw.as_bytes()[open - 1] == b'$' {
            return ComposeValue::Literal(raw);
        }
        if open != 0 || !raw.ends_with('}') || raw[2..].contains("${") {
            return ComposeValue::Composite(raw);
        }
        let body = &raw[2..raw.len() - 1];
        if body.contains('}') {
            return ComposeValue::Composite(raw);
        }

        // Operators, longest first: `:?` and `:-` must be tried before the
        // bare `?` and `-` they contain.
        for (operator, build) in [
            (
                ":?",
                (|name, rest| ComposeValue::Required { name, reason: rest })
                    as fn(&'a str, &'a str) -> ComposeValue<'a>,
            ),
            ("?", |name, rest| ComposeValue::Required { name, reason: rest }),
            (":-", |name, rest| ComposeValue::Defaulted {
                name,
                default: rest,
            }),
            ("-", |name, rest| ComposeValue::Defaulted {
                name,
                default: rest,
            }),
        ] {
            if let Some((name, rest)) = body.split_once(operator) {
                return build(name, rest);
            }
        }
        ComposeValue::Substituted { name: body }
    }
}

/// Why a compose file could not be turned into a model.
#[derive(Debug)]
pub enum LoadError {
    /// The file could not be read.
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The bytes are not a compose file this model understands.
    Yaml {
        /// The path that failed.
        path: PathBuf,
        /// The parser's message, which carries line and column.
        source: serde_norway::Error,
    },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "read {}: {source}", path.display())
            }
            Self::Yaml { path, source } => {
                write!(f, "parse {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for LoadError {}

impl ComposeFile {
    /// Read and parse a compose file.
    ///
    /// # Errors
    ///
    /// [`LoadError::Io`] when the path cannot be read, [`LoadError::Yaml`]
    /// when the contents are not YAML or do not match the modelled keys.
    pub fn load(path: &Path) -> Result<Self, LoadError> {
        let text = std::fs::read_to_string(path).map_err(|source| LoadError::Io {
            path: path.to_owned(),
            source,
        })?;
        let mut file = Self::from_yaml(&text).map_err(|source| LoadError::Yaml {
            path: path.to_owned(),
            source,
        })?;
        path.clone_into(&mut file.path);
        Ok(file)
    }

    /// Parse compose YAML that is already in memory. Used by tests and by any
    /// caller synthesising a file.
    ///
    /// # Errors
    ///
    /// Returns the parser's error when the text does not match the model.
    pub fn from_yaml(text: &str) -> Result<Self, serde_norway::Error> {
        serde_norway::from_str(text)
    }

    /// The path this file was loaded from (empty for [`Self::from_yaml`]).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every `(service, value)` declaring the environment variable `name`.
    ///
    /// Returns each occurrence rather than deduplicating: two services setting
    /// the SAME secret to different values is a real defect, and a gate cannot
    /// see it through a deduplicated view.
    #[must_use]
    pub fn environment_occurrences(&self, name: &str) -> Vec<(&str, Option<&str>)> {
        self.services
            .iter()
            .filter_map(|(service, spec)| {
                spec.environment
                    .get(name)
                    .map(|value| (service.as_str(), value))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{ComposeFile, ComposeValue, Environment};

    #[test]
    fn both_environment_spellings_read_the_same() {
        let file = ComposeFile::from_yaml(
            "services:\n  \
             mapping:\n    \
             environment:\n      \
             FOO: bar\n      \
             PASSTHROUGH:\n  \
             list:\n    \
             environment:\n      \
             - FOO=bar\n      \
             - PASSTHROUGH\n",
        )
        .expect("parses");

        assert_eq!(
            file.services["mapping"].environment.get("FOO"),
            Some(Some("bar"))
        );
        assert_eq!(
            file.services["list"].environment.get("FOO"),
            Some(Some("bar"))
        );

        // The one-variable partner: a declared-but-valueless entry is
        // Some(None), and an undeclared one is None. Collapsing these is how a
        // gate silently "checks" a variable that carries nothing.
        assert_eq!(
            file.services["mapping"].environment.get("PASSTHROUGH"),
            Some(None)
        );
        assert_eq!(
            file.services["list"].environment.get("PASSTHROUGH"),
            Some(None)
        );
        assert_eq!(file.services["mapping"].environment.get("ABSENT"), None);

        assert_eq!(
            file.environment_occurrences("FOO"),
            vec![("list", Some("bar")), ("mapping", Some("bar"))]
        );
    }

    #[test]
    fn interpolation_forms_are_distinguished() {
        assert_eq!(
            ComposeValue::parse("${FOO:?run zeroship dev init}"),
            ComposeValue::Required {
                name: "FOO",
                reason: "run zeroship dev init"
            }
        );
        assert_eq!(
            ComposeValue::parse("${FOO?bare}"),
            ComposeValue::Required {
                name: "FOO",
                reason: "bare"
            }
        );
        assert_eq!(
            ComposeValue::parse("${FOO:-weak}"),
            ComposeValue::Defaulted {
                name: "FOO",
                default: "weak"
            }
        );
        assert_eq!(
            ComposeValue::parse("${FOO:-}"),
            ComposeValue::Defaulted {
                name: "FOO",
                default: ""
            }
        );
        assert_eq!(
            ComposeValue::parse("${FOO}"),
            ComposeValue::Substituted { name: "FOO" }
        );
        assert_eq!(ComposeValue::parse("plain"), ComposeValue::Literal("plain"));
        // A real value from deploy/compose/docker-compose.yml.
        assert_eq!(
            ComposeValue::parse("${ZEROSHIP_SECRETS_DIR:-./secrets}/broker-secret"),
            ComposeValue::Composite("${ZEROSHIP_SECRETS_DIR:-./secrets}/broker-secret")
        );
        // `$$` escapes the interpolation: compose passes `${FOO}` through.
        assert_eq!(
            ComposeValue::parse("$${FOO}"),
            ComposeValue::Literal("$${FOO}")
        );
        // A secret whose literal text merely CONTAINS a brace stays a literal.
        assert_eq!(
            ComposeValue::parse("abc}def"),
            ComposeValue::Literal("abc}def")
        );

        // Does NOT cover compose's `${FOO:+alt}` form, which this tree does
        // not use; it currently classifies as Substituted with a name of
        // "FOO:+alt", which is wrong but unreachable. Add an arm before using
        // that spelling.
    }

    #[test]
    fn an_absent_environment_key_is_empty_not_an_error() {
        let file = ComposeFile::from_yaml("services:\n  bare:\n    image: alpine\n").expect("parses");
        assert!(matches!(
            file.services["bare"].environment,
            Environment::Absent
        ));
        assert!(file.services["bare"].environment.entries().is_empty());
        assert_eq!(file.services["bare"].image.as_deref(), Some("alpine"));
    }
}
