//! Logical key namespaces issued by trusted hosts.

use crate::KvError;

/// A validated namespace. App and platform names occupy disjoint keyspaces.
///
/// Namespace selection is a host responsibility, not an authorization check;
/// private platform stores need credentials unavailable to creator workers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Namespace(String);

impl Namespace {
    /// Bind a trusted app identity. The host authenticates the identity; this
    /// constructor checks that it cannot change the storage key grammar.
    ///
    /// # Errors
    /// Returns `InvalidArgument` for empty names or characters outside the
    /// supported ASCII letters, digits, underscore, hyphen, and period.
    pub fn app(app_id: &str) -> Result<Self, KvError> {
        validate_component(app_id)?;
        Ok(Self(app_id.to_owned()))
    }

    /// Bind an internal platform service or subsystem.
    ///
    /// # Errors
    /// Returns `InvalidArgument` for a name outside the same grammar as [`Self::app`].
    pub fn platform(name: &str) -> Result<Self, KvError> {
        validate_component(name)?;
        Ok(Self(format!("platform:{name}")))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate_component(name: &str) -> Result<(), KvError> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(KvError::invalid_argument(
            "kv: namespace must contain only ASCII letters, digits, '_', '-', or '.'",
        ));
    }
    Ok(())
}
