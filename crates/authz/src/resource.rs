use serde::{Deserialize, Serialize};

use crate::entities::cedar_string;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Resource {
    App { id: String },
    Org { id: String },
    Any,
}

impl Resource {
    #[must_use]
    pub fn cedar_uid(&self) -> String {
        match self {
            Self::App { id } => format!("App::{}", cedar_string(id)),
            Self::Org { id } => format!("Org::{}", cedar_string(id)),
            Self::Any => "*".to_owned(),
        }
    }

    /// Validate resource IDs before they reach Cedar source lowering.
    ///
    /// App and org ids are platform identifiers, not arbitrary Cedar strings.
    /// Keeping the alphabet closed prevents source-level ambiguity in wrapper
    /// policies and fails bad HTTP path/input values before authorization.
    pub fn validate_ids(&self) -> Result<(), &'static str> {
        match self {
            Self::App { id } | Self::Org { id } if !is_valid_resource_id(id) => {
                Err("resource id must use only ASCII letters, digits, '_' or '-'")
            }
            _ => Ok(()),
        }
    }
}

#[must_use]
pub fn is_valid_resource_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}
