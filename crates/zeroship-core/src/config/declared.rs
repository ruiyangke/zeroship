//! Typed identities for environment reads the config contract does not generate.
//!
//! [`super::names::EnvKey`] covers ONE population: zeroship platform startup
//! settings, whose environment spelling is derived (`ZEROSHIP_` plus the
//! canonical identity). Step 4 of
//! `docs/proposals/2026-08-11-config-name-alignment.md` has to account for
//! every OTHER first-party read as well - `PATH`, `AWS_ACCESS_KEY_ID`,
//! `RUST_LOG`, a test's `PG_TEST_URL`, a build script's `OUT_DIR` - and those
//! have literal names owned by somebody else. Projecting them from a canonical
//! identity would be a lie, and leaving them as bare `std::env::var` calls is
//! exactly the invisibility the step exists to remove.
//!
//! So they get their own key type. A [`DeclaredEnvKey`] carries the LITERAL
//! name plus an [`EnvClass`] saying whose contract it is, and is bound to a
//! consumer marker in the same way a config key is. Reading one goes through
//! [`crate::read_declared_env`], which links a [`DeclaredEnvRead`] into
//! [`DECLARED_ENV_READS`]. The result is that "every first-party environment
//! read is enumerable" is true of the whole population, not just the generated
//! part, and that each read carries a stated classification instead of silence.
//!
//! What this module deliberately does NOT do: it does not give these names a
//! TOML slot, a CLI flag, a default, or any place in the operator-facing
//! contract. They are reads, classified and located. A name that deserves to be
//! operator-visible platform configuration belongs in a `#[zeroship_config]`
//! declaration instead.
//!
//! One class, [`EnvClass::Platform`], names settings that have not made that
//! trip yet. It records an incomplete conversion rather than hiding it; read
//! its documentation before using it.

use std::ffi::OsString;
use std::marker::PhantomData;
use std::str::FromStr;

use super::names::{ConfigConsumer, Consumer, EnvReadError};

/// Whose contract an environment name belongs to.
///
/// Every variant except [`EnvClass::Config`] is constructible from this module;
/// `Config` exists only so a report can print the generated population and the
/// declared one in one table. There is deliberately no `DeclaredEnvKey`
/// constructor for it, because a platform setting must come from a
/// `#[zeroship_config]` declaration that also generates its flag, its TOML path
/// and its `--check-config` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EnvClass {
    /// A generated platform startup setting. Not constructible here.
    Config,
    /// A contract owned by somebody else: `PATH`, `AWS_*`, `RUST_LOG`,
    /// `HOSTNAME`, a broker address supplied by the surrounding deployment.
    External,
    /// Read only by first-party test code, and meaningless in a shipped image.
    Test,
    /// Read only on the local development vector.
    Dev,
    /// The creator CLI's own environment surface. It is first-party and it is
    /// declared, but it gets no server TOML overlay: `zeroship` is a tool run
    /// on a creator's machine, not a service an operator configures.
    Cli,
    /// A compiler/build input read by a build script (`OUT_DIR`, `CARGO_*`).
    Build,
    /// A value that belongs to the deployed creator app rather than to the
    /// platform, and is passed THROUGH by a platform process.
    Creator,
    /// A zeroship-owned tunable that has NOT yet been given a generated
    /// `#[zeroship_config]` declaration.
    ///
    /// This is the transitional class. It is the only one that may carry the
    /// `ZEROSHIP_` prefix without being dev, test, CLI or creator scoped, and it
    /// is also where a zeroship-owned knob that happens NOT to carry the prefix
    /// belongs - `CONTROL_DEPLOY_RETENTION_BATCH_SIZE` is ours whatever it is
    /// spelled, and calling it `external` would be a claim that somebody else
    /// owns it. Every member is a conversion candidate: the setting deserves a
    /// canonical identity, a flag, a TOML path and a `--check-config` row, and
    /// until it has them an operator cannot discover it and `--check-config`
    /// cannot report it.
    ///
    /// It exists because the alternative was worse. Step 4 converts READS; a
    /// generated declaration also needs a consuming binary's config struct, a
    /// resolver and a deployment rewrite, which is Step 3 and Step 6 work. Left
    /// as raw reads these names would have been invisible; classified `external`
    /// they would have been a lie about who owns them. Recorded here they are
    /// counted, located, and impossible to confuse with a converted setting.
    ///
    /// Do not add to it without saying why a generated declaration is not
    /// possible in the same change.
    Platform,
}

impl EnvClass {
    /// Stable lowercase spelling used by reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::External => "external",
            Self::Test => "test",
            Self::Dev => "dev",
            Self::Cli => "cli",
            Self::Build => "build",
            Self::Creator => "creator",
            Self::Platform => "platform",
        }
    }
}

/// Whether `name` is a plausible environment-variable spelling.
///
/// Deliberately strict: uppercase ASCII, digits and underscore, non-empty, not
/// starting with a digit. A key whose literal fails this is a typo or a
/// canonical name pasted into the wrong constructor.
#[must_use]
pub const fn is_valid_env_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    if bytes[0].is_ascii_digit() {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if !(byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_') {
            return false;
        }
        index += 1;
    }
    true
}

/// A literal-named environment identity outside the generated config contract.
///
/// Bound to a consumer marker for the same reason [`super::names::EnvKey`] is:
/// the reading component is part of the record, and a key declared for one
/// component cannot be read with another's token.
#[derive(Debug, Clone, Copy)]
pub struct DeclaredEnvKey<T, C> {
    name: &'static str,
    class: EnvClass,
    marker: PhantomData<fn() -> (T, C)>,
}

macro_rules! declared_constructor {
    ($fn_name:ident, $class:ident, $doc:expr) => {
        #[doc = $doc]
        #[must_use]
        pub const fn $fn_name(name: &'static str) -> Self {
            Self {
                name,
                class: EnvClass::$class,
                marker: PhantomData,
            }
        }
    };
}

impl<T, C: ConfigConsumer> DeclaredEnvKey<T, C> {
    declared_constructor!(
        external,
        External,
        "Declare a name owned by a third party or by the surrounding deployment."
    );
    declared_constructor!(
        test,
        Test,
        "Declare a name read only by first-party test code."
    );
    declared_constructor!(dev, Dev, "Declare a name read only on the dev vector.");
    declared_constructor!(cli, Cli, "Declare a creator-CLI environment name.");
    declared_constructor!(build, Build, "Declare a build-script compiler input.");
    declared_constructor!(
        creator,
        Creator,
        "Declare a value owned by the deployed creator app, not by the platform."
    );
    declared_constructor!(
        platform,
        Platform,
        "Declare a zeroship-owned tunable that has no generated declaration YET."
    );

    /// The literal environment spelling.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// Whose contract this name belongs to.
    #[must_use]
    pub const fn class(self) -> EnvClass {
        self.class
    }

    /// Consumer metadata carried by the key's marker type.
    #[doc(hidden)]
    #[must_use]
    pub const fn consumer(self) -> Consumer {
        Consumer::new(C::BINARY, C::SCOPE)
    }
}

/// A FAMILY of environment names sharing one literal prefix.
///
/// For names whose suffix is supplied at runtime, the declaration records the
/// literal prefix. Reports can then identify the family without persisting a
/// potentially sensitive suffix or requiring it to be known at compile time.
///
/// This is the concept the proposal spells `ExternalEnvFamily` in its
/// source-policy vocabulary.
#[derive(Debug, Clone, Copy)]
pub struct DeclaredEnvFamily<T, C> {
    prefix: &'static str,
    class: EnvClass,
    marker: PhantomData<fn() -> (T, C)>,
}

impl<T, C: ConfigConsumer> DeclaredEnvFamily<T, C> {
    /// Declare a zeroship-owned family whose members have no declaration yet.
    #[must_use]
    pub const fn platform(prefix: &'static str) -> Self {
        Self {
            prefix,
            class: EnvClass::Platform,
            marker: PhantomData,
        }
    }

    /// Declare a family owned by a third party or the surrounding deployment.
    #[must_use]
    pub const fn external(prefix: &'static str) -> Self {
        Self {
            prefix,
            class: EnvClass::External,
            marker: PhantomData,
        }
    }

    /// The literal prefix every member shares.
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        self.prefix
    }

    /// Whose contract this family belongs to.
    #[must_use]
    pub const fn class(self) -> EnvClass {
        self.class
    }

    /// Consumer metadata carried by the family's marker type.
    #[doc(hidden)]
    #[must_use]
    pub const fn consumer(self) -> Consumer {
        Consumer::new(C::BINARY, C::SCOPE)
    }
}

/// Read one member of a declared family.
///
/// # Errors
///
/// Returns a name-only diagnostic for non-Unicode or unparsable values. The
/// error names the FULL member, because that is what an operator has to fix.
#[doc(hidden)]
pub fn read_declared_env_family_value<T, C>(
    family: DeclaredEnvFamily<T, C>,
    suffix: &str,
    _consumer: super::names::ConsumerToken<C>,
) -> Result<Option<T>, EnvReadError>
where
    T: FromStr,
    C: ConfigConsumer,
{
    let name = format!("{}{suffix}", family.prefix);
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

/// Read one member of a declared family and register the family's read site.
#[macro_export]
macro_rules! read_declared_env_family {
    ($family:expr, $suffix:expr, $consumer:expr $(,)?) => {{
        #[::zeroship_core::__private::linkme::distributed_slice(
            ::zeroship_core::config::DECLARED_ENV_READS
        )]
        #[linkme(crate = ::zeroship_core::__private::linkme)]
        // The linked element is a `static` with `#[link_section]`, which the
        // workspace `unsafe_code = "deny"` rejects. Outside `zeroship-core` the
        // lint does not reach external-macro output and this is inert; INSIDE
        // core the macro is local, so without it every core-internal declared
        // read would need its own function-scoped allow. Scoped to the generated
        // static, which contains no `unsafe` block to hide.
        #[allow(unsafe_code)]
        static __ZEROSHIP_DECLARED_ENV_FAMILY_READ: ::zeroship_core::config::DeclaredEnvRead =
            ::zeroship_core::config::DeclaredEnvRead::family(
                ($family).prefix(),
                ($family).class(),
                ($family).consumer(),
                ::core::file!(),
                ::core::line!(),
                ::core::column!(),
            );
        ::zeroship_core::config::read_declared_env_family_value(
            $family,
            $suffix,
            ::zeroship_core::config::ConsumerToken::new_for(&$consumer),
        )
    }};
}

/// One independently linked read of a declared non-config environment name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclaredEnvRead {
    name: &'static str,
    class: EnvClass,
    family: bool,
    consumer: Consumer,
    file: &'static str,
    line: u32,
    column: u32,
}

impl DeclaredEnvRead {
    /// Construct linked metadata. Called only by the reading macros.
    #[doc(hidden)]
    #[must_use]
    pub const fn new(
        name: &'static str,
        class: EnvClass,
        consumer: Consumer,
        file: &'static str,
        line: u32,
        column: u32,
    ) -> Self {
        Self {
            name,
            class,
            family: false,
            consumer,
            file,
            line,
            column,
        }
    }

    /// Construct linked metadata for a family read. Called only by its macro.
    #[doc(hidden)]
    #[must_use]
    pub const fn family(
        prefix: &'static str,
        class: EnvClass,
        consumer: Consumer,
        file: &'static str,
        line: u32,
        column: u32,
    ) -> Self {
        Self {
            name: prefix,
            class,
            family: true,
            consumer,
            file,
            line,
            column,
        }
    }

    /// The literal environment spelling read, or a family's shared prefix.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// Whether [`DeclaredEnvRead::name`] is a whole name or a family prefix.
    ///
    /// Reports distinguish a complete variable name from a prefix that still
    /// needs a runtime suffix.
    #[must_use]
    pub const fn is_family(self) -> bool {
        self.family
    }

    /// Stated classification.
    #[must_use]
    pub const fn class(self) -> EnvClass {
        self.class
    }

    /// Component that performed the read.
    ///
    /// For a library or test consumer the target is the cargo PACKAGE, not a
    /// binary target: library code has no binary, and pretending otherwise
    /// would put a fictional target name in the record.
    #[must_use]
    pub const fn consumer(self) -> Consumer {
        self.consumer
    }

    /// Source location for diagnostics.
    #[must_use]
    pub const fn location(self) -> (&'static str, u32, u32) {
        (self.file, self.line, self.column)
    }
}

#[linkme::distributed_slice]
/// Every linked non-config environment read in the final binary.
pub static DECLARED_ENV_READS: [DeclaredEnvRead];

/// Read one declared key, parsed into `T`.
///
/// # Errors
///
/// Returns a name-only diagnostic for non-Unicode or unparsable values. The
/// value itself never reaches the error, because a declared name is as likely
/// to hold a credential as a generated one is.
#[doc(hidden)]
pub fn read_declared_env_value<T, C>(
    key: DeclaredEnvKey<T, C>,
    _consumer: super::names::ConsumerToken<C>,
) -> Result<Option<T>, EnvReadError>
where
    T: FromStr,
    C: ConfigConsumer,
{
    let name = key.name;
    let Some(value) = super::env::raw_var(name).map_err(|()| EnvReadError::NotUnicode {
        name: name.to_owned(),
    })?
    else {
        return Ok(None);
    };
    value
        .parse()
        .map(Some)
        .map_err(|_| EnvReadError::InvalidValue {
            name: name.to_owned(),
        })
}

/// Read one declared key without requiring valid Unicode.
///
/// This is the `var_os` shape. It exists because several first-party reads are
/// filesystem paths (`HOME`, `XDG_CONFIG_HOME`) where a non-Unicode value is
/// legitimate on Unix, and because save/restore helpers must round-trip absence
/// faithfully - `Option<OsString>` distinguishes "unset" from "set to empty",
/// which a `String` accessor that maps errors to `None` cannot.
#[doc(hidden)]
#[must_use]
pub fn read_declared_env_os_value<C>(
    key: DeclaredEnvKey<OsString, C>,
    _consumer: super::names::ConsumerToken<C>,
) -> Option<OsString>
where
    C: ConfigConsumer,
{
    super::env::raw_var_os(key.name)
}

/// Consumer metadata inferred from a value of an exact consumer marker type.
///
/// [`DeclaredEnvKey::consumer`] serves the keyed macros; a whole-environment
/// snapshot has no key, so its macro needs this instead. It is `const` because
/// the result is placed in a linked `static`.
#[doc(hidden)]
#[must_use]
pub const fn consumer_of<C: ConfigConsumer>(_consumer: &C) -> Consumer {
    Consumer::new(C::BINARY, C::SCOPE)
}

/// The reserved name a whole-environment snapshot is recorded under.
///
/// A snapshot has no single identity, so it gets a reserved one rather than
/// being left out of the record. It cannot collide with a real variable:
/// [`is_valid_env_name`] rejects the angle brackets, so no declared key can
/// ever carry this spelling.
pub const PROCESS_ENV_SNAPSHOT: &str = "<PROCESS ENVIRONMENT SNAPSHOT>";

/// Take a whole-environment snapshot on behalf of creator app code.
///
/// This is the `std::env::vars()` shape and exists for exactly one purpose:
/// forwarding the process environment into a creator app's `process.env`. It is
/// classified [`EnvClass::Creator`] because the names in it belong to the app,
/// not to the platform - the platform cannot enumerate them and must not try.
#[doc(hidden)]
#[must_use]
pub fn read_process_env_snapshot_value<C>(
    _consumer: super::names::ConsumerToken<C>,
) -> Vec<(String, String)>
where
    C: ConfigConsumer,
{
    super::env::raw_vars()
}

/// Snapshot the whole process environment and register the read site.
#[macro_export]
macro_rules! read_process_env_snapshot {
    ($consumer:expr $(,)?) => {{
        #[::zeroship_core::__private::linkme::distributed_slice(
            ::zeroship_core::config::DECLARED_ENV_READS
        )]
        #[linkme(crate = ::zeroship_core::__private::linkme)]
        // The linked element is a `static` with `#[link_section]`, which the
        // workspace `unsafe_code = "deny"` rejects. Outside `zeroship-core` the
        // lint does not reach external-macro output and this is inert; INSIDE
        // core the macro is local, so without it every core-internal declared
        // read would need its own function-scoped allow. Scoped to the generated
        // static, which contains no `unsafe` block to hide.
        #[allow(unsafe_code)]
        static __ZEROSHIP_PROCESS_ENV_SNAPSHOT_READ: ::zeroship_core::config::DeclaredEnvRead =
            ::zeroship_core::config::DeclaredEnvRead::new(
                ::zeroship_core::config::PROCESS_ENV_SNAPSHOT,
                ::zeroship_core::config::EnvClass::Creator,
                ::zeroship_core::config::consumer_of(&$consumer),
                ::core::file!(),
                ::core::line!(),
                ::core::column!(),
            );
        ::zeroship_core::config::read_process_env_snapshot_value(
            ::zeroship_core::config::ConsumerToken::new_for(&$consumer),
        )
    }};
}

/// Read a declared key and independently register its exact read site.
///
/// `$key` must be a CONSTANT expression, for the same reason
/// [`crate::read_config_env`] requires one: the identity is copied into a
/// linked `static`, so a runtime binding is a compile error rather than an
/// unrecorded read.
#[macro_export]
macro_rules! read_declared_env {
    ($key:expr, $consumer:expr $(,)?) => {{
        #[::zeroship_core::__private::linkme::distributed_slice(
            ::zeroship_core::config::DECLARED_ENV_READS
        )]
        #[linkme(crate = ::zeroship_core::__private::linkme)]
        // The linked element is a `static` with `#[link_section]`, which the
        // workspace `unsafe_code = "deny"` rejects. Outside `zeroship-core` the
        // lint does not reach external-macro output and this is inert; INSIDE
        // core the macro is local, so without it every core-internal declared
        // read would need its own function-scoped allow. Scoped to the generated
        // static, which contains no `unsafe` block to hide.
        #[allow(unsafe_code)]
        static __ZEROSHIP_DECLARED_ENV_READ: ::zeroship_core::config::DeclaredEnvRead =
            ::zeroship_core::config::DeclaredEnvRead::new(
                ($key).name(),
                ($key).class(),
                ($key).consumer(),
                ::core::file!(),
                ::core::line!(),
                ::core::column!(),
            );
        ::zeroship_core::config::read_declared_env_value(
            $key,
            ::zeroship_core::config::ConsumerToken::new_for(&$consumer),
        )
    }};
}

/// Read a declared key as an `OsString` and register its exact read site.
#[macro_export]
macro_rules! read_declared_env_os {
    ($key:expr, $consumer:expr $(,)?) => {{
        #[::zeroship_core::__private::linkme::distributed_slice(
            ::zeroship_core::config::DECLARED_ENV_READS
        )]
        #[linkme(crate = ::zeroship_core::__private::linkme)]
        // The linked element is a `static` with `#[link_section]`, which the
        // workspace `unsafe_code = "deny"` rejects. Outside `zeroship-core` the
        // lint does not reach external-macro output and this is inert; INSIDE
        // core the macro is local, so without it every core-internal declared
        // read would need its own function-scoped allow. Scoped to the generated
        // static, which contains no `unsafe` block to hide.
        #[allow(unsafe_code)]
        static __ZEROSHIP_DECLARED_ENV_OS_READ: ::zeroship_core::config::DeclaredEnvRead =
            ::zeroship_core::config::DeclaredEnvRead::new(
                ($key).name(),
                ($key).class(),
                ($key).consumer(),
                ::core::file!(),
                ::core::line!(),
                ::core::column!(),
            );
        ::zeroship_core::config::read_declared_env_os_value(
            $key,
            ::zeroship_core::config::ConsumerToken::new_for(&$consumer),
        )
    }};
}

/// Read a declared name in one line, naming its class and its consumer.
///
/// This is the ordinary spelling. It expands to a `const` key plus
/// [`crate::read_declared_env`], so the read is registered exactly as the
/// explicit two-step form is; what it saves is a named constant per site.
///
/// The name must be a LITERAL so the declaration remains visible at the call
/// site. A site that needs one name in several places declares the key itself
/// and uses [`crate::read_declared_env`].
///
/// Returns `Option<String>`, dropping a non-Unicode value exactly as the
/// `std::env::var(..).ok()` it replaces did. That is deliberate: a
/// behaviour-preserving conversion is what lets the gate land without auditing
/// every call site for a changed failure mode. Use the explicit key form when
/// the difference between "unset" and "not Unicode" matters.
#[macro_export]
macro_rules! declared_env {
    ($class:ident, $name:literal, $consumer:path $(,)?) => {{
        const __ZEROSHIP_DECLARED_KEY: $crate::config::DeclaredEnvKey<
            ::std::string::String,
            $consumer,
        > = $crate::config::DeclaredEnvKey::$class($name);
        $crate::read_declared_env!(__ZEROSHIP_DECLARED_KEY, $consumer).ok().flatten()
    }};
}

/// Read a declared name as an `OsString`, naming its class and its consumer.
#[macro_export]
macro_rules! declared_env_os {
    ($class:ident, $name:literal, $consumer:path $(,)?) => {{
        const __ZEROSHIP_DECLARED_OS_KEY: $crate::config::DeclaredEnvKey<
            ::std::ffi::OsString,
            $consumer,
        > = $crate::config::DeclaredEnvKey::$class($name);
        $crate::read_declared_env_os!(__ZEROSHIP_DECLARED_OS_KEY, $consumer)
    }};
}

/// The consumer marker for first-party test code.
///
/// Test binaries have no `#[zeroship_config]` declaration and no shipped
/// target, so there is nothing for a per-crate marker to name that the read
/// site's file path does not already say. One shared marker keeps a test read
/// attributable (class `test`, plus file and line) without inventing a
/// fictional binary per crate. A test read is not bound to its crate at the
/// type level, so review must keep crate-owned test settings local.
#[derive(Clone, Copy, Debug)]
pub struct TestHarness;

impl ConfigConsumer for TestHarness {
    const BINARY: &'static str = "<first-party tests>";
    const SCOPE: &'static str = "test";
}

/// Read a test-only name. Shorthand for `declared_env!(test, NAME, TestHarness)`.
#[macro_export]
macro_rules! test_env {
    ($name:literal $(,)?) => {
        $crate::declared_env!(test, $name, $crate::config::TestHarness)
    };
}

/// Read a test-only name as an `OsString`.
#[macro_export]
macro_rules! test_env_os {
    ($name:literal $(,)?) => {
        $crate::declared_env_os!(test, $name, $crate::config::TestHarness)
    };
}

/// Resolve the S3 blob-store inputs `zeroship-bundle` cannot read.
///
/// `zeroship-core` depends on `zeroship-bundle`, so bundle cannot use the typed
/// keys in this module; it takes a resolved
/// [`zeroship_bundle::blob_config::S3Runtime`] instead. That moves those reads
/// up into whichever server binary builds the store, and this macro is what
/// stops the same reads being written out once per binary. Every one of them
/// registers against the CALLING binary's consumer, which is the point: the
/// record names which process reads `AWS_SECRET_ACCESS_KEY`, rather than a
/// shared library.
///
/// Expands to `Result<S3Runtime, BlobStoreConfigError>`. Call it only when the
/// location is actually `s3://`; on a local store the credentials are absent by
/// design and the error is correct but useless.
#[macro_export]
macro_rules! resolve_s3_runtime {
    ($consumer:path $(,)?) => {
        ::zeroship_bundle::blob_config::S3Runtime::from_resolved(
            $crate::declared_env!(external, "AWS_ACCESS_KEY_ID", $consumer),
            $crate::declared_env!(external, "AWS_SECRET_ACCESS_KEY", $consumer),
            $crate::declared_env!(external, "AWS_SESSION_TOKEN", $consumer),
            ::zeroship_bundle::limits::resolve_upload_concurrency(
                $crate::declared_env!(
                    platform,
                    "ZEROSHIP_BLOB_UPLOAD_CONCURRENCY",
                    $consumer
                )
                .as_deref(),
            ),
        )
    };
}

/// Declare a consumer marker for a component that is not a generated binary.
///
/// `#[zeroship_config]` generates a `ConfigConsumer` for each server binary. A
/// library crate, a test binary or a build script has no such declaration and
/// still has to name itself when it reads the environment, so this macro emits
/// the same marker shape. `target` is the cargo PACKAGE for library and test
/// consumers; see [`DeclaredEnvRead::consumer`] for why that is not a binary.
#[macro_export]
macro_rules! declare_env_consumer {
    (
        $(#[$attribute:meta])*
        $visibility:vis $name:ident, target = $target:literal, scope = $scope:literal $(,)?
    ) => {
        $(#[$attribute])*
        #[derive(Clone, Copy, Debug)]
        $visibility struct $name;

        impl $crate::config::ConfigConsumer for $name {
            const BINARY: &'static str = $target;
            const SCOPE: &'static str = $scope;
        }
    };
}

#[cfg(test)]
mod tests {
    use super::{is_valid_env_name, DeclaredEnvKey, EnvClass};

    declare_env_consumer!(TestConsumer, target = "zeroship-core", scope = "core");

    #[test]
    fn valid_env_names_are_uppercase_ascii() {
        assert!(is_valid_env_name("PATH"));
        assert!(is_valid_env_name("AWS_ACCESS_KEY_ID"));
        assert!(is_valid_env_name("PG_TEST_URL2"));
        assert!(!is_valid_env_name(""));
        assert!(!is_valid_env_name("2FAST"));
        assert!(!is_valid_env_name("lowercase"));
        assert!(!is_valid_env_name("HAS-DASH"));
        assert!(!is_valid_env_name("control.port"));
    }

    #[test]
    fn each_constructor_records_its_own_class() {
        const EXTERNAL: DeclaredEnvKey<String, TestConsumer> = DeclaredEnvKey::external("PATH");
        const TEST: DeclaredEnvKey<String, TestConsumer> = DeclaredEnvKey::test("PG_TEST_URL");
        const DEV: DeclaredEnvKey<String, TestConsumer> = DeclaredEnvKey::dev("ZEROSHIP_DEV");
        const CLI: DeclaredEnvKey<String, TestConsumer> = DeclaredEnvKey::cli("ZEROSHIP_TOKEN");
        const BUILD: DeclaredEnvKey<String, TestConsumer> = DeclaredEnvKey::build("OUT_DIR");
        const CREATOR: DeclaredEnvKey<String, TestConsumer> =
            DeclaredEnvKey::creator("ZEROSHIP_DEPLOY_ID");

        assert_eq!(EXTERNAL.class(), EnvClass::External);
        assert_eq!(TEST.class(), EnvClass::Test);
        assert_eq!(DEV.class(), EnvClass::Dev);
        assert_eq!(CLI.class(), EnvClass::Cli);
        assert_eq!(BUILD.class(), EnvClass::Build);
        assert_eq!(CREATOR.class(), EnvClass::Creator);
        assert_eq!(EXTERNAL.name(), "PATH");
        assert_eq!(EXTERNAL.consumer().target(), "zeroship-core");
    }

    #[test]
    fn class_spellings_are_distinct() {
        let spellings = [
            EnvClass::Config,
            EnvClass::External,
            EnvClass::Test,
            EnvClass::Dev,
            EnvClass::Cli,
            EnvClass::Build,
            EnvClass::Creator,
            EnvClass::Platform,
        ]
        .map(EnvClass::as_str);
        let mut sorted = spellings.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), spellings.len(), "class spellings collide");
    }
}
