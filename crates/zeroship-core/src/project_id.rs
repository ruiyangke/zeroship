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
//! # The prefix collision, and why it is not a foreign key
//!
//! `zeroship.sandboxes.project_id` carries a `^prj_[0-9A-Za-z]{20,40}$` CHECK.
//! That column is NOT this entity: it holds a derived dedup key minted by the
//! extracted `zeroship-sandbox` controller - usually an app id re-tagged into
//! the `prj_` namespace - it has no foreign key to anything, and its only use is
//! a partial unique index over live sandboxes. It is being retired so this
//! prefix carries one meaning per schema. Until that lands, the two must not be
//! joined, and this crate deliberately offers no conversion between them.
//!
//! Like [`crate::organization_id::OrganizationId`] and unlike
//! [`crate::app_id::AppId`], this type exposes no route to the embedded bits: a
//! project id keys rows and scopes a subject derivation that lives in the auth
//! process, and nothing derives a physical name from it.

use crate::entity_id::declare_entity_id;
use crate::typed_id::PROJECT_PREFIX;

declare_entity_id! {
    /// The typed id of one project: `prj_<base62(uuidv7)>`.
    ProjectId,
    PROJECT_PREFIX,
    project_id_tests,
}
