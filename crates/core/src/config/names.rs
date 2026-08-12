//! Inert canonical naming, source policy, and read-site machinery.
//!
//! Nothing in this module is wired into a live process in Step 1. It provides
//! one vocabulary for later conversions and lets the non-shipped contract tool
//! prove that generated declarations and generated readers agree.

use std::fmt;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::str::FromStr;

use serde::de::DeserializeOwned;
use thiserror::Error;

/// A validated ASCII lowercase dotted snake_case configuration identity.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalName<'a>(&'a str);

impl fmt::Debug for CanonicalName<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("CanonicalName").field(&self.0).finish()
    }
}

impl<'a> CanonicalName<'a> {
    /// Validate a borrowed canonical identity.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalNameError`] when `raw` is not the exact grammar.
    pub fn new(raw: &'a str) -> Result<Self, CanonicalNameError> {
        if is_canonical(raw) {
            Ok(Self(raw))
        } else {
            Err(CanonicalNameError(raw.to_owned()))
        }
    }

    /// Construct a static identity in generated code.
    ///
    /// The proc macro validates the literal before emitting this call. Keeping
    /// this const makes generated registry elements link-time data.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_static(raw: &'static str) -> CanonicalName<'static> {
        CanonicalName(raw)
    }

    /// Return the canonical spelling.
    #[must_use]
    pub const fn as_str(self) -> &'a str {
        self.0
    }

    /// Remove exactly one matching leading binary-scope segment.
    #[must_use]
    pub fn local(self, scope: &str) -> &'a str {
        self.0
            .strip_prefix(scope)
            .and_then(|rest| rest.strip_prefix('.'))
            .unwrap_or(self.0)
    }

    /// Project the complete identity into its reserved environment name.
    #[must_use]
    pub fn env_name(self) -> String {
        format!(
            "ZEROSHIP_{}",
            self.0.replace('.', "_").to_ascii_uppercase()
        )
    }

    /// Project the identity into a clap long name, without leading dashes.
    #[must_use]
    pub fn flag_name(self, scope: &str, sensitivity: Sensitivity) -> String {
        let mut name = self.local(scope).replace(['.', '_'], "-");
        if sensitivity == Sensitivity::Secret {
            name.push_str("-file");
        }
        name
    }

    /// Project into the TOML path. Operational and secret paths are identical.
    #[must_use]
    pub const fn toml_path(self) -> &'a str {
        self.0
    }
}

fn is_canonical(raw: &str) -> bool {
    !raw.is_empty() && raw.is_ascii() && raw.split('.').all(is_segment)
}

fn is_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_lowercase) {
        return false;
    }
    let mut underscore = false;
    for byte in bytes {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            underscore = false;
        } else if *byte == b'_' && !underscore {
            underscore = true;
        } else {
            return false;
        }
    }
    !underscore
}

/// An invalid canonical identity.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("invalid canonical config name {0:?}; expected ASCII lowercase dotted snake_case")]
pub struct CanonicalNameError(String);

/// Exact binary identity plus its CLI-local scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Consumer {
    target: &'static str,
    scope: &'static str,
}

impl Consumer {
    /// Construct generated consumer metadata.
    #[must_use]
    pub const fn new(target: &'static str, scope: &'static str) -> Self {
        Self { target, scope }
    }

    /// Exact Cargo binary target.
    #[must_use]
    pub const fn target(self) -> &'static str {
        self.target
    }

    /// One canonical segment removed from local flags when it matches.
    #[must_use]
    pub const fn scope(self) -> &'static str {
        self.scope
    }
}

/// Whether disclosure of a resolved value is sensitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Sensitivity {
    /// Safe to display subject to ordinary validation.
    Operational,
    /// Must remain redacted and must not travel through argv.
    Secret,
}

/// One generated source slot read by a resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceKind {
    /// Operational value flag.
    Cli,
    /// Secret path flag.
    CliFile,
    /// Environment input.
    Env,
    /// Optional TOML overlay input.
    Toml,
}

const OPERATIONAL_SOURCES: &[SourceKind] =
    &[SourceKind::Cli, SourceKind::Env, SourceKind::Toml];
const SECRET_SOURCES: &[SourceKind] =
    &[SourceKind::CliFile, SourceKind::Env, SourceKind::Toml];

/// One generated setting contract. Projections are computed, never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigSpec {
    canonical: CanonicalName<'static>,
    consumers: &'static [Consumer],
    sensitivity: Sensitivity,
    field: &'static str,
    arg_id: &'static str,
    rust_type: &'static str,
    default: Option<&'static str>,
}

impl ConfigSpec {
    /// Declare an operational setting.
    #[must_use]
    pub const fn operational(
        canonical: CanonicalName<'static>,
        consumers: &'static [Consumer],
        field: &'static str,
        arg_id: &'static str,
        rust_type: &'static str,
        default: Option<&'static str>,
    ) -> Self {
        Self {
            canonical,
            consumers,
            sensitivity: Sensitivity::Operational,
            field,
            arg_id,
            rust_type,
            default,
        }
    }

    /// Declare a secret setting.
    #[must_use]
    pub const fn secret(
        canonical: CanonicalName<'static>,
        consumers: &'static [Consumer],
        field: &'static str,
        arg_id: &'static str,
        rust_type: &'static str,
        _default: Option<&'static str>,
    ) -> Self {
        Self {
            canonical,
            consumers,
            sensitivity: Sensitivity::Secret,
            field,
            arg_id,
            rust_type,
            default: None,
        }
    }

    /// Canonical identity.
    #[must_use]
    pub const fn canonical(self) -> CanonicalName<'static> {
        self.canonical
    }

    /// Every exact process consumer.
    #[must_use]
    pub const fn consumers(self) -> &'static [Consumer] {
        self.consumers
    }

    /// Supply sensitivity.
    #[must_use]
    pub const fn sensitivity(self) -> Sensitivity {
        self.sensitivity
    }

    /// Derived sources for this type-driven class.
    #[must_use]
    pub const fn sources(self) -> &'static [SourceKind] {
        match self.sensitivity {
            Sensitivity::Operational => OPERATIONAL_SOURCES,
            Sensitivity::Secret => SECRET_SOURCES,
        }
    }

    /// Canonical environment projection.
    #[must_use]
    pub fn env_name(self) -> Option<String> {
        Some(self.canonical.env_name())
    }

    /// Consumer-local flag projection.
    #[must_use]
    pub fn flag_name(self, consumer: Consumer) -> Option<String> {
        Some(self.canonical.flag_name(consumer.scope(), self.sensitivity))
    }

    /// Canonical TOML path, regardless of sensitivity.
    #[must_use]
    pub const fn toml_path(self) -> &'static str {
        self.canonical.toml_path()
    }

    /// Rust field name.
    #[must_use]
    pub const fn field(self) -> &'static str {
        self.field
    }

    /// Compiled clap argument identifier.
    #[must_use]
    pub const fn arg_id(self) -> &'static str {
        self.arg_id
    }

    /// Resolved inner Rust type spelling.
    #[must_use]
    pub const fn rust_type(self) -> &'static str {
        self.rust_type
    }

    /// Stringified compiled default, if one exists.
    #[must_use]
    pub const fn default(self) -> Option<&'static str> {
        self.default
    }
}

/// One independently linked resolver read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadSite {
    canonical: CanonicalName<'static>,
    consumer: Consumer,
    source: SourceKind,
    file: &'static str,
    line: u32,
    column: u32,
}

impl ReadSite {
    /// Construct generated linked metadata.
    #[must_use]
    pub const fn new(
        canonical: CanonicalName<'static>,
        consumer: Consumer,
        source: SourceKind,
        file: &'static str,
        line: u32,
        column: u32,
    ) -> Self {
        Self {
            canonical,
            consumer,
            source,
            file,
            line,
            column,
        }
    }

    /// Canonical identity read.
    #[must_use]
    pub const fn canonical(self) -> CanonicalName<'static> {
        self.canonical
    }

    /// Exact binary consumer.
    #[must_use]
    pub const fn consumer(self) -> Consumer {
        self.consumer
    }

    /// Source tier read.
    #[must_use]
    pub const fn source(self) -> SourceKind {
        self.source
    }

    /// Source location for diagnostics.
    #[must_use]
    pub const fn location(self) -> (&'static str, u32, u32) {
        (self.file, self.line, self.column)
    }
}

#[linkme::distributed_slice]
/// All linked typed and generated config reads in the final binary.
pub static CONFIG_READ_SITES: [ReadSite];

/// Marker implemented by each generated exact binary consumer.
pub trait ConfigConsumer: Copy + 'static {
    /// Exact Cargo binary target name.
    const BINARY: &'static str;
    /// Canonical segment used only for CLI localization.
    const SCOPE: &'static str;
}

/// Typed environment identity bound to one consumer marker.
#[derive(Debug, Clone, Copy)]
pub struct EnvKey<T, C> {
    canonical: CanonicalName<'static>,
    marker: PhantomData<fn() -> (T, C)>,
}

impl<T, C: ConfigConsumer> EnvKey<T, C> {
    /// Construct a macro-validated key.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_static(canonical: CanonicalName<'static>) -> Self {
        Self {
            canonical,
            marker: PhantomData,
        }
    }

    /// Canonical identity.
    #[must_use]
    pub const fn canonical(self) -> CanonicalName<'static> {
        self.canonical
    }

    /// Consumer metadata carried by the key's marker type.
    #[doc(hidden)]
    #[must_use]
    pub const fn consumer(self) -> Consumer {
        Consumer::new(C::BINARY, C::SCOPE)
    }
}

/// Consumer token used to make wrong-binary reads a type mismatch.
#[derive(Debug, Clone, Copy)]
pub struct ConsumerToken<C>(PhantomData<fn() -> C>);

impl<C: ConfigConsumer> ConsumerToken<C> {
    /// Create the zero-sized token.
    #[must_use]
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<C: ConfigConsumer> Default for ConsumerToken<C> {
    fn default() -> Self {
        Self::new()
    }
}

/// A parsed typed environment read failure that never contains the raw value.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EnvReadError {
    /// Process environment was not valid Unicode.
    #[error("environment variable {name} is not valid Unicode")]
    NotUnicode {
        /// Derived environment name.
        name: String,
    },
    /// The target type rejected the value; parser text is deliberately hidden.
    #[error("environment variable {name} has an invalid value")]
    InvalidValue {
        /// Derived environment name.
        name: String,
    },
}

/// Read one typed key for its statically matching consumer.
///
/// # Errors
///
/// Returns a name-only diagnostic for non-Unicode or unparsable values.
pub fn read_typed_env<T, C>(
    key: EnvKey<T, C>,
    _consumer: ConsumerToken<C>,
) -> Result<Option<T>, EnvReadError>
where
    T: FromStr,
    C: ConfigConsumer,
{
    let name = key.canonical.env_name();
    let Some(value) = super::env::raw_var(&name).map_err(|()| EnvReadError::NotUnicode {
        name: name.clone(),
    })?
    else {
        return Ok(None);
    };
    value
        .parse()
        .map(Some)
        .map_err(|_| EnvReadError::InvalidValue { name })
}

/// Read a typed key and independently register its exact read site.
#[macro_export]
macro_rules! read_config_env {
    ($key:expr, $consumer:expr $(,)?) => {{
        #[::zeroship_core::__private::linkme::distributed_slice(
            ::zeroship_core::config::CONFIG_READ_SITES
        )]
        #[linkme(crate = ::zeroship_core::__private::linkme)]
        static __ZEROSHIP_CONFIG_TYPED_ENV_READ: ::zeroship_core::config::ReadSite =
            ::zeroship_core::config::ReadSite::new(
                ($key).canonical(),
                ($key).consumer(),
                ::zeroship_core::config::SourceKind::Env,
                ::core::file!(),
                ::core::line!(),
                ::core::column!(),
            );
        ::zeroship_core::config::read_typed_env(
            $key,
            ::zeroship_core::config::ConsumerToken::new_for(&$consumer),
        )
    }};
}

impl<C: ConfigConsumer> ConsumerToken<C> {
    /// Infer a token from a value of the exact consumer marker type.
    #[doc(hidden)]
    #[must_use]
    pub const fn new_for(_consumer: &C) -> Self {
        Self::new()
    }
}

/// An ordinary resolved value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operational<T>(T);

impl<T> Operational<T> {
    /// Wrap a resolved operational value.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Borrow the resolved value.
    #[must_use]
    pub const fn get(&self) -> &T {
        &self.0
    }

    /// Consume the wrapper.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.0
    }
}

/// A resolved secret whose standard formatting surface cannot expose its value.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    /// Wrap resolved secret material.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Explicitly borrow secret material at its true consumption boundary.
    #[must_use]
    pub const fn expose_secret(&self) -> &T {
        &self.0
    }

    /// Explicitly consume the wrapper.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

macro_rules! policy_wrapper {
    ($name:ident) => {
        #[doc = concat!("Type-driven source policy wrapper `", stringify!($name), "`.")]
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name<T>(pub T);
    };
}

policy_wrapper!(BootstrapControl);
policy_wrapper!(CommandControl);
policy_wrapper!(CommandEnv);
policy_wrapper!(CliEnv);
policy_wrapper!(ExternalEnv);
policy_wrapper!(ExternalEnvFamily);
policy_wrapper!(DevEnv);
policy_wrapper!(TestEnv);

/// A failure walking or decoding the generic canonical overlay.
#[derive(Debug, Error)]
pub enum OverlayLookupError {
    /// A non-table value blocked traversal before the final segment.
    #[error("TOML path {path} crosses a non-table value at {prefix}")]
    NonTable {
        /// Full canonical path.
        path: String,
        /// Prefix at which traversal stopped.
        prefix: String,
    },
    /// The value existed but did not match the requested operational type.
    #[error("TOML path {path} has an invalid value")]
    InvalidValue {
        /// Full canonical path. Parser text and values are suppressed.
        path: String,
    },
}

/// Look up a TOML value by the canonical path itself.
///
/// # Errors
///
/// Returns [`OverlayLookupError::NonTable`] if an intermediate segment is a leaf.
pub fn lookup_overlay<'a>(
    root: &'a toml::Value,
    name: CanonicalName<'_>,
) -> Result<Option<&'a toml::Value>, OverlayLookupError> {
    let mut current = root;
    let mut walked = Vec::new();
    let segments = name.as_str().split('.').collect::<Vec<_>>();
    for (index, segment) in segments.iter().enumerate() {
        walked.push(*segment);
        let Some(table) = current.as_table() else {
            return Err(OverlayLookupError::NonTable {
                path: name.as_str().to_owned(),
                prefix: walked[..walked.len().saturating_sub(1)].join("."),
            });
        };
        let Some(next) = table.get(*segment) else {
            return Ok(None);
        };
        current = next;
        if index + 1 == segments.len() {
            return Ok(Some(current));
        }
    }
    Ok(None)
}

/// Resolve one operational carrier with CLI/env already merged by clap.
///
/// # Errors
///
/// Returns a name-only error for invalid overlay data or a missing value.
pub fn resolve_operational<T, F>(
    name: CanonicalName<'static>,
    carrier: Option<T>,
    overlay: Option<&toml::Value>,
    default: F,
) -> Result<Operational<T>, ConfigResolveError>
where
    T: DeserializeOwned,
    F: FnOnce() -> Option<T>,
{
    if let Some(value) = carrier {
        return Ok(Operational::new(value));
    }
    if let Some(root) = overlay {
        if let Some(value) = lookup_overlay(root, name)? {
            return value
                .clone()
                .try_into()
                .map(Operational::new)
                .map_err(|_| ConfigResolveError::InvalidValue {
                    canonical: name.as_str(),
                });
        }
    }
    default()
        .map(Operational::new)
        .ok_or(ConfigResolveError::Missing {
            canonical: name.as_str(),
        })
}

/// Resolve secret path, canonical environment, then canonical TOML input.
///
/// A TOML literal is accepted. Parse/resolver errors deliberately expose only
/// the canonical identity, never secret material or a parser message.
///
/// # Errors
///
/// Returns name-only I/O or missing-input diagnostics.
pub fn resolve_secret_sources(
    name: CanonicalName<'static>,
    cli_file: Option<PathBuf>,
    env: Option<String>,
    overlay: Option<&toml::Value>,
) -> Result<Secret<String>, ConfigResolveError> {
    if let Some(path) = cli_file {
        return std::fs::read_to_string(&path)
            .map(|mut value| {
                if value.ends_with('\n') {
                    value.pop();
                    if value.ends_with('\r') {
                        value.pop();
                    }
                }
                Secret::new(value)
            })
            .map_err(|_| ConfigResolveError::SecretFile {
                canonical: name.as_str(),
                path,
            });
    }
    if let Some(value) = env {
        validate_secret_input(name, &value)?;
        return Ok(Secret::new(value));
    }
    if let Some(root) = overlay {
        if let Some(value) = lookup_overlay(root, name)? {
            if let Some(literal_or_reference) = value.as_str() {
                validate_secret_input(name, literal_or_reference)?;
                return Ok(Secret::new(literal_or_reference.to_owned()));
            }
            return Err(ConfigResolveError::InvalidValue {
                canonical: name.as_str(),
            });
        }
    }
    Err(ConfigResolveError::Missing {
        canonical: name.as_str(),
    })
}

fn validate_secret_input(
    name: CanonicalName<'static>,
    value: &str,
) -> Result<(), ConfigResolveError> {
    super::secrets::validate_secret_ref(value).map_err(|_| ConfigResolveError::InvalidValue {
        canonical: name.as_str(),
    })
}

/// A generated-source resolution failure with value-free diagnostics.
#[derive(Debug, Error)]
pub enum ConfigResolveError {
    /// No enabled source produced a value.
    #[error("configuration {canonical} has no value")]
    Missing {
        /// Canonical identity.
        canonical: &'static str,
    },
    /// A supplied value failed typed validation.
    #[error("configuration {canonical} has an invalid value")]
    InvalidValue {
        /// Canonical identity.
        canonical: &'static str,
    },
    /// A secret file could not be read.
    #[error("configuration {canonical} could not read secret file {path}", path = .path.display())]
    SecretFile {
        /// Canonical identity.
        canonical: &'static str,
        /// Operator-selected file path, not secret contents.
        path: PathBuf,
    },
    /// Canonical overlay traversal failed.
    #[error(transparent)]
    Overlay(#[from] OverlayLookupError),
    /// Typed environment access failed.
    #[error(transparent)]
    Env(#[from] EnvReadError),
}

/// Implemented by one proc-macro-generated resolved configuration declaration.
pub trait GeneratedConfig: Sized {
    /// Generated clap carrier.
    type Sources;
    /// Exact binary consumer marker.
    type Consumer: ConfigConsumer;

    /// Generated declaration registry.
    const SPECS: &'static [ConfigSpec];

    /// Resolve carriers against an optional canonical TOML overlay.
    ///
    /// # Errors
    ///
    /// Returns value-free resolution diagnostics.
    fn resolve_config(
        sources: Self::Sources,
        overlay: Option<&toml::Value>,
    ) -> Result<Self, ConfigResolveError>;
}

#[cfg(test)]
mod tests {
    use super::{
        CanonicalName, Consumer, ConfigSpec, Secret, Sensitivity, lookup_overlay,
        resolve_secret_sources,
    };

    #[test]
    fn canonical_grammar_and_transforms_match_the_contract() {
        let control = CanonicalName::new("control.database_url").expect("canonical");
        assert_eq!(control.local("control"), "database_url");
        assert_eq!(control.local("worker"), "control.database_url");
        assert_eq!(control.env_name(), "ZEROSHIP_CONTROL_DATABASE_URL");
        assert_eq!(
            control.flag_name("control", Sensitivity::Operational),
            "database-url"
        );
        assert_eq!(
            control.flag_name("control", Sensitivity::Secret),
            "database-url-file"
        );
        assert_eq!(control.toml_path(), "control.database_url");
        assert_eq!(
            CanonicalName::new("control_key")
                .expect("global")
                .flag_name("control", Sensitivity::Secret),
            "control-key-file"
        );

        for bad in ["", "Control.port", "control..port", "control.max__apps"] {
            assert!(CanonicalName::new(bad).is_err(), "accepted {bad:?}");
        }

        // This does not prove registry uniqueness; contract-crate fixtures
        // exercise cross-declaration projection collisions.
    }

    #[test]
    fn amended_toml_projection_accepts_secret_literal_at_canonical_path() {
        let overlay: toml::Value = toml::from_str(
            r#"
[control]
database_url = "postgres://operator-mounted-secret"
"#,
        )
        .expect("TOML");
        let name = CanonicalName::new("control.database_url").expect("canonical");

        assert_eq!(
            lookup_overlay(&overlay, name)
                .expect("lookup")
                .and_then(toml::Value::as_str),
            Some("postgres://operator-mounted-secret")
        );
        let secret = resolve_secret_sources(
            CanonicalName::from_static("control.database_url"),
            None,
            None,
            Some(&overlay),
        )
        .expect("literal secret is allowed");
        assert_eq!(
            secret.expose_secret(),
            "postgres://operator-mounted-secret"
        );

        // This does not inspect tracked files or runtime permissions. Those are
        // amendment Section 4.7 gates and deliberately are not Step 1 behavior.
    }

    #[test]
    fn secret_debug_never_formats_the_inner_value() {
        struct Hostile;
        impl std::fmt::Debug for Hostile {
            fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                panic!("inner secret formatter must not run")
            }
        }
        assert_eq!(format!("{:?}", Secret::new(Hostile)), "Secret(<redacted>)");

        // This does not prevent an explicit expose_secret call. The wrapper
        // removes accidental formatting, not deliberate value consumption.
    }

    #[test]
    fn malformed_secret_urn_is_rejected_without_echoing_the_value() {
        let sentinel = "urn:zeroship:file:";
        let overlay: toml::Value = toml::from_str(&format!(
            "[control]\ndatabase_url = {sentinel:?}\n"
        ))
        .expect("fixture TOML");
        let error = resolve_secret_sources(
            CanonicalName::from_static("control.database_url"),
            None,
            None,
            Some(&overlay),
        )
        .expect_err("empty URN body must fail");
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("control.database_url"));
        assert!(!diagnostic.contains(sentinel));

        // This does not resolve a referenced backend. It proves only format
        // rejection and value-free diagnostics in the inert resolver.
    }

    #[test]
    fn config_spec_derives_every_spelling() {
        const CONSUMERS: &[Consumer] = &[Consumer::new("zeroship-control", "control")];
        let spec = ConfigSpec::secret(
            CanonicalName::from_static("control.database_url"),
            CONSUMERS,
            "database_url",
            "database_url",
            "String",
            None,
        );
        assert_eq!(spec.env_name().as_deref(), Some("ZEROSHIP_CONTROL_DATABASE_URL"));
        assert_eq!(spec.flag_name(CONSUMERS[0]).as_deref(), Some("database-url-file"));
        assert_eq!(spec.toml_path(), "control.database_url");
    }
}
