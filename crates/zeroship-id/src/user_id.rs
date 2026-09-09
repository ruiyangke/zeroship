//! [`UserId`] - one human's platform identity, as a typed id.
//!
//! # What a user id is, and what it is not
//!
//! A `UserId` names a row in `zeroship.users`: a person who signs in to the
//! creator platform, is a member of an organization, and owns or acts on apps.
//! It is the actor in every audit row and the subject of every creator-facing
//! session.
//!
//! It is NOT an end user of a creator app. Those subjects are scoped to a
//! [`crate::project_id::ProjectId`] under the auth foundation's audience sum,
//! are minted by the auth process, and never appear in a control-plane column.
//! Keeping them separate types is what stops a creator's identity being joined
//! against an app's audience by accident.
//!
//! # It seeds no physical name
//!
//! [`crate::app_id::AppId`] has a whole module of derivations hanging off it - a
//! schema, two roles, a publication, a salt. A user id keys rows and scopes a
//! session; nothing in the data plane is named after it. Like every
//! macro-declared id it exposes no route to the embedded bits, which is the
//! macro's rule rather than a property of this type.

use crate::entity_id::declare_entity_id;
use crate::typed_id::USER_PREFIX;

declare_entity_id! {
    /// The typed id of one platform user: `usr_<base36(uuidv7)>`.
    UserId,
    USER_PREFIX,
    user_id_tests,
}
