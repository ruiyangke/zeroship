//! [`DatastoreId`] names one `PostgreSQL` cluster the platform places creator
//! databases on.
//!
//! It is operator-owned and never creator-visible: no API accepts one, no
//! manifest carries one, and `zeroship-worker`'s env map must never hold one,
//! because two apps under one actor that read equal datastore handles have
//! confirmed co-residency.
//!
//! The `zeroship.datastores` row is keyed on the cluster's own
//! `pg_control_system().system_identifier`, not on this id. That is what makes
//! registration idempotent - two services configured against one cluster
//! converge on one row - while this id stays the stable handle every other
//! table references.

use crate::entity_id::declare_entity_id;
use crate::typed_id::DATASTORE_PREFIX;

declare_entity_id! {
    /// The typed id of one operator-owned cluster: `dst_<base36(uuidv7)>`.
    DatastoreId,
    DATASTORE_PREFIX,
    datastore_id_tests,
}
