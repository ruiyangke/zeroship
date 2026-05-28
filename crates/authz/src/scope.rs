use std::fmt;

use crate::Action;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Scope(Action);

impl Scope {
    /// Parse an OAuth scope token into the closed zeroship scope vocabulary.
    ///
    /// OAuth request bodies carry scopes as strings, but the control plane
    /// authorizes with `Action`; the mapping is intentionally 1:1.
    pub fn parse(value: &str) -> Result<Self, ScopeParseError> {
        Action::from_wire(value)
            .map(Self)
            .ok_or_else(|| ScopeParseError {
                value: value.to_owned(),
            })
    }

    #[must_use]
    pub const fn action(self) -> Action {
        self.0
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0.cedar_id()
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ScopeParseError {
    value: String,
}

impl ScopeParseError {
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for ScopeParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown OAuth scope: {}", self.value)
    }
}

impl std::error::Error for ScopeParseError {}
