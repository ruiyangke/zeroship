//! [`DeployCommandId`] - the identity of one normal deploy command.
//!
//! # The command id is not the artifact
//!
//! A client mints this id once per logical deploy and sends it as the deploy
//! request's `Idempotency-Key`. Retrying the same request reuses it, so the
//! control plane can return the first acceptance instead of accepting twice. A
//! new deploy mints a new id even when it uploads the same bytes: redeploying an
//! earlier artifact is an intentional rollback, and deriving the identity from
//! the artifact hash could not tell that apart from a late retry.
//!
//! Like the other row ids in this crate it seeds no physical name and exposes no
//! route to the embedded bits.

use crate::entity_id::declare_entity_id;
use crate::typed_id::DEPLOY_COMMAND_PREFIX;

declare_entity_id! {
    /// The typed id of one deploy command: `dcm_<base36(uuidv7)>`.
    DeployCommandId,
    DEPLOY_COMMAND_PREFIX,
    deploy_command_id_tests,
}
