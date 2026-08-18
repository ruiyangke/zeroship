use serde::{Deserialize, Serialize};

use crate::entities::cedar_string;

/// What an authorization request is ABOUT.
///
/// There is no `Org` variant. It existed for exactly one caller - the operator
/// gate, which probed `team:write` on a synthetic `Org::"zeroship_platform"`
/// that no migration ever inserted - and only the deleted universal-allow
/// policy could satisfy it. Reintroducing an org resource later means
/// introducing real orgs, with rows and membership, rather than a sentinel id.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Resource {
    App { id: String },
    Any,
}

impl Resource {
    #[must_use]
    pub fn cedar_uid(&self) -> String {
        match self {
            Self::App { id } => format!("App::{}", cedar_string(id)),
            Self::Any => "*".to_owned(),
        }
    }

    /// Validate resource IDs before they reach Cedar source lowering.
    ///
    /// An app id is a platform identifier, not an arbitrary Cedar string.
    /// Keeping the alphabet closed prevents source-level ambiguity in wrapper
    /// policies and fails bad HTTP path/input values before authorization.
    pub fn validate_ids(&self) -> Result<(), &'static str> {
        match self {
            Self::App { id } if !is_valid_resource_id(id) => {
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
