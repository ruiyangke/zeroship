//! [`UserId`] - the platform identity of one user.
//!
//! A user id is always the canonical `usr_<base62(uuidv7)>` value. It is the
//! value stored in `zeroship.users.id` and carried across service boundaries.
//! The type exposes no UUID conversion because UUID is not a storage or wire
//! representation of a user id.

use crate::entity_id::declare_entity_id;
use crate::typed_id::USER_PREFIX;

declare_entity_id! {
    /// The typed id of one platform user: `usr_<base62(uuidv7)>`.
    UserId,
    USER_PREFIX,
    user_id_tests,
}
