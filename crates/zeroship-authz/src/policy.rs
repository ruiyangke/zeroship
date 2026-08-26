use serde::{Deserialize, Serialize};

use crate::{Statement, ValidationError};

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Policy {
    pub name: String,
    pub statements: Vec<Statement>,
}

impl Policy {
    /// Builds a policy from the wrapper JSON representation.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] when the JSON value does not match the
    /// wrapper policy shape.
    pub fn from_json_value(v: &serde_json::Value) -> Result<Self, ValidationError> {
        serde_json::from_value(v.clone())
            .map_err(|err| ValidationError::PolicyJsonShape(err.to_string()))
    }

    /// Converts this policy to the wrapper JSON representation.
    ///
    /// # Panics
    ///
    /// Panics only if serde cannot serialize the in-memory [`Policy`] value.
    /// The current policy fields are all JSON-serializable.
    #[must_use]
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("Policy serialization is infallible")
    }
}
