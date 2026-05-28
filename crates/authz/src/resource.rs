use serde::{Deserialize, Serialize};

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
            Self::App { id } => format!("App::\"{id}\""),
            Self::Org { id } => format!("Org::\"{id}\""),
            Self::Any => "*".to_owned(),
        }
    }
}
