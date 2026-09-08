//! [`OrganizationId`] - the ownership root and the billing subject, as a typed
//! id.
//!
//! # What an organization id is, and what it is not
//!
//! An organization is the entity that OWNS and the entity that PAYS: apps
//! descend from it, the subscription hangs off it, Stripe Connect settles to it,
//! and membership is scoped to it. It is a company, not a product.
//!
//! It is deliberately NOT an audience. Under the auth foundation's closed
//! audience sum an end user authenticates to a `Project`, never to an
//! organization: a company may run unrelated products, and correlating a
//! customer across them is a privacy leak rather than a feature. So this id
//! seeds no subject derivation, appears in no token `aud`, and is never handed
//! to app code.
//!
//! It also seeds no physical name. [`crate::app_id::AppId`] is the id with a
//! whole module of derivations hanging off it - a schema, two roles, a
//! publication, a salt; an organization id keys rows and nothing else. Neither
//! exposes a route to the embedded bits, and that is the macro's rule rather
//! than a property of this one type.
//!
//! # Spelling
//!
//! The prefix is the abbreviation `org` while every column name is spelled out
//! in full - `organization_id`, never `org_id`. That asymmetry is deliberate and
//! was decided rather than inherited: a column name is prose a reader meets
//! constantly, and an abbreviation there is a small tax paid forever; a prefix
//! is a fixed-width tag inside an opaque value nobody reads as a word.

use crate::entity_id::declare_entity_id;
use crate::typed_id::ORGANIZATION_PREFIX;

declare_entity_id! {
    /// The typed id of one organization: `org_<base62(uuidv7)>`.
    OrganizationId,
    ORGANIZATION_PREFIX,
    organization_id_tests,
}
