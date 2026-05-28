use std::fmt::{Display, Formatter};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthzError {
    CedarParse(String),
    CedarValidation(String),
    PolicyJsonShape(String),
    Validation(String),
}

pub type ValidationError = AuthzError;

impl Display for AuthzError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CedarParse(message) => write!(f, "Cedar parse error: {message}"),
            Self::CedarValidation(message) => write!(f, "Cedar validation error: {message}"),
            Self::PolicyJsonShape(message) => write!(f, "policy JSON shape error: {message}"),
            Self::Validation(message) => write!(f, "validation error: {message}"),
        }
    }
}

impl std::error::Error for AuthzError {}
