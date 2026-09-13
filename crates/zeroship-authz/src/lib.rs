//! Authorization wrapper policy types for the zeroship platform.
//!
//! Authority is two integers on a closed ladder
//! (`zeroship.organization_roles`), resolved from the database on EVERY
//! request, narrowed per project, and carried into Cedar as request context.
//! Nothing is cached: see [`authority`] for why the cache that used to sit here
//! was deleted rather than fixed.

pub mod action;
pub mod authority;
pub mod condition;
pub mod effect;
pub mod engine;
pub mod entities;
pub mod error;
pub mod eval;
pub mod lower;
pub mod policy;
pub mod resource;
pub mod scope;
pub mod statement;
pub mod wrapper_revocation;

pub use action::Action;
pub use authority::{effective_project_rank, Authority};
pub use cedar_policy::PolicySet;
pub use condition::Condition;
pub use effect::Effect;
pub use engine::{load_platform_policies, policy_hash, PlatformPolicies};
pub use entities::assemble_entities;
pub use error::{AuthzError, ValidationError};
pub use eval::{enforce, is_authorized_anywhere, AuthzContext, AuthzDecision};
pub use lower::lower;
pub use policy::Policy;
pub use resource::Resource;
pub use scope::{parse_scope_string, scopes_to_policy, ParseScopeError, Scope};
pub use statement::Statement;

#[cfg(test)]
mod tests;
