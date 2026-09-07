use std::fmt::{Display, Formatter};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthzError {
    CedarParse(String),
    CedarValidation(String),
    CedarEntities(String),
    CedarRequest(String),
    /// Cedar completed the authorization but recorded a per-policy EVALUATION
    /// error - a dereference of a missing attribute, an extension function on
    /// the wrong type. The affected policy is SKIPPED, so the response is a
    /// deny whose `matched_policy_ids` is empty: indistinguishable, in the one
    /// durable record of the decision, from a principal who simply held no
    /// authority. It is surfaced as an error rather than returned as that deny.
    CedarEval(String),
    Db(String),
    PolicyJsonShape(String),
    Validation(String),
}

pub type ValidationError = AuthzError;

impl Display for AuthzError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CedarParse(message) => write!(f, "Cedar parse error: {message}"),
            Self::CedarValidation(message) => write!(f, "Cedar validation error: {message}"),
            Self::CedarEntities(message) => write!(f, "Cedar entities error: {message}"),
            Self::CedarRequest(message) => write!(f, "Cedar request error: {message}"),
            Self::CedarEval(message) => write!(f, "Cedar evaluation error: {message}"),
            Self::Db(message) => write!(f, "database error: {message}"),
            Self::PolicyJsonShape(message) => write!(f, "policy JSON shape error: {message}"),
            Self::Validation(message) => write!(f, "validation error: {message}"),
        }
    }
}

impl std::error::Error for AuthzError {}
