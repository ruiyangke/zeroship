//! Physical schema identity shared by migration services and data execution.

/// A validated physical schema name. SQL consumers own identifier quoting.
///
/// ```compile_fail
/// let schema = zeroship_core::schema_name::SchemaName("unchecked".into());
/// ```
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SchemaName(String);

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SchemaNameError {
    #[error("schema name cannot be empty")]
    Empty,
    #[error("invalid schema name: {0}")]
    Invalid(String),
}

impl SchemaName {
    /// Accept an identifier containing ASCII letters, digits, underscores or hyphens.
    pub fn new(name: &str) -> Result<Self, SchemaNameError> {
        if name.is_empty() {
            return Err(SchemaNameError::Empty);
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(SchemaNameError::Invalid(name.to_owned()));
        }
        Ok(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}
