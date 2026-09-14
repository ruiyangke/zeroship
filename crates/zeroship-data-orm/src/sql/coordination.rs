//! Session coordination statements: advisory locks and transaction-local settings.
//!
//! These statements carry keys and names only. Hashing, case folding and the
//! lifetime of a lock or setting are the database's: a caller that spells the
//! same key form in SQL contends on the identical lock.

use super::compiler::CompileError;
use std::fmt;

/// Output column of an [`AdvisoryLockAction::Try`] statement.
pub const ADVISORY_ACQUIRED: &str = "acquired";
/// Output column of an [`AdvisoryLockAction::Release`] statement.
pub const ADVISORY_RELEASED: &str = "released";

/// Longest identifier segment accepted in a setting name.
const MAX_SETTING_SEGMENT_BYTES: usize = 63;

/// An advisory lock key in one of PostgreSQL's key forms.
///
/// The hashed forms send their text to the database, which computes the key.
/// One-argument keys (`Single`, `Hashed`, `HashedLowercase`) and two-argument
/// keys (`Pair`, `HashedPair`) occupy separate key spaces, so they never
/// contend even when their bits are equal. Debug output omits key text.
#[derive(Clone, PartialEq, Eq)]
pub enum AdvisoryKey {
    /// One signed 64-bit key.
    Single(i64),
    /// Two signed 32-bit keys.
    Pair(i32, i32),
    /// A 32-bit namespace paired with the database's 32-bit hash of the text.
    HashedPair { namespace: i32, text: String },
    /// The database's hash of the text, widened to a 64-bit key.
    Hashed(String),
    /// The database's hash of the lowercased text, widened to a 64-bit key.
    HashedLowercase(String),
}

impl AdvisoryKey {
    #[must_use]
    pub const fn single(key: i64) -> Self {
        Self::Single(key)
    }

    #[must_use]
    pub const fn pair(high: i32, low: i32) -> Self {
        Self::Pair(high, low)
    }

    pub fn hashed_pair(namespace: i32, text: impl Into<String>) -> Self {
        Self::HashedPair {
            namespace,
            text: text.into(),
        }
    }

    pub fn hashed(text: impl Into<String>) -> Self {
        Self::Hashed(text.into())
    }

    pub fn hashed_lowercase(text: impl Into<String>) -> Self {
        Self::HashedLowercase(text.into())
    }

    fn text(&self) -> Option<&str> {
        match self {
            Self::Single(_) | Self::Pair(..) => None,
            Self::HashedPair { text, .. } | Self::Hashed(text) | Self::HashedLowercase(text) => {
                Some(text)
            }
        }
    }

    /// Bound parameters used by this key form.
    #[must_use]
    pub fn bind_parameters(&self) -> usize {
        match self {
            Self::Single(_) | Self::Hashed(_) | Self::HashedLowercase(_) => 1,
            Self::Pair(..) | Self::HashedPair { .. } => 2,
        }
    }
}

impl fmt::Debug for AdvisoryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Single(_) => f.write_str("Single"),
            Self::Pair(..) => f.write_str("Pair"),
            Self::HashedPair { .. } => f.write_str("HashedPair"),
            Self::Hashed(_) => f.write_str("Hashed"),
            Self::HashedLowercase(_) => f.write_str("HashedLowercase"),
        }
    }
}

/// How long an advisory lock is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdvisoryLockScope {
    /// Released when the enclosing transaction commits or rolls back.
    Transaction,
    /// Held by the database session until released or the session ends.
    Session,
}

/// What an advisory lock statement does with its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdvisoryLockAction {
    /// Wait for the lock. The statement returns no value.
    Wait,
    /// Try once without waiting and return [`ADVISORY_ACQUIRED`].
    Try,
    /// Release one session-level hold and return [`ADVISORY_RELEASED`].
    Release,
}

/// A validated advisory lock statement.
#[derive(Debug)]
pub struct AdvisoryLock {
    key: AdvisoryKey,
    scope: AdvisoryLockScope,
    action: AdvisoryLockAction,
}

impl AdvisoryLock {
    pub fn new(
        key: AdvisoryKey,
        scope: AdvisoryLockScope,
        action: AdvisoryLockAction,
    ) -> Result<Self, CompileError> {
        let lock = Self { key, scope, action };
        lock.validate()?;
        Ok(lock)
    }

    pub fn validate(&self) -> Result<(), CompileError> {
        if self.scope == AdvisoryLockScope::Transaction && self.action == AdvisoryLockAction::Release
        {
            return Err(invalid(
                "transaction advisory locks are released when the transaction settles",
            ));
        }
        if self.key.text().is_some_and(|text| text.contains('\0')) {
            return Err(invalid("advisory key text cannot contain a NUL byte"));
        }
        Ok(())
    }

    #[must_use]
    pub fn key(&self) -> &AdvisoryKey {
        &self.key
    }

    #[must_use]
    pub fn scope(&self) -> AdvisoryLockScope {
        self.scope
    }

    #[must_use]
    pub fn action(&self) -> AdvisoryLockAction {
        self.action
    }
}

/// A custom setting name: two lowercase identifiers joined by one dot.
///
/// Every built-in PostgreSQL setting is dotless, including the role and
/// resource limits the ORM applies to its sessions, so a valid name can
/// never override them. Which namespaces a connection may set is the host's
/// declaration, checked where the setting is applied.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SettingName {
    name: String,
    dot: usize,
}

impl SettingName {
    pub fn new(name: &str) -> Result<Self, CompileError> {
        let (namespace, setting) = name
            .split_once('.')
            .ok_or_else(|| invalid("a transaction setting name must be namespace.name"))?;
        if !valid_setting_segment(namespace) || !valid_setting_segment(setting) {
            return Err(invalid(
                "a transaction setting name must be two lowercase identifiers joined by a dot",
            ));
        }
        Ok(Self {
            name: name.to_owned(),
            dot: namespace.len(),
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.name[..self.dot]
    }
}

/// Whether `namespace` is a lowercase identifier usable as a setting namespace.
#[must_use]
pub fn valid_setting_namespace(namespace: &str) -> bool {
    valid_setting_segment(namespace)
}

fn valid_setting_segment(segment: &str) -> bool {
    let mut bytes = segment.bytes();
    segment.len() <= MAX_SETTING_SEGMENT_BYTES
        && bytes
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Set a custom setting until the enclosing transaction or savepoint ends.
/// Debug output omits the value.
pub struct SetTransactionSetting {
    name: SettingName,
    value: String,
}

impl SetTransactionSetting {
    pub fn new(name: SettingName, value: impl Into<String>) -> Result<Self, CompileError> {
        let setting = Self {
            name,
            value: value.into(),
        };
        setting.validate()?;
        Ok(setting)
    }

    pub fn validate(&self) -> Result<(), CompileError> {
        if self.value.contains('\0') {
            return Err(invalid("a transaction setting value cannot contain a NUL byte"));
        }
        Ok(())
    }

    #[must_use]
    pub fn name(&self) -> &SettingName {
        &self.name
    }

    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn into_parts(self) -> (SettingName, String) {
        (self.name, self.value)
    }
}

impl fmt::Debug for SetTransactionSetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SetTransactionSetting")
            .field("name", &self.name.as_str())
            .finish_non_exhaustive()
    }
}

fn invalid(message: &'static str) -> CompileError {
    CompileError::InvalidStatement(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_names_are_dotted_lowercase_identifiers() {
        for name in ["test_ns.flag", "_ns.flag_2", "a.b"] {
            let parsed = SettingName::new(name).unwrap();
            assert_eq!(parsed.as_str(), name);
        }
        assert_eq!(SettingName::new("test_ns.flag").unwrap().namespace(), "test_ns");
        for name in [
            "statement_timeout",
            "role",
            "search_path",
            "lock_timeout",
            "idle_in_transaction_session_timeout",
            "Test_ns.flag",
            "test_ns.flag;x",
            "test_ns.",
            ".flag",
            "test_ns.flag.extra",
            "1ns.flag",
            "test-ns.flag",
            "test_ns.fl ag",
            "",
        ] {
            assert!(SettingName::new(name).is_err(), "{name}");
        }
        let long = "a".repeat(MAX_SETTING_SEGMENT_BYTES);
        assert!(SettingName::new(&format!("{long}.flag")).is_ok());
        assert!(SettingName::new(&format!("{long}a.flag")).is_err());
        assert!(valid_setting_namespace("test_ns"));
        for namespace in ["", "Test", "a.b", "a-b", "9a"] {
            assert!(!valid_setting_namespace(namespace), "{namespace}");
        }
    }

    #[test]
    fn advisory_statements_refuse_nul_text_and_transaction_release() {
        let transaction = AdvisoryLockScope::Transaction;
        let session = AdvisoryLockScope::Session;
        for key in [
            AdvisoryKey::hashed("a\0b"),
            AdvisoryKey::hashed_lowercase("\0"),
            AdvisoryKey::hashed_pair(1, "x\0"),
        ] {
            assert!(AdvisoryLock::new(key, transaction, AdvisoryLockAction::Wait).is_err());
        }
        assert!(AdvisoryLock::new(
            AdvisoryKey::single(1),
            transaction,
            AdvisoryLockAction::Release
        )
        .is_err());
        // Controls: the same forms without NUL, and session release, are accepted.
        for key in [
            AdvisoryKey::hashed("a b"),
            AdvisoryKey::hashed_lowercase("MiXeD"),
            AdvisoryKey::hashed_pair(1, "x"),
        ] {
            assert!(AdvisoryLock::new(key, transaction, AdvisoryLockAction::Wait).is_ok());
        }
        assert!(
            AdvisoryLock::new(AdvisoryKey::single(1), session, AdvisoryLockAction::Release).is_ok()
        );
        assert!(SetTransactionSetting::new(SettingName::new("a.b").unwrap(), "x\0").is_err());
        assert!(SetTransactionSetting::new(SettingName::new("a.b").unwrap(), "x';\\").is_ok());
    }

    #[test]
    fn debug_output_omits_key_text_and_setting_values() {
        let key = format!("{:?}", AdvisoryKey::hashed_lowercase("person@example.test"));
        assert_eq!(key, "HashedLowercase");
        let setting =
            SetTransactionSetting::new(SettingName::new("a.b").unwrap(), "secret-value").unwrap();
        let rendered = format!("{setting:?}");
        assert!(rendered.contains("a.b") && !rendered.contains("secret-value"), "{rendered}");
    }
}
