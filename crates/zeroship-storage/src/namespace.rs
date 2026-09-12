//! Namespaces chosen by the trusted host.

use crate::StorageError;

/// A validated storage scope. App and platform constructors produce disjoint
/// names. Choosing the identity is the host's responsibility; this type
/// validates the storage path grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Namespace(String);

impl Namespace {
    /// Bind an authenticated app identity.
    ///
    /// # Errors
    /// Rejects empty identities, path components and reserved delimiters.
    pub fn app(app_id: &str) -> Result<Self, StorageError> {
        validate_name(app_id)?;
        Ok(Self(app_id.to_owned()))
    }

    /// Bind an internal platform subsystem. Sensitive platform objects should
    /// use a store whose credentials are unavailable to creator workers.
    ///
    /// # Errors
    /// Rejects names outside the same grammar as [`Self::app`].
    pub fn platform(name: &str) -> Result<Self, StorageError> {
        validate_name(name)?;
        Ok(Self(format!("platform:{name}")))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate_name(name: &str) -> Result<(), StorageError> {
    if name.is_empty()
        || name == "."
        || name.contains("..")
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(StorageError::InvalidArgument(
            "storage: namespace must be a non-empty name without path components or reserved delimiters".into(),
        ));
    }
    Ok(())
}
