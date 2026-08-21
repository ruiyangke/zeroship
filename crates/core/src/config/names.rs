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
const BOOTSTRAP_SOURCES: &[SourceKind] = &[SourceKind::Cli, SourceKind::Env];
const FLAG_ONLY_SOURCES: &[SourceKind] = &[SourceKind::Cli];

/// Which supply set a declaration's wrapper type selects.
///
/// This is the type-driven classification from the design. [`Sensitivity`] is
/// derived from it and answers a different question - whether the resolved value
/// may be displayed - so the two are deliberately not the same enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SupplyClass {
    /// `Operational<T>`: flag, env, TOML, compiled default.
    Operational,
    /// `Secret<T>`: `-file` flag, env, TOML, no compiled default.
    Secret,
    /// `BootstrapControl<T>`: flag, and env unless the declaration disables it.
    ///
    /// Either the value is needed BEFORE the overlay can be loaded (the overlay
    /// selector itself), or it is a safety control that must not be persistable
    /// in the overlay it would otherwise be read from. A safety control may also
    /// refuse the environment, because "someone left a variable set" is exactly
    /// how a protection gets turned off by accident.
    Bootstrap,
    /// `CommandControl<T>`: flag only. An action, not a server setting.
    Command,
}

/// One generated setting contract. Projections are computed, never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigSpec {
    canonical: CanonicalName<'static>,
    consumers: &'static [Consumer],
    class: SupplyClass,
    /// Only a bootstrap control may set this false; see [`SupplyClass`].
    env_enabled: bool,
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
            class: SupplyClass::Operational,
            env_enabled: true,
            field,
            arg_id,
            rust_type,
            default,
        }
    }

    /// Declare a secret setting.
    ///
    /// There is deliberately no default parameter: a compiled default for a
    /// secret would put credential material in the binary. The attribute
    /// rejects `default` on a `Secret<T>` field, and this signature makes the
    /// same rule unrepresentable for a hand-written spec.
    #[must_use]
    pub const fn secret(
        canonical: CanonicalName<'static>,
        consumers: &'static [Consumer],
        field: &'static str,
        arg_id: &'static str,
        rust_type: &'static str,
    ) -> Self {
        Self {
            canonical,
            consumers,
            class: SupplyClass::Secret,
            env_enabled: true,
            field,
            arg_id,
            rust_type,
            default: None,
        }
    }

    /// Declare a bootstrap control: flag, optionally environment, never TOML.
    ///
    /// `env_enabled` is the transform table's "env when enabled". A safety
    /// control sets it false so no ambient variable can relax the protection.
    #[must_use]
    pub const fn bootstrap(
        canonical: CanonicalName<'static>,
        consumers: &'static [Consumer],
        env_enabled: bool,
        field: &'static str,
        arg_id: &'static str,
        rust_type: &'static str,
        default: Option<&'static str>,
    ) -> Self {
        Self {
            canonical,
            consumers,
            class: SupplyClass::Bootstrap,
            env_enabled,
            field,
            arg_id,
            rust_type,
            default,
        }
    }

    /// Declare a command control: flag only, no environment and no TOML.
    #[must_use]
    pub const fn command(
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
            class: SupplyClass::Command,
            env_enabled: false,
            field,
            arg_id,
            rust_type,
            default,
        }
    }

    /// Wrapper-selected supply class.
    #[must_use]
    pub const fn class(self) -> SupplyClass {
        self.class
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

    /// Supply sensitivity. Only `Secret<T>` is secret-classed.
    #[must_use]
    pub const fn sensitivity(self) -> Sensitivity {
        match self.class {
            SupplyClass::Secret => Sensitivity::Secret,
            SupplyClass::Operational | SupplyClass::Bootstrap | SupplyClass::Command => {
                Sensitivity::Operational
            }
        }
    }

    /// Derived sources for this type-driven class.
    #[must_use]
    pub const fn sources(self) -> &'static [SourceKind] {
        match self.class {
            SupplyClass::Operational => OPERATIONAL_SOURCES,
            SupplyClass::Secret => SECRET_SOURCES,
            SupplyClass::Bootstrap if self.env_enabled => BOOTSTRAP_SOURCES,
            SupplyClass::Bootstrap | SupplyClass::Command => FLAG_ONLY_SOURCES,
        }
    }

    /// Canonical environment projection, absent when the class has no env tier.
    #[must_use]
    pub fn env_name(self) -> Option<String> {
        self.env_enabled.then(|| self.canonical.env_name())
    }

    /// Consumer-local flag projection.
    #[must_use]
    pub fn flag_name(self, consumer: Consumer) -> Option<String> {
        Some(
            self.canonical
                .flag_name(consumer.scope(), self.sensitivity()),
        )
    }

    /// Canonical TOML path, or `None` when the class has no overlay slot.
    ///
    /// Operational and secret settings share one projection: the canonical name
    /// itself. Bootstrap and command controls have none by construction, so a
    /// later ops-TOML gate cannot mistake a control for a valid overlay leaf.
    #[must_use]
    pub const fn toml_path(self) -> Option<&'static str> {
        match self.class {
            SupplyClass::Operational | SupplyClass::Secret => Some(self.canonical.toml_path()),
            SupplyClass::Bootstrap | SupplyClass::Command => None,
        }
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
///
/// `$key` must be a CONSTANT expression: the identity is copied into a linked
/// `static`, so a runtime binding is rejected by the compiler. Generated
/// resolvers pass `EnvKey::from_static(..)` directly; hand-written accessors
/// must name a `const` key. `$consumer` is a value of the exact consumer marker
/// type, which makes a wrong-binary read a type mismatch.
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

/// A resolved secret: WHICH tier supplied it, and the material only when this
/// run actually resolved it.
///
/// The `Option` on the material is not an oversight, it is the `--check-config`
/// contract made structural. A dry run establishes the SOURCE of every secret
/// without opening a file or dereferencing a reference, so on that path there is
/// no material to hold and the type says so. A report can therefore only ever
/// ask [`Secret::is_configured`], and a consumer that wants material has to
/// handle its absence rather than receive a plausible-looking empty string.
///
/// Nothing here can format the material: [`fmt::Debug`] prints the source tier
/// and nothing else - not the value, not a prefix of it, not its length.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T> {
    source: Option<SourceKind>,
    material: Option<T>,
}

impl<T> Secret<T> {
    /// A secret no enabled source supplied.
    #[must_use]
    pub const fn absent() -> Self {
        Self {
            source: None,
            material: None,
        }
    }

    /// A secret supplied by `source`, carrying `material` when it was resolved.
    ///
    /// `material` is `None` on the `--check-config` path for any source that
    /// would need I/O to read: the secret is configured, and that is all the run
    /// is entitled to know.
    #[must_use]
    pub const fn supplied(source: SourceKind, material: Option<T>) -> Self {
        Self {
            source: Some(source),
            material,
        }
    }

    /// True when some enabled source supplied this secret.
    ///
    /// This is the ONLY fact a generated report may publish about a secret.
    #[must_use]
    pub const fn is_configured(&self) -> bool {
        self.source.is_some()
    }

    /// The tier that supplied the secret, if any. A tier name, never material.
    #[must_use]
    pub const fn source(&self) -> Option<SourceKind> {
        self.source
    }

    /// Explicitly borrow secret material at its true consumption boundary.
    ///
    /// `None` means either "no source supplied it" or "this run resolved the
    /// source but deliberately did not read the material". Both are cases a
    /// consumer must handle; neither is an empty secret.
    #[must_use]
    pub const fn expose_secret(&self) -> Option<&T> {
        self.material.as_ref()
    }

    /// Explicitly consume the wrapper.
    #[must_use]
    pub fn into_inner(self) -> Option<T> {
        self.material
    }
}

impl Secret<String> {
    /// Borrow the material as a string slice, or `""` when there is none.
    ///
    /// The bridge to the existing strength validators, which take `&str` and
    /// already treat an empty value as "required and missing". Using it on the
    /// `--check-config` path would make an unread file look like an empty
    /// secret, so callers there must branch on [`Secret::expose_secret`] first.
    #[must_use]
    pub fn expose_str(&self) -> &str {
        self.material.as_deref().unwrap_or_default()
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.source {
            Some(source) => write!(formatter, "Secret(configured from {source:?})"),
            None => formatter.write_str("Secret(<unset>)"),
        }
    }
}

/// A value needed before the overlay exists, or one the overlay must not carry.
///
/// Its supply set is flag and environment. There is no TOML slot: the overlay
/// selector cannot be read from the overlay it selects, and a safety control
/// read from a persisted file is a control an operator can forget they left on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapControl<T>(T);

impl<T> BootstrapControl<T> {
    /// Wrap a resolved bootstrap control.
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

/// An action selector such as `--check-config` rather than a server setting.
///
/// Its supply set is the flag alone. An environment variable that silently
/// turned a running server into a config dump would be a foot-gun, and a TOML
/// key for it would be a setting that outlives the command it describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandControl<T>(T);

impl<T> CommandControl<T> {
    /// Wrap a resolved command control.
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

/// Vocabulary the later migration steps need, with NO machinery behind it yet.
///
/// The attribute accepts `Operational<T>`, `Secret<T>`, `BootstrapControl<T>`
/// and `CommandControl<T>` only, and rejects every wrapper below by name. They
/// exist so the classification in the design has one spelling, not because
/// declaring one does anything today. Do not read a wrapper's presence as a
/// source policy that is being enforced.
macro_rules! policy_wrapper {
    ($name:ident) => {
        #[doc = concat!(
            "Placeholder source-policy wrapper `", stringify!($name),
            "`. The `zeroship_config` attribute does NOT accept it yet."
        )]
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name<T>(pub T);
    };
}

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

/// Resolve one control carrier. There is deliberately no overlay parameter.
///
/// Bootstrap and command controls have no TOML slot, so this signature - not a
/// convention - is what stops a generated resolver reading one from the
/// overlay. clap has already merged the flag and (for a bootstrap control) the
/// environment into `carrier`.
///
/// # Errors
///
/// Returns [`ConfigResolveError::Missing`] when neither the carrier nor a
/// compiled default supplied a value.
pub fn resolve_control<T, F>(
    name: CanonicalName<'static>,
    carrier: Option<T>,
    default: F,
) -> Result<T, ConfigResolveError>
where
    F: FnOnce() -> Option<T>,
{
    carrier
        .or_else(default)
        .ok_or(ConfigResolveError::Missing {
            canonical: name.as_str(),
        })
}

/// How far a run is entitled to take a secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretResolution {
    /// Real startup: dereference the supplying source down to the material.
    Boot,
    /// `--check-config`: establish the SOURCE and validate its source policy and
    /// format, and stop there. No file is opened and no reference is followed.
    CheckConfig,
}

/// Resolve secret path flag, canonical environment, then canonical TOML input.
///
/// Precedence is `CLI -file > env > TOML`, matching every other class. Each of
/// the latter two may carry a literal or a `urn:zeroship:file:<path>` reference;
/// a reference in any other scheme is rejected by source policy in BOTH modes,
/// which is what makes an env-to-env alias or a Vault URN a `--check-config`
/// failure rather than a boot-time surprise.
///
/// Under [`SecretResolution::CheckConfig`] nothing is read: a `-file` flag and a
/// file reference resolve to `is_configured() == true` with no material, while a
/// literal already in memory keeps its material so the caller's strength
/// validators still have something real to check.
///
/// An absent secret is [`Secret::absent`], NOT an error. There is no compiled
/// default for a secret, so "required" is a property of the consumer, and the
/// consumer's own validator produces a far better message than a generic
/// missing-value diagnostic could.
///
/// Errors deliberately expose only the canonical identity, never secret material
/// and never a parser message that might quote it.
///
/// # Errors
///
/// Returns name-only source-policy, overlay-type, and I/O diagnostics.
pub fn resolve_secret_sources(
    name: CanonicalName<'static>,
    cli_file: Option<PathBuf>,
    env: Option<String>,
    overlay: Option<&toml::Value>,
    mode: SecretResolution,
) -> Result<Secret<String>, ConfigResolveError> {
    if let Some(path) = cli_file {
        if mode == SecretResolution::CheckConfig {
            return Ok(Secret::supplied(SourceKind::CliFile, None));
        }
        let material = zeroship_secret_policy::read_secret_file(&path.to_string_lossy())
            .map_err(|error| secret_file_error(name, path, error))?;
        return Ok(Secret::supplied(SourceKind::CliFile, Some(material)));
    }
    if let Some(value) = env {
        return resolve_secret_input(name, SourceKind::Env, &value, mode);
    }
    if let Some(root) = overlay {
        if let Some(value) = lookup_overlay(root, name)? {
            let Some(literal_or_reference) = value.as_str() else {
                return Err(ConfigResolveError::InvalidValue {
                    canonical: name.as_str(),
                });
            };
            return resolve_secret_input(name, SourceKind::Toml, literal_or_reference, mode);
        }
    }
    Ok(Secret::absent())
}

/// Apply source policy to one in-band secret input, then resolve it if asked.
fn resolve_secret_input(
    name: CanonicalName<'static>,
    source: SourceKind,
    value: &str,
    mode: SecretResolution,
) -> Result<Secret<String>, ConfigResolveError> {
    let parsed = zeroship_secret_policy::parse_secret_ref(value).map_err(|_| {
        ConfigResolveError::InvalidSecretSource {
            canonical: name.as_str(),
        }
    })?;
    let material = match (parsed, mode) {
        (zeroship_secret_policy::SecretRef::Literal(literal), _) => Some(literal.to_owned()),
        (zeroship_secret_policy::SecretRef::File(_), SecretResolution::CheckConfig) => None,
        (zeroship_secret_policy::SecretRef::File(path), SecretResolution::Boot) => Some(
            zeroship_secret_policy::read_secret_file(path)
                .map_err(|error| secret_file_error(name, PathBuf::from(path), error))?,
        ),
    };
    Ok(Secret::supplied(source, material))
}

/// Map a secret-file failure onto the value-free resolver diagnostic.
///
/// A permission refusal keeps its own text: the whole point of the owner-only
/// policy is that the operator is told the mode and the fix, and collapsing it
/// into "could not read secret file X" sends them looking for the wrong thing.
/// Every other failure stays name-and-path only, because an I/O message can
/// carry parser detail and this crate never risks that next to a secret.
fn secret_file_error(
    name: CanonicalName<'static>,
    path: PathBuf,
    error: zeroship_secret_policy::SecretError,
) -> ConfigResolveError {
    match error {
        error @ (zeroship_secret_policy::SecretError::InsecurePermissions { .. }
        | zeroship_secret_policy::SecretError::UndeterminableMode { .. }) => {
            ConfigResolveError::SecretFilePermissions {
                canonical: name.as_str(),
                reason: error.to_string(),
            }
        }
        _ => ConfigResolveError::SecretFile {
            canonical: name.as_str(),
            path,
        },
    }
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
    /// A secret input named a source outside the supply set.
    ///
    /// Distinct from [`ConfigResolveError::InvalidValue`] because it is the one
    /// diagnostic an operator migrating off an env-to-env alias, a Vault URN or
    /// an AWS ARN will see, and it must say what IS allowed. The offending text
    /// is still withheld: it sits where a secret sits.
    #[error(
        "configuration {canonical} names an unsupported secret source; supply the value \
         itself or a urn:zeroship:file:<path> reference"
    )]
    InvalidSecretSource {
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
    /// A secret file failed the owner-only permission policy.
    ///
    /// Distinct from [`ConfigResolveError::SecretFile`] because "could not read"
    /// sends an operator hunting a missing file or a bad mount, and the fix here
    /// is a `chmod`. The reason comes from
    /// [`zeroship_secret_policy::SecretError`] and quotes only the path and the mode -
    /// both operator-selected, neither secret material.
    #[error("configuration {canonical} rejected its secret file: {reason}")]
    SecretFilePermissions {
        /// Canonical identity.
        canonical: &'static str,
        /// The permission refusal, rendered from `SecretError`.
        reason: String,
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
        CanonicalName, Consumer, ConfigSpec, Secret, SecretResolution, Sensitivity, SourceKind,
        SupplyClass, lookup_overlay, resolve_control, resolve_secret_sources,
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
            SecretResolution::Boot,
        )
        .expect("literal secret is allowed");
        assert_eq!(
            secret.expose_secret().map(String::as_str),
            Some("postgres://operator-mounted-secret")
        );
        assert_eq!(secret.source(), Some(SourceKind::Toml));

        // This does not inspect tracked files or runtime permissions. Those are
        // amendment Section 4.7 gates and deliberately are not this step's.
    }

    #[test]
    fn secret_debug_never_formats_the_inner_value() {
        struct Hostile;
        impl std::fmt::Debug for Hostile {
            fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                panic!("inner secret formatter must not run")
            }
        }
        assert_eq!(
            format!("{:?}", Secret::supplied(SourceKind::Env, Some(Hostile))),
            "Secret(configured from Env)"
        );
        assert_eq!(
            format!("{:?}", Secret::<Hostile>::absent()),
            "Secret(<unset>)"
        );

        // This does not prevent an explicit expose_secret call. The wrapper
        // removes accidental formatting, not deliberate value consumption.
    }

    // The presence-only claim, at the type. A report may ask is_configured();
    // there is no accessor that yields a prefix or a length, and Debug names
    // only the tier. A sentinel long enough to be recognisable is used so a
    // substring leak would show.
    #[test]
    fn a_secret_publishes_presence_and_source_but_never_material() {
        const SENTINEL: &str = "zeroship-presence-sentinel-9d2f7a4c1e";
        let secret = Secret::supplied(SourceKind::Env, Some(SENTINEL.to_owned()));
        assert!(secret.is_configured());
        assert_eq!(secret.source(), Some(SourceKind::Env));

        let rendered = format!("{secret:?}");
        assert!(!rendered.contains(SENTINEL));
        for length in 4..=SENTINEL.len() {
            assert!(
                !rendered.contains(&SENTINEL[..length]),
                "Debug leaked a {length}-char prefix of the secret: {rendered}"
            );
        }
        assert!(
            !rendered.contains(&SENTINEL.len().to_string()),
            "Debug leaked the secret's length: {rendered}"
        );

        // Does NOT cover a caller that calls expose_secret and prints the result
        // itself. Nothing in a type can stop deliberate disclosure; the binaries'
        // report construction is checked separately by the e2e sentinel case.
    }

    #[test]
    fn malformed_secret_urn_is_rejected_without_echoing_the_value() {
        // The body carries a recognisable sentinel: a diagnostic that echoed the
        // rejected input would carry it too, and a secret sits exactly here.
        let sentinel = "urn:zeroship:nope:h4nd3d-back-to-the-operator";
        let overlay: toml::Value = toml::from_str(&format!(
            "[control]\ndatabase_url = {sentinel:?}\n"
        ))
        .expect("fixture TOML");
        let error = resolve_secret_sources(
            CanonicalName::from_static("control.database_url"),
            None,
            None,
            Some(&overlay),
            SecretResolution::CheckConfig,
        )
        .expect_err("empty URN body must fail");
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("control.database_url"));
        assert!(!diagnostic.contains(sentinel));
    }

    // THE `--check-config` PAIR. Both halves name a secret FILE that does not
    // exist and differ in exactly one variable: the scheme. The file scheme is
    // inside the supply set, so the dry run succeeds WITHOUT opening anything;
    // the deleted env scheme is outside it, so the dry run fails. A dry run that
    // passed only when the file happened to exist would fail the first half, and
    // one that validated no source policy at all would fail the second.
    #[test]
    fn check_config_validates_source_policy_without_opening_the_file() {
        let name = CanonicalName::from_static("control.master_key");
        let missing = "/no/such/zeroship/check-config/secret";
        assert!(
            !std::path::Path::new(missing).exists(),
            "the fixture path must really be absent"
        );

        let accepted = resolve_secret_sources(
            name,
            None,
            Some(format!("urn:zeroship:file:{missing}")),
            None,
            SecretResolution::CheckConfig,
        )
        .expect("a file reference is format-valid whether or not the path exists");
        assert!(accepted.is_configured());
        assert_eq!(
            accepted.expose_secret(),
            None,
            "--check-config must not have read the file"
        );

        let rejected = resolve_secret_sources(
            name,
            None,
            Some("urn:zeroship:env:SOME_OTHER_VAR".to_owned()),
            None,
            SecretResolution::CheckConfig,
        )
        .expect_err("an env-to-env alias is outside the supply set");
        assert!(rejected.to_string().contains("control.master_key"));
        assert!(rejected.to_string().contains("urn:zeroship:file:"));

        // The same absent path on the BOOT path must fail, so "no error" above
        // is a property of the mode and not of the resolver ignoring files.
        assert!(resolve_secret_sources(
            name,
            None,
            Some(format!("urn:zeroship:file:{missing}")),
            None,
            SecretResolution::Boot,
        )
        .is_err());

        // Does NOT cover the CLI `-file` flag arm, which is asserted below, nor
        // whether a real binary routes --check-config to CheckConfig mode; that
        // is the macro's job and tests/config_check_e2e.sh proves it end to end.
    }

    #[test]
    fn a_check_config_run_never_reads_a_secret_path_flag() {
        let name = CanonicalName::from_static("control.master_key");
        let missing = std::path::PathBuf::from("/no/such/zeroship/path-flag/secret");

        let checked = resolve_secret_sources(
            name,
            Some(missing.clone()),
            None,
            None,
            SecretResolution::CheckConfig,
        )
        .expect("a path flag is not opened during a dry run");
        assert!(checked.is_configured());
        assert_eq!(checked.source(), Some(SourceKind::CliFile));
        assert_eq!(checked.expose_secret(), None);

        // One-variable control: only the mode differs, and boot does open it.
        assert!(
            resolve_secret_sources(name, Some(missing), None, None, SecretResolution::Boot)
                .is_err()
        );
    }

    #[test]
    fn an_unsupplied_secret_is_absent_rather_than_an_error() {
        let secret = resolve_secret_sources(
            CanonicalName::from_static("control.master_key"),
            None,
            None,
            None,
            SecretResolution::Boot,
        )
        .expect("no source is not a resolution failure");
        assert!(!secret.is_configured());
        assert_eq!(secret.expose_secret(), None);
        assert_eq!(secret.expose_str(), "");

        // Does NOT cover whether any given consumer REQUIRES the secret. That
        // stays with the consumer's own validator and its own message.
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
        );
        assert_eq!(spec.env_name().as_deref(), Some("ZEROSHIP_CONTROL_DATABASE_URL"));
        assert_eq!(spec.flag_name(CONSUMERS[0]).as_deref(), Some("database-url-file"));
        assert_eq!(spec.toml_path(), Some("control.database_url"));
    }

    #[test]
    fn control_classes_drop_the_layers_their_type_forbids() {
        const CONSUMERS: &[Consumer] = &[Consumer::new("zeroship-control", "control")];

        let bootstrap = ConfigSpec::bootstrap(
            CanonicalName::from_static("config"),
            CONSUMERS,
            true,
            "config",
            "config",
            "Option<PathBuf>",
            None,
        );
        assert_eq!(bootstrap.class(), SupplyClass::Bootstrap);
        assert_eq!(bootstrap.sensitivity(), Sensitivity::Operational);
        assert_eq!(bootstrap.sources(), [SourceKind::Cli, SourceKind::Env]);
        assert_eq!(bootstrap.env_name().as_deref(), Some("ZEROSHIP_CONFIG"));
        assert_eq!(bootstrap.flag_name(CONSUMERS[0]).as_deref(), Some("config"));
        assert_eq!(
            bootstrap.toml_path(),
            None,
            "the overlay selector must not be readable from the overlay it selects"
        );

        let command = ConfigSpec::command(
            CanonicalName::from_static("check_config"),
            CONSUMERS,
            "check_config",
            "check_config",
            "bool",
            None,
        );
        assert_eq!(command.sources(), [SourceKind::Cli]);
        assert_eq!(command.env_name(), None);
        assert_eq!(
            ConfigSpec::bootstrap(
                CanonicalName::from_static("worker.workflow_advance_unsigned"),
                CONSUMERS,
                false,
                "workflow_advance_unsigned",
                "workflow_advance_unsigned",
                "bool",
                None,
            )
            .sources(),
            [SourceKind::Cli],
            "a safety control that refuses the environment declares no Env source"
        );
        assert_eq!(
            command.flag_name(CONSUMERS[0]).as_deref(),
            Some("check-config")
        );
        assert_eq!(command.toml_path(), None);

        // This asserts the SPEC's derived layers. That the generated clap
        // carrier and resolver agree with it is a separate claim, proved by the
        // contract crate against the compiled Command and the linked read sites.
    }

    #[test]
    fn a_control_resolves_from_its_carrier_then_its_compiled_default() {
        let name = CanonicalName::from_static("check_config_format");
        assert_eq!(
            resolve_control(name, Some("json"), || Some("text")).expect("carrier wins"),
            "json"
        );
        assert_eq!(
            resolve_control(name, None, || Some("text")).expect("default applies"),
            "text"
        );
        let missing = resolve_control::<&str, _>(name, None, || None)
            .expect_err("no source and no default is an error");
        assert!(missing.to_string().contains("check_config_format"));

        // resolve_control takes no overlay argument at all, so "a control never
        // reads TOML" is a property of the signature rather than of this test.
    }
}
