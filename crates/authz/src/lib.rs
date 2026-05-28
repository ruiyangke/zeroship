//! Authorization wrapper policy types for the zeroship platform.

pub mod action;
pub mod condition;
pub mod effect;
pub mod engine;
pub mod entities;
pub mod eval;
pub mod error;
pub mod lower;
pub mod policy;
pub mod resource;
pub mod scope;
pub mod statement;

pub use action::Action;
pub use cedar_policy::PolicySet;
pub use condition::Condition;
pub use effect::Effect;
pub use engine::{load_platform_policies, policy_hash, Authorizer};
pub use entities::{assemble_entities, EntityCache};
pub use eval::{enforce, is_authorized_anywhere, AuthzContext, AuthzDecision};
pub use error::{AuthzError, ValidationError};
pub use lower::lower;
pub use policy::Policy;
pub use resource::Resource;
pub use scope::{parse_scope_string, scopes_to_policy, ParseScopeError, Scope};
pub use statement::Statement;
