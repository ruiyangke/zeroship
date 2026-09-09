//! [`InviteId`] - the addressable name of one pending organization invite.
//!
//! # This id is not the secret, and that separation is the whole design
//!
//! An invite has two values and they must never be confused. This one names the
//! ROW: it appears in the revoke path, in the audit trail, and in any listing an
//! admin reads. The other is a high-entropy token the recipient presents, which
//! is stored only as a digest (`organization_invites.token_hash`) and is handed
//! out exactly once, at issue.
//!
//! Because the id is not the secret it is safe to print, log and list. Because
//! the secret is not the id, an admin reading the invite list learns nothing
//! that would let them redeem an invite on someone else's behalf, and a leaked
//! audit row is not a leaked invitation.
//!
//! Like [`crate::organization_id::OrganizationId`] and
//! [`crate::project_id::ProjectId`], this id keys a row and seeds no derivation,
//! so it holds the printed text alone and exposes no route to the embedded bits.

use crate::entity_id::declare_entity_id;
use crate::typed_id::INVITE_PREFIX;

declare_entity_id! {
    /// The typed id of one organization invite: `ivt_<base36(uuidv7)>`.
    InviteId,
    INVITE_PREFIX,
    invite_id_tests,
}
