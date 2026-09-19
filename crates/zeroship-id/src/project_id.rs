//! [`ProjectId`] - the shared-infrastructure boundary and the end-user identity
//! domain, as a typed id.
//!
//! # What a project is
//!
//! A project is one product: the apps that make it up share data and share a
//! view of who their users are. It sits between the organization (who owns and
//! pays) and the app (what deploys and routes).
//!
//! Two independent arguments put the same boundary here, which is why it is an
//! entity rather than a label:
//!
//! - **Identity.** Under the auth foundation's audience sum a subject is scoped
//!   to a project, so one human is one subject across every app of a product.
//!   With app-scoped subjects there is no unit between "one app" and "the
//!   platform", and cross-app teardown has no object.
//! - **Data.** Creator apps scope rows by the id `env.auth.getUser()` returns.
//!   If two apps of one product saw different subjects for the same human, a
//!   database they share would be useless for exactly the case that motivates
//!   sharing it.
//!
//! Like every macro-declared id it exposes no route to the embedded bits, and
//! like [`crate::organization_id::OrganizationId`] it seeds no physical name: a
//! project id keys rows and scopes a subject derivation that lives in the auth
//! process.

use crate::entity_id::declare_entity_id;
use crate::typed_id::PROJECT_PREFIX;

declare_entity_id! {
    /// The typed id of one project: `prj_<base36(uuidv7)>`.
    ProjectId,
    PROJECT_PREFIX,
    project_id_tests,
}
