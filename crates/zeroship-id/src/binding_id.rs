//! [`BindingId`] names one edge joining an app to a database with one
//! capability.
//!
//! # Why the edge has an identity of its own
//!
//! The `PostgreSQL` role the data plane narrows to is derived from it:
//! `zs_bind_<binding>_e<epoch>`. A composite natural key would put two ids in
//! one identifier, and `max_identifier_length` truncates silently past 63 with
//! the epoch at the END of the name - so two epochs would collapse onto one
//! role rather than error.
//!
//! # Why `bnd` and not `grt`
//!
//! [`crate::typed_id::GRANT_PREFIX`] already means one row per (person,
//! audience) in `zeroship.grants`. Putting a data-access edge in that namespace
//! is exactly the collision the disjointness rule exists to prevent: a
//! mis-typed id must be unresolvable, never resolvable against the wrong table.

use crate::entity_id::declare_entity_id;
use crate::typed_id::BINDING_PREFIX;

declare_entity_id! {
    /// The typed id of one (app, database, capability) edge: `bnd_<base36(uuidv7)>`.
    BindingId,
    BINDING_PREFIX,
    binding_id_tests,
}
